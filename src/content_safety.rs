//! Shared policy for filesystem content reads.
//!
//! Content paths are inspected without following links before they are opened.
//! Supported desktop targets also ask the operating system not to follow the
//! final component at open time, then verify that the opened handle still
//! names the inspected regular file before any bytes are read.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentPathKind {
    Regular,
    SymbolicLink,
    Directory,
    #[cfg(unix)]
    Fifo,
    #[cfg(unix)]
    Socket,
    #[cfg(unix)]
    BlockDevice,
    #[cfg(unix)]
    CharacterDevice,
    #[cfg(windows)]
    ReparsePoint,
    Other,
}

impl ContentPathKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Regular => "regular file",
            Self::SymbolicLink => "symbolic link",
            Self::Directory => "directory",
            #[cfg(unix)]
            Self::Fifo => "FIFO (named pipe)",
            #[cfg(unix)]
            Self::Socket => "socket",
            #[cfg(unix)]
            Self::BlockDevice => "block device",
            #[cfg(unix)]
            Self::CharacterDevice => "character device",
            #[cfg(windows)]
            Self::ReparsePoint => "filesystem reparse point",
            Self::Other => "non-regular filesystem object",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ContentPathInspection {
    pub(crate) kind: ContentPathKind,
    pub(crate) path: PathBuf,
    metadata: Metadata,
}

pub(crate) enum OpenRegular {
    Opened(SafeFile),
    Declined(ContentPathInspection),
}

/// A handle that passed the shared non-following regular-file policy.
pub(crate) struct SafeFile {
    file: File,
    len: u64,
}

impl SafeFile {
    pub(crate) const fn len(&self) -> u64 {
        self.len
    }
}

impl Read for SafeFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for SafeFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

/// Inspect every path component below `content_root` without following links.
///
/// The root is expected to be canonical (the App and repository discovery
/// paths satisfy this). When no root is supplied, only the selected object can
/// be classified; callers that enforce a workspace boundary should always
/// provide one.
pub(crate) fn inspect_content_path(
    content_root: Option<&Path>,
    path: &Path,
) -> Result<ContentPathInspection> {
    let Some(root) = content_root else {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("cannot inspect {}", path.display()))?;
        return Ok(ContentPathInspection {
            kind: classify(&metadata),
            path: path.to_path_buf(),
            metadata,
        });
    };

    let relative = relative_beneath(root, path)?;
    if relative.as_os_str().is_empty() {
        let metadata = fs::symlink_metadata(root)
            .with_context(|| format!("cannot inspect {}", root.display()))?;
        return Ok(ContentPathInspection {
            kind: classify(&metadata),
            path: root.to_path_buf(),
            metadata,
        });
    }

    let mut current = root.to_path_buf();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            bail!("content path {} escapes {}", path.display(), root.display());
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("cannot inspect {}", current.display()))?;
        let kind = classify(&metadata);
        let is_final = index + 1 == component_count;
        if is_final || kind != ContentPathKind::Directory {
            return Ok(ContentPathInspection {
                kind,
                path: current,
                metadata,
            });
        }
    }

    unreachable!("a non-empty relative path has at least one component")
}

