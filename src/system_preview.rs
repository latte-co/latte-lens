//! Bounded classification and shell-free handoff to the host's default app.

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use zip::ZipArchive;

use crate::content_safety::{FileFingerprint, OpenRegular, SafeFile, open_regular};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
    io,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const OPEN_PROBE_BYTES: usize = 8 * 1024;
const MAX_OOXML_ENTRIES: usize = 4_096;
const MAX_CONTENT_TYPES_BYTES: u64 = 1024 * 1024;

#[cfg(any(target_os = "linux", target_os = "macos"))]
// macOS can spend a few hundred milliseconds starting even a tiny executable
// under load. Keep this on the I/O worker and wait long enough to distinguish
// an immediate handoff failure from a genuinely long-lived viewer process.
const QUICK_FAILURE_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExternalOpenOutcome {
    Opened,
    ConfirmationRequired {
        fingerprint: FileFingerprint,
        detected: String,
    },
    Unavailable {
        reason: String,
        image_fallback: bool,
    },
    Failed {
        reason: String,
        image_fallback: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum SystemOpenAdapter {
    Host,
    Disabled(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Eligibility {
    Direct,
    ConfirmationRequired,
}

#[derive(Debug)]
struct Classification {
    launch_path: PathBuf,
    fingerprint: FileFingerprint,
    detected: String,
    image_fallback: bool,
    eligibility: Eligibility,
}

/// Classify, revalidate, and explicitly hand one regular file to the host.
///
/// `confirmed` is accepted only for the exact unknown-file fingerprint emitted
/// by an earlier [`ExternalOpenOutcome::ConfirmationRequired`]. Active files,
/// mismatches, links rejected by the caller's path policy, and replacements
/// never gain a confirmation path.
pub(crate) fn open_file(
    path: &Path,
    content_root: Option<&Path>,
    follow_symlinks: bool,
    confirmed: Option<&FileFingerprint>,
    adapter: &SystemOpenAdapter,
) -> Result<ExternalOpenOutcome> {
    debug_assert!(path.is_absolute());
    let first = classify_file(path, content_root, follow_symlinks)?;
    if first.eligibility == Eligibility::ConfirmationRequired {
        let Some(confirmed) = confirmed else {
            return Ok(ExternalOpenOutcome::ConfirmationRequired {
                fingerprint: first.fingerprint,
                detected: first.detected,
            });
        };
        if confirmed != &first.fingerprint {
            bail!("the file changed after external-open confirmation");
        }
    }

    // Repeat the complete bounded classification immediately before the
    // platform adapter. The receiving application still reopens a path, so
    // this narrows common replacement races without claiming atomic handoff.
    let second = classify_file(path, content_root, follow_symlinks)?;
    if first.fingerprint != second.fingerprint
        || first.launch_path != second.launch_path
        || first.detected != second.detected
        || first.eligibility != second.eligibility
    {
        bail!("the file changed while it was being opened");
    }
    if second.eligibility == Eligibility::ConfirmationRequired
        && confirmed != Some(&second.fingerprint)
    {
        return Ok(ExternalOpenOutcome::ConfirmationRequired {
            fingerprint: second.fingerprint,
            detected: second.detected,
        });
    }

    let platform_outcome = match adapter {
        SystemOpenAdapter::Host => open_file_platform(&second.launch_path),
        SystemOpenAdapter::Disabled(reason) => PlatformOpenOutcome::Unavailable(reason.clone()),
    };
    Ok(match platform_outcome {
        PlatformOpenOutcome::Opened => ExternalOpenOutcome::Opened,
        PlatformOpenOutcome::Unavailable(reason) => ExternalOpenOutcome::Unavailable {
            reason,
            image_fallback: second.image_fallback,
        },
        PlatformOpenOutcome::Failed(reason) => ExternalOpenOutcome::Failed {
            reason,
            image_fallback: second.image_fallback,
        },
    })
}

pub(crate) fn terminal_truecolor_supported() -> bool {
    !std::env::var_os("TERM").is_some_and(|term| term == "dumb")
}

fn classify_file(
    path: &Path,
    content_root: Option<&Path>,
    follow_symlinks: bool,
) -> Result<Classification> {
    let launch_path = if follow_symlinks {
        path.canonicalize()
            .with_context(|| format!("cannot resolve {} for the system app", path.display()))?
    } else {
        path.to_path_buf()
    };
    let opened = if follow_symlinks {
        open_regular(None, &launch_path)?
    } else {
        open_regular(content_root, path)?
    };
    let mut file = match opened {
        OpenRegular::Opened(file) => file,
        OpenRegular::Declined(inspected) => {
            bail!(
                "system opening is allowed only for a regular file; the target is a {}",
                inspected.kind.label()
            )
        }
    };
    let fingerprint = file.fingerprint()?;
    let extension = lowercase_extension(&launch_path);
    if let Some(reason) = active_extension(extension.as_deref()) {
        bail!("system opening was blocked because {reason}");
    }
    if file.is_executable() {
        bail!("system opening was blocked because the file has executable permission bits");
    }

    let probe_len = usize::try_from(file.len())
        .unwrap_or(usize::MAX)
        .min(OPEN_PROBE_BYTES);
    let mut probe = vec![0; probe_len];
    let read = file.read(&mut probe).with_context(|| {
        format!(
            "cannot inspect {} for system opening",
            launch_path.display()
        )
    })?;
    probe.truncate(read);
    if let Some(active) = active_signature(&probe) {
        bail!("system opening was blocked because the content is {active}");
    }

    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("cannot rewind {} after inspection", launch_path.display()))?;
    let detected = detect_passive_type(&probe, file, extension.as_deref())?;
    let (detected_label, image_fallback, eligibility) = match detected {
        Some(detected)
            if extension
                .as_deref()
                .is_none_or(|ext| detected.matches_extension(ext)) =>
        {
            (
                detected.label().to_owned(),
                detected.is_image(),
                Eligibility::Direct,
            )
        }
        Some(detected) if extension.as_deref().is_some_and(is_claimed_binary_format) => {
            bail!(
                "the extension claims {}, but the content is {}; system opening was blocked",
                extension.as_deref().unwrap_or_default(),
                detected.label()
            )
        }
        Some(detected) => (
            detected.label().to_owned(),
            detected.is_image(),
            Eligibility::ConfirmationRequired,
        ),
        None if extension.as_deref().is_some_and(is_claimed_binary_format) => {
            bail!(
                "the extension claims {}, but the content does not match; system opening was blocked",
                extension.as_deref().unwrap_or_default()
            )
        }
        None if is_bounded_text(&probe) => ("text".to_owned(), false, Eligibility::Direct),
        None => (
            "unknown regular file".to_owned(),
            false,
            Eligibility::ConfirmationRequired,
        ),
    };

    Ok(Classification {
        launch_path,
        fingerprint,
        detected: detected_label,
        image_fallback,
        eligibility,
    })
}

fn is_bounded_text(probe: &[u8]) -> bool {
    !probe.contains(&0) && std::str::from_utf8(probe).is_ok()
}

fn lowercase_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

fn active_extension(extension: Option<&str>) -> Option<&'static str> {
    let extension = extension?;
    let category = if matches!(
        extension,
        "exe" | "com" | "msi" | "msix" | "dll" | "elf" | "dylib" | "so" | "appimage"
    ) {
        "an executable or installer"
    } else if matches!(
        extension,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "command"
            | "bat"
            | "cmd"
            | "ps1"
            | "vbs"
            | "vbe"
            | "js"
            | "jse"
            | "wsf"
            | "wsh"
            | "hta"
            | "py"
            | "rb"
            | "pl"
    ) {
        "a script"
    } else if matches!(
        extension,
        "desktop" | "app" | "lnk" | "url" | "scf" | "workflow"
    ) {
        "an application launcher or shortcut"
    } else if matches!(
        extension,
        "docm" | "dotm" | "xlsm" | "xltm" | "xlam" | "pptm" | "potm" | "ppam" | "sldm"
    ) {
        "a macro-capable Office document"
    } else {
        return None;
    };
    Some(category)
}

fn active_signature(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"#!") {
        return Some("a shebang script");
    }
    if bytes.starts_with(b"\x7fELF") {
        return Some("an ELF executable");
    }
    if bytes.starts_with(b"MZ") {
        return Some("a PE/DOS executable");
    }
    if matches!(
        bytes.get(..4),
        Some(
            b"\xfe\xed\xfa\xce"
                | b"\xce\xfa\xed\xfe"
                | b"\xfe\xed\xfa\xcf"
                | b"\xcf\xfa\xed\xfe"
                | b"\xca\xfe\xba\xbe"
                | b"\xbe\xba\xfe\xca"
        )
    ) {
        return Some("a Mach-O executable");
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PassiveType {
    Png,
    Jpeg,
    Gif,
    WebP,
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Zip,
    Gzip,
    SevenZip,
    Rar,
    Tar,
    Mp3,
    Flac,
    Ogg,
    Wave,
    Avi,
    Mp4,
    WebM,
}

impl PassiveType {
    const fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG image",
            Self::Jpeg => "JPEG image",
            Self::Gif => "GIF image",
            Self::WebP => "WebP image",
            Self::Pdf => "PDF document",
            Self::Docx => "DOCX document",
            Self::Xlsx => "XLSX workbook",
            Self::Pptx => "PPTX presentation",
            Self::Zip => "ZIP archive",
            Self::Gzip => "gzip archive",
            Self::SevenZip => "7z archive",
            Self::Rar => "RAR archive",
            Self::Tar => "tar archive",
            Self::Mp3 => "MP3 audio",
            Self::Flac => "FLAC audio",
            Self::Ogg => "Ogg media",
            Self::Wave => "WAVE audio",
            Self::Avi => "AVI video",
            Self::Mp4 => "MP4/QuickTime media",
            Self::WebM => "WebM/Matroska media",
        }
    }

    const fn is_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg | Self::Gif | Self::WebP)
    }

    fn matches_extension(self, extension: &str) -> bool {
        match self {
            Self::Png => extension == "png",
            Self::Jpeg => matches!(extension, "jpg" | "jpeg"),
            Self::Gif => extension == "gif",
            Self::WebP => extension == "webp",
            Self::Pdf => extension == "pdf",
            Self::Docx => extension == "docx",
            Self::Xlsx => extension == "xlsx",
            Self::Pptx => extension == "pptx",
            Self::Zip => extension == "zip",
            Self::Gzip => matches!(extension, "gz" | "tgz"),
            Self::SevenZip => extension == "7z",
            Self::Rar => extension == "rar",
            Self::Tar => extension == "tar",
            Self::Mp3 => extension == "mp3",
            Self::Flac => extension == "flac",
            Self::Ogg => matches!(extension, "ogg" | "oga" | "ogv" | "opus"),
            Self::Wave => extension == "wav",
            Self::Avi => extension == "avi",
            Self::Mp4 => matches!(extension, "mp4" | "m4a" | "m4v" | "mov"),
            Self::WebM => matches!(extension, "webm" | "mkv" | "mka"),
        }
    }
}