/// Open a regular file without allowing a symlink or special file to turn the
/// read into blocking or workspace-escaping I/O.
pub(crate) fn open_regular(content_root: Option<&Path>, path: &Path) -> Result<OpenRegular> {
    let inspected = inspect_content_path(content_root, path)?;
    if inspected.kind != ContentPathKind::Regular || inspected.path != path {
        return Ok(OpenRegular::Declined(inspected));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    configure_non_following_open(&mut options);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) => {
            // The final component may have changed after inspection. Recheck
            // it so a link/special-file race is a safe decline, not a read.
            if let Ok(changed) = inspect_content_path(content_root, path)
                && (changed.kind != ContentPathKind::Regular || changed.path != path)
            {
                return Ok(OpenRegular::Declined(changed));
            }
            return Err(error).with_context(|| format!("cannot open {}", path.display()));
        }
    };
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect opened file {}", path.display()))?;
    let opened_kind = classify(&opened_metadata);
    if opened_kind != ContentPathKind::Regular {
        return Ok(OpenRegular::Declined(ContentPathInspection {
            kind: opened_kind,
            path: path.to_path_buf(),
            metadata: opened_metadata,
        }));
    }

    verify_inspected_identity(path, &inspected.metadata, &opened_metadata)?;

    if let Some(root) = content_root {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot resolve opened file {}", path.display()))?;
        if !canonical.starts_with(root) {
            bail!(
                "content path {} resolved outside {} while it was being opened",
                path.display(),
                root.display()
            );
        }
        verify_current_identity(path, &canonical, &file, &opened_metadata)?;
    }

    Ok(OpenRegular::Opened(SafeFile {
        len: opened_metadata.len(),
        file,
    }))
}

/// Read a final symlink's target text without opening the target.
pub(crate) fn read_link_bounded(
    content_root: Option<&Path>,
    path: &Path,
    max_bytes: usize,
) -> Result<Option<(String, bool)>> {
    let inspected = inspect_content_path(content_root, path)?;
    if inspected.kind != ContentPathKind::SymbolicLink || inspected.path != path {
        return Ok(None);
    }

    let target = fs::read_link(path)
        .with_context(|| format!("cannot read symbolic link {}", path.display()))?;
    // Revalidate the final component. A replacement can change which target
    // text is shown, but it can never make this operation read target content.
    let current = fs::symlink_metadata(path)
        .with_context(|| format!("cannot revalidate symbolic link {}", path.display()))?;
    if classify(&current) != ContentPathKind::SymbolicLink {
        return Ok(None);
    }

    let target = target.to_string_lossy();
    let truncated = target.len() > max_bytes;
    let target = if truncated {
        let mut end = max_bytes.min(target.len());
        while !target.is_char_boundary(end) {
            end -= 1;
        }
        target[..end].to_owned()
    } else {
        target.into_owned()
    };
    Ok(Some((target, truncated)))
}

pub(crate) fn path_exists_without_following(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Classify a path by following symbolic links to their ultimate target.
///
/// Unlike [`inspect_content_path`], this resolves every link on the path and
/// reports the target's kind (a link to a regular file is [`Regular`], a link
/// to a directory is [`Directory`]). Special files and broken or looping links
/// surface as errors or non-regular kinds so callers still fail closed.
///
/// This is the deliberate All Files exception to the no-follow policy: it lets
/// the interactive filesystem view browse link targets, including targets that
/// live outside the selected workspace. Bulk and repository readers keep using
/// the non-following [`inspect_content_path`]/[`open_regular`] path.
///
/// [`Regular`]: ContentPathKind::Regular
/// [`Directory`]: ContentPathKind::Directory
pub(crate) fn inspect_following(path: &Path) -> Result<ContentPathInspection> {
    let metadata =
        fs::metadata(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    Ok(ContentPathInspection {
        kind: classify(&metadata),
        path: path.to_path_buf(),
        metadata,
    })
}

/// Report whether `path` resolves to a directory after following links.
///
/// Broken or looping links resolve to `false`, so a dangling directory link is
/// treated as a non-expandable leaf rather than crashing traversal.
pub(crate) fn resolves_to_directory(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Return the raw target text of a symbolic link, or `None` when `path` is not
/// a symbolic link. The target is read without following it, so it reflects
/// exactly what `ln -s` recorded (relative or absolute), never the resolved
/// destination.
pub(crate) fn symlink_target(path: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_symlink() {
        return None;
    }
    fs::read_link(path).ok()
}

/// Open a followed link target (or plain regular file) for reading.
///
/// The link is resolved to its canonical target first, then handed to the same
/// no-follow gate as [`open_regular`]. Because the canonical target contains no
/// links, `O_NOFOLLOW` still succeeds, and every special-file, TOCTOU, and
/// non-blocking guard from that gate keeps applying. This intentionally does
/// not enforce a workspace root: All Files browses link targets by design.
pub(crate) fn open_following(path: &Path) -> Result<OpenRegular> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    open_regular(None, &canonical)
}

pub(crate) fn ensure_beneath(root: &Path, path: &Path) -> Result<()> {
    let _ = relative_beneath(root, path)?;
    Ok(())
}

fn relative_beneath<'a>(root: &Path, path: &'a Path) -> Result<&'a Path> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "content path {} is outside selected workspace {}",
            path.display(),
            root.display()
        )
    })?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!(
            "content path {} escapes selected workspace {}",
            path.display(),
            root.display()
        );
    }
    Ok(relative)
}