fn detect_passive_type(
    probe: &[u8],
    file: SafeFile,
    extension: Option<&str>,
) -> Result<Option<PassiveType>> {
    let detected = if probe.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(PassiveType::Png)
    } else if probe.starts_with(b"\xff\xd8\xff") {
        Some(PassiveType::Jpeg)
    } else if probe.starts_with(b"GIF87a") || probe.starts_with(b"GIF89a") {
        Some(PassiveType::Gif)
    } else if probe.starts_with(b"RIFF") && probe.get(8..12) == Some(b"WEBP") {
        Some(PassiveType::WebP)
    } else if probe.starts_with(b"%PDF-") {
        Some(PassiveType::Pdf)
    } else if matches!(
        probe.get(..4),
        Some(b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
    ) {
        if matches!(extension, Some("docx" | "xlsx" | "pptx")) {
            Some(detect_ooxml(file)?.ok_or_else(|| {
                anyhow::anyhow!("the claimed OOXML file has no matching main content type")
            })?)
        } else {
            Some(PassiveType::Zip)
        }
    } else if probe.starts_with(b"\x1f\x8b") {
        Some(PassiveType::Gzip)
    } else if probe.starts_with(b"7z\xbc\xaf\x27\x1c") {
        Some(PassiveType::SevenZip)
    } else if probe.starts_with(b"Rar!\x1a\x07") {
        Some(PassiveType::Rar)
    } else if probe.get(257..262) == Some(b"ustar") {
        Some(PassiveType::Tar)
    } else if probe.starts_with(b"ID3") {
        Some(PassiveType::Mp3)
    } else if probe.starts_with(b"fLaC") {
        Some(PassiveType::Flac)
    } else if probe.starts_with(b"OggS") {
        Some(PassiveType::Ogg)
    } else if probe.starts_with(b"RIFF") && probe.get(8..12) == Some(b"WAVE") {
        Some(PassiveType::Wave)
    } else if probe.starts_with(b"RIFF") && probe.get(8..12) == Some(b"AVI ") {
        Some(PassiveType::Avi)
    } else if probe.get(4..8) == Some(b"ftyp") {
        Some(PassiveType::Mp4)
    } else if probe.starts_with(b"\x1a\x45\xdf\xa3") {
        Some(PassiveType::WebM)
    } else {
        None
    };
    Ok(detected)
}

fn detect_ooxml(file: SafeFile) -> Result<Option<PassiveType>> {
    let mut archive = ZipArchive::new(file).context("cannot inspect OOXML ZIP container")?;
    if archive.offset() != 0 {
        bail!("OOXML container has prepended data and is rejected as a polyglot");
    }
    if archive.len() > MAX_OOXML_ENTRIES {
        bail!(
            "OOXML container has {} entries; the external-open limit is {MAX_OOXML_ENTRIES}",
            archive.len()
        );
    }
    let part = archive
        .by_name("[Content_Types].xml")
        .context("OOXML container is missing [Content_Types].xml")?;
    if part.encrypted() {
        bail!("encrypted OOXML content types are not supported for system opening");
    }
    if part.size() > MAX_CONTENT_TYPES_BYTES {
        bail!("OOXML content types exceed the external-open inspection limit");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(part.size()).unwrap_or(0));
    part.take(MAX_CONTENT_TYPES_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .context("cannot read OOXML content types")?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONTENT_TYPES_BYTES {
        bail!("OOXML content types grew beyond the inspection limit");
    }
    let text = std::str::from_utf8(&bytes).context("OOXML content types are not UTF-8")?;
    let lower = text.to_ascii_lowercase();
    if lower.contains("macroenabled") {
        bail!("macro-capable OOXML content was blocked from system opening");
    }
    if text.contains(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
    ) {
        Ok(Some(PassiveType::Docx))
    } else if text
        .contains("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml")
    {
        Ok(Some(PassiveType::Xlsx))
    } else if text.contains(
        "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
    ) {
        Ok(Some(PassiveType::Pptx))
    } else {
        Ok(None)
    }
}

fn is_claimed_binary_format(extension: &str) -> bool {
    matches!(
        extension,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "pdf"
            | "docx"
            | "xlsx"
            | "pptx"
            | "zip"
            | "gz"
            | "tgz"
            | "7z"
            | "rar"
            | "tar"
            | "mp3"
            | "flac"
            | "ogg"
            | "oga"
            | "ogv"
            | "opus"
            | "wav"
            | "avi"
            | "mp4"
            | "m4a"
            | "m4v"
            | "mov"
            | "webm"
            | "mkv"
            | "mka"
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PlatformOpenOutcome {
    Opened,
    Unavailable(String),
    Failed(String),
}

#[cfg(target_os = "linux")]
fn open_file_platform(path: &Path) -> PlatformOpenOutcome {
    if !graphical_session_present(
        std::env::var_os("DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
    ) {
        return PlatformOpenOutcome::Unavailable(
            "no DISPLAY or WAYLAND_DISPLAY desktop session was detected".to_owned(),
        );
    }
    match launch_command(Command::new("xdg-open").arg(path), "xdg-open") {
        LaunchAttempt::Missing => {
            match launch_command(Command::new("gio").arg("open").arg(path), "gio open") {
                LaunchAttempt::Missing => PlatformOpenOutcome::Unavailable(
                    "neither xdg-open nor gio is installed".to_owned(),
                ),
                LaunchAttempt::Outcome(outcome) => outcome,
            }
        }
        LaunchAttempt::Outcome(outcome) => outcome,
    }
}

#[cfg(target_os = "macos")]
fn open_file_platform(path: &Path) -> PlatformOpenOutcome {
    match launch_command(Command::new("open").arg("--").arg(path), "open") {
        LaunchAttempt::Missing => {
            PlatformOpenOutcome::Unavailable("open is not installed".to_owned())
        }
        LaunchAttempt::Outcome(outcome) => outcome,
    }
}

#[cfg(windows)]
fn open_file_platform(path: &Path) -> PlatformOpenOutcome {
    use std::{iter, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let operation: Vec<u16> = "open".encode_utf16().chain(iter::once(0)).collect();
    let path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    // SAFETY: every pointer is either null or points to a NUL-terminated UTF-16
    // buffer that remains alive for the duration of this synchronous call.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            operation.as_ptr(),
            path.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize > 32 {
        PlatformOpenOutcome::Opened
    } else {
        PlatformOpenOutcome::Failed(format!(
            "ShellExecuteW rejected the file with code {}",
            result as isize
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn open_file_platform(_path: &Path) -> PlatformOpenOutcome {
    PlatformOpenOutcome::Unavailable("this operating system has no open adapter".to_owned())
}

#[cfg(any(target_os = "linux", test))]
fn graphical_session_present(
    display: Option<&std::ffi::OsStr>,
    wayland_display: Option<&std::ffi::OsStr>,
) -> bool {
    [display, wayland_display]
        .into_iter()
        .flatten()
        .any(|value| !value.is_empty())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
enum LaunchAttempt {
    Missing,
    Outcome(PlatformOpenOutcome),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn launch_command(command: &mut Command, name: &str) -> LaunchAttempt {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return LaunchAttempt::Missing,
        Err(error) => {
            return LaunchAttempt::Outcome(PlatformOpenOutcome::Failed(format!(
                "cannot start {name}: {error}"
            )));
        }
    };

    let deadline = Instant::now() + QUICK_FAILURE_WINDOW;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return LaunchAttempt::Outcome(outcome_from_status(name, status)),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = thread::Builder::new()
                    .name("latte-lens-system-open-reaper".to_owned())
                    .spawn(move || {
                        let _ = child.wait();
                    });
                return LaunchAttempt::Outcome(PlatformOpenOutcome::Opened);
            }
            Err(error) => {
                return LaunchAttempt::Outcome(PlatformOpenOutcome::Failed(format!(
                    "cannot observe {name} startup: {error}"
                )));
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn outcome_from_status(name: &str, status: ExitStatus) -> PlatformOpenOutcome {
    if status.success() {
        PlatformOpenOutcome::Opened
    } else {
        PlatformOpenOutcome::Failed(format!("{name} exited before opening the file ({status})"))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use super::*;

    fn classify_fixture(name: &str, bytes: &[u8]) -> Result<Classification> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let path = root.join(name);
        fs::write(&path, bytes)?;
        classify_file(&path, Some(&root), false)
    }

    #[test]
    fn verified_pdf_and_text_are_directly_eligible() -> Result<()> {
        let pdf = classify_fixture("fixture.pdf", b"%PDF-1.7\n%%EOF")?;
        assert_eq!(pdf.eligibility, Eligibility::Direct);
        assert_eq!(pdf.detected, "PDF document");

        let text = classify_fixture("notes.txt", b"hello\nworld\n")?;
        assert_eq!(text.eligibility, Eligibility::Direct);
        assert_eq!(text.detected, "text");
        Ok(())
    }

    #[test]
    fn common_passive_signatures_are_directly_eligible() -> Result<()> {
        let mut tar = vec![0; 512];
        tar[257..262].copy_from_slice(b"ustar");
        let fixtures: Vec<(&str, Vec<u8>, &str, bool)> = vec![
            (
                "image.png",
                b"\x89PNG\r\n\x1a\n".to_vec(),
                "PNG image",
                true,
            ),
            ("image.jpg", b"\xff\xd8\xff".to_vec(), "JPEG image", true),
            ("image.gif", b"GIF89a".to_vec(), "GIF image", true),
            (
                "image.webp",
                b"RIFF\0\0\0\0WEBP".to_vec(),
                "WebP image",
                true,
            ),
            ("archive.zip", b"PK\x05\x06".to_vec(), "ZIP archive", false),
            ("archive.gz", b"\x1f\x8b".to_vec(), "gzip archive", false),
            (
                "archive.7z",
                b"7z\xbc\xaf\x27\x1c".to_vec(),
                "7z archive",
                false,
            ),
            (
                "archive.rar",
                b"Rar!\x1a\x07\0".to_vec(),
                "RAR archive",
                false,
            ),
            ("archive.tar", tar, "tar archive", false),
            ("audio.mp3", b"ID3".to_vec(), "MP3 audio", false),
            ("audio.flac", b"fLaC".to_vec(), "FLAC audio", false),
            ("audio.ogg", b"OggS".to_vec(), "Ogg media", false),
            (
                "audio.wav",
                b"RIFF\0\0\0\0WAVE".to_vec(),
                "WAVE audio",
                false,
            ),
            (
                "video.avi",
                b"RIFF\0\0\0\0AVI ".to_vec(),
                "AVI video",
                false,
            ),
            (
                "video.mp4",
                b"\0\0\0\0ftyp".to_vec(),
                "MP4/QuickTime media",
                false,
            ),
            (
                "video.webm",
                b"\x1a\x45\xdf\xa3".to_vec(),
                "WebM/Matroska media",
                false,
            ),
        ];

        for (name, bytes, expected, image_fallback) in fixtures {
            let classification = classify_fixture(name, &bytes)?;
            assert_eq!(classification.eligibility, Eligibility::Direct, "{name}");
            assert_eq!(classification.detected, expected, "{name}");
            assert_eq!(classification.image_fallback, image_fallback, "{name}");
        }
        Ok(())
    }

    #[test]
    fn unknown_binary_requires_confirmation() -> Result<()> {
        let unknown = classify_fixture("fixture.data", b"\x00\x01\x02\x03")?;
        assert_eq!(unknown.eligibility, Eligibility::ConfirmationRequired);
        assert_eq!(unknown.detected, "unknown regular file");
        Ok(())
    }

    #[test]
    fn disabled_adapter_preserves_typed_outcomes_and_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let adapter = SystemOpenAdapter::Disabled("fixture unavailable".to_owned());

        let image = root.join("image.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        assert!(matches!(
            open_file(&image, Some(&root), false, None, &adapter).unwrap(),
            ExternalOpenOutcome::Unavailable { reason, image_fallback: true }
                if reason == "fixture unavailable"
        ));

        let unknown = root.join("unknown.data");
        fs::write(&unknown, b"\0\x01\x02\x03").unwrap();
        let fingerprint = match open_file(&unknown, Some(&root), false, None, &adapter).unwrap() {
            ExternalOpenOutcome::ConfirmationRequired {
                fingerprint,
                detected,
            } => {
                assert_eq!(detected, "unknown regular file");
                fingerprint
            }
            outcome => panic!("unexpected unknown-file outcome: {outcome:?}"),
        };
        assert!(matches!(
            open_file(
                &unknown,
                Some(&root),
                false,
                Some(&fingerprint),
                &adapter,
            )
            .unwrap(),
            ExternalOpenOutcome::Unavailable { reason, image_fallback: false }
                if reason == "fixture unavailable"
        ));
    }

    #[test]
    fn scripts_and_passive_suffix_mismatches_are_blocked() {
        let script = classify_fixture("fixture.pdf", b"#!/bin/sh\necho nope\n").unwrap_err();
        assert!(script.to_string().contains("shebang script"));

        let mismatch = classify_fixture("fixture.png", b"plain text").unwrap_err();
        assert!(mismatch.to_string().contains("does not match"));
    }

    #[test]
    fn active_extensions_and_signatures_cover_each_platform_family() {
        for (name, expected) in [
            ("payload.exe", "executable or installer"),
            ("payload.PS1", "script"),
            ("payload.desktop", "launcher or shortcut"),
            ("payload.docm", "macro-capable Office"),
        ] {
            let error = classify_fixture(name, b"plain text").unwrap_err();
            assert!(error.to_string().contains(expected), "{name}: {error:#}");
        }

        for (bytes, expected) in [
            (b"#!fixture".as_slice(), "shebang"),
            (b"\x7fELFfixture".as_slice(), "ELF"),
            (b"MZfixture".as_slice(), "PE/DOS"),
            (b"\xfe\xed\xfa\xcffixture".as_slice(), "Mach-O"),
        ] {
            let error = classify_fixture("payload.data", bytes).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn ooxml_content_type_must_match_the_passive_suffix() {
        use std::io::Write as _;
        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("[Content_Types].xml", options).unwrap();
        writer
            .write_all(
                br#"<Types><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#,
            )
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let workbook = classify_fixture("fixture.xlsx", &bytes).unwrap();
        assert_eq!(workbook.eligibility, Eligibility::Direct);
        assert_eq!(workbook.detected, "XLSX workbook");

        let mismatch = classify_fixture("fixture.docx", &bytes).unwrap_err();
        assert!(mismatch.to_string().contains("extension claims docx"));
    }

    #[test]
    fn unknown_confirmation_is_bound_to_the_original_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("fixture.data");
        fs::write(&path, b"\x00\x01\x02\x03").unwrap();
        let first = classify_file(&path, Some(&root), false).unwrap();
        assert_eq!(first.eligibility, Eligibility::ConfirmationRequired);

        fs::write(&path, b"\x00changed").unwrap();
        let error = open_file(
            &path,
            Some(&root),
            false,
            Some(&first.fingerprint),
            &SystemOpenAdapter::Disabled("test adapter".to_owned()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("changed"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_permission_bits_are_blocked_even_for_text() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("notes.txt");
        fs::write(&path, b"plain text").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();

        let error = classify_file(&path, Some(&root), false).unwrap_err();
        assert!(error.to_string().contains("executable permission"));
    }

    #[test]
    fn linux_requires_a_non_empty_graphical_session_variable() {
        use std::ffi::OsStr;

        assert!(!graphical_session_present(None, None));
        assert!(!graphical_session_present(Some(OsStr::new("")), None));
        assert!(graphical_session_present(Some(OsStr::new(":0")), None));
        assert!(graphical_session_present(
            None,
            Some(OsStr::new("wayland-0"))
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn nonzero_opener_status_is_reported_as_failure() {
        let status = Command::new("false")
            .status()
            .expect("false fixture should start");
        assert!(matches!(
            outcome_from_status("fixture", status),
            PlatformOpenOutcome::Failed(message) if message.contains("fixture")
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn quickly_failing_opener_process_is_not_misreported_as_opened() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let opener = directory.path().join("opener");
        fs::write(&opener, "#!/bin/sh\nexit 23\n").unwrap();
        let mut permissions = fs::metadata(&opener).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&opener, permissions).unwrap();

        assert!(matches!(
            launch_command(&mut Command::new(opener), "fixture"),
            LaunchAttempt::Outcome(PlatformOpenOutcome::Failed(message)) if message.contains("fixture")
        ));
    }

    #[test]
    fn claimed_binary_extensions_are_explicit() {
        assert!(is_claimed_binary_format("pdf"));
        assert!(is_claimed_binary_format("xlsx"));
        assert!(!is_claimed_binary_format("rs"));
        assert!(active_extension(Some("ps1")).is_some());
        assert!(Path::new("notes.rs").extension().is_some());
    }
}