fn classify(metadata: &Metadata) -> ContentPathKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return ContentPathKind::SymbolicLink;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return ContentPathKind::ReparsePoint;
        }
    }

    if file_type.is_file() {
        return ContentPathKind::Regular;
    }
    if file_type.is_dir() {
        return ContentPathKind::Directory;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        if file_type.is_fifo() {
            return ContentPathKind::Fifo;
        }
        if file_type.is_socket() {
            return ContentPathKind::Socket;
        }
        if file_type.is_block_device() {
            return ContentPathKind::BlockDevice;
        }
        if file_type.is_char_device() {
            return ContentPathKind::CharacterDevice;
        }
    }

    ContentPathKind::Other
}

#[cfg(unix)]
fn configure_non_following_open(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    // O_NONBLOCK keeps a last-moment regular-file-to-FIFO replacement from
    // blocking open(2). It has no effect on regular-file reads.
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn configure_non_following_open(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_non_following_open(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn verify_inspected_identity(path: &Path, left: &Metadata, right: &Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if left.dev() != right.dev() || left.ino() != right.ino() {
        bail!("{} changed while it was being opened", path.display());
    }
    Ok(())
}

#[cfg(windows)]
fn verify_inspected_identity(_path: &Path, _left: &Metadata, _right: &Metadata) -> Result<()> {
    // Rust 1.88 does not expose file identity on path metadata. The opened
    // handle is compared with a second no-follow handle after canonical
    // containment has been checked in `verify_current_identity` below.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_inspected_identity(_path: &Path, _left: &Metadata, _right: &Metadata) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn verify_current_identity(
    path: &Path,
    canonical: &Path,
    _opened_file: &File,
    opened_metadata: &Metadata,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let current_metadata = fs::metadata(canonical)
        .with_context(|| format!("cannot revalidate opened file {}", path.display()))?;
    if opened_metadata.dev() != current_metadata.dev()
        || opened_metadata.ino() != current_metadata.ino()
    {
        bail!(
            "{} changed while its workspace boundary was checked",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn verify_current_identity(
    path: &Path,
    canonical: &Path,
    opened_file: &File,
    _opened_metadata: &Metadata,
) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_non_following_open(&mut options);
    let current_file = options
        .open(canonical)
        .with_context(|| format!("cannot revalidate opened file {}", path.display()))?;
    let current_metadata = current_file
        .metadata()
        .with_context(|| format!("cannot inspect revalidation handle {}", path.display()))?;
    if classify(&current_metadata) != ContentPathKind::Regular {
        bail!(
            "{} changed while its workspace boundary was checked",
            path.display()
        );
    }
    if windows_file_identity(opened_file)? != windows_file_identity(&current_file)? {
        bail!(
            "{} changed while its workspace boundary was checked",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Result<(u32, u64)> {
    use std::{mem::MaybeUninit, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` owns a live handle for this call, and the Windows API
    // initializes the complete output structure when it returns nonzero.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error()).context("cannot read opened file identity");
    }
    // SAFETY: the successful API call above initialized `information`.
    let information = unsafe { information.assume_init() };
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, index))
}

#[cfg(not(any(unix, windows)))]
fn verify_current_identity(
    _path: &Path,
    _canonical: &Path,
    _opened_file: &File,
    _opened_metadata: &Metadata,
) -> Result<()> {
    Ok(())
}
