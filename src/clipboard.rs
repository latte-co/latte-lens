use std::{
    io::{self, Write},
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardMode {
    Auto,
    Native,
    Osc52,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClipboardDelivery {
    NativeConfirmed,
    TerminalSequenceSent,
}

pub(crate) fn copy_text(text: &str) -> io::Result<ClipboardDelivery> {
    let mode = match std::env::var_os("LATTELENS_CLIPBOARD") {
        Some(value) if value.eq_ignore_ascii_case("native") => ClipboardMode::Native,
        Some(value) if value.eq_ignore_ascii_case("osc52") => ClipboardMode::Osc52,
        _ => ClipboardMode::Auto,
    };
    let mut stdout = io::stdout().lock();
    copy_text_with_writer(text, mode, &mut stdout)
}

fn copy_text_with_writer(
    text: &str,
    mode: ClipboardMode,
    writer: &mut impl Write,
) -> io::Result<ClipboardDelivery> {
    copy_text_with_writer_and_native(text, mode, writer, copy_with_native_command)
}

fn copy_text_with_writer_and_native(
    text: &str,
    mode: ClipboardMode,
    writer: &mut impl Write,
    native_copy: impl FnOnce(&str) -> io::Result<()>,
) -> io::Result<ClipboardDelivery> {
    match mode {
        ClipboardMode::Native => native_copy(text).map(|()| ClipboardDelivery::NativeConfirmed),
        ClipboardMode::Osc52 => {
            write_osc52(text, writer).map(|()| ClipboardDelivery::TerminalSequenceSent)
        }
        ClipboardMode::Auto => {
            // Auto deliberately attempts both delivery paths. Native clipboard
            // success must not suppress OSC 52, which is needed when Latte Lens
            // is running inside a nested terminal session.
            let native_result = native_copy(text);
            let terminal_result = write_osc52(text, writer);
            match (native_result, terminal_result) {
                (Ok(()), _) => Ok(ClipboardDelivery::NativeConfirmed),
                (Err(_), Ok(())) => Ok(ClipboardDelivery::TerminalSequenceSent),
                (Err(native_error), Err(_)) => Err(native_error),
            }
        }
    }
}

fn write_osc52(text: &str, writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(osc52_sequence(text).as_bytes())?;
    writer.flush()
}

#[cfg(target_os = "macos")]
fn copy_with_native_command(text: &str) -> io::Result<()> {
    run_copy_command("pbcopy", &[], text)
}

#[cfg(target_os = "windows")]
fn copy_with_native_command(text: &str) -> io::Result<()> {
    run_copy_command("clip.exe", &[], text)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn copy_with_native_command(text: &str) -> io::Result<()> {
    copy_with_native_command_runner(text, run_copy_command)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn copy_with_native_command_runner(
    text: &str,
    mut run: impl FnMut(&str, &[&str], &str) -> io::Result<()>,
) -> io::Result<()> {
    let candidates: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    let mut last_error = None;
    for (program, arguments) in candidates {
        match run(program, arguments, text) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("no clipboard command available")))
}

#[cfg(not(any(unix, target_os = "windows")))]
fn copy_with_native_command(_text: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no native clipboard command available",
    ))
}

fn run_copy_command(program: &str, arguments: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other(format!("{program} stdin is unavailable")))?;
    let write_result = stdin.write_all(text.as_bytes());
    drop(stdin);

    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "{program} exited with status {status}"
        )));
    }
    write_result
}

fn osc52_sequence(text: &str) -> String {
    format!("\u{1b}]52;c;{}\u{7}", STANDARD.encode(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encodes_utf8_without_putting_plain_text_in_the_escape() {
        let sequence = osc52_sequence("Latte 拿铁");

        assert_eq!(sequence, "\u{1b}]52;c;TGF0dGUg5ou/6ZOB\u{7}");
        assert!(!sequence.contains("拿铁"));
    }

    #[test]
    fn forced_osc52_copy_writes_and_flushes_the_terminal_sequence() {
        let mut output = Vec::new();

        let delivery = copy_text_with_writer("clean", ClipboardMode::Osc52, &mut output).unwrap();

        assert_eq!(delivery, ClipboardDelivery::TerminalSequenceSent);
        assert_eq!(output, b"\x1b]52;c;Y2xlYW4=\x07");
    }

    #[test]
    fn auto_emits_osc52_even_when_injected_native_delivery_succeeds() {
        let mut output = Vec::new();
        let mut native_input = None;

        let delivery = copy_text_with_writer_and_native(
            "Latte \u{62ff}\u{94c1}",
            ClipboardMode::Auto,
            &mut output,
            |text| {
                native_input = Some(text.to_owned());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(delivery, ClipboardDelivery::NativeConfirmed);
        assert_eq!(native_input.as_deref(), Some("Latte \u{62ff}\u{94c1}"));
        assert_eq!(output, b"\x1b]52;c;TGF0dGUg5ou/6ZOB\x07");
    }

    #[test]
    fn auto_reports_terminal_sequence_when_injected_native_delivery_fails() {
        let mut output = Vec::new();

        let delivery =
            copy_text_with_writer_and_native("fallback", ClipboardMode::Auto, &mut output, |_| {
                Err(io::Error::other("injected native failure"))
            })
            .unwrap();

        assert_eq!(delivery, ClipboardDelivery::TerminalSequenceSent);
        assert_eq!(output, b"\x1b]52;c;ZmFsbGJhY2s=\x07");
    }

    #[test]
    fn explicit_native_reports_confirmed_delivery_without_emitting_osc52() {
        let mut output = Vec::new();
        let mut native_input = None;

        let delivery = copy_text_with_writer_and_native(
            "native",
            ClipboardMode::Native,
            &mut output,
            |text| {
                native_input = Some(text.to_owned());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(delivery, ClipboardDelivery::NativeConfirmed);
        assert_eq!(native_input.as_deref(), Some("native"));
        assert!(output.is_empty());
    }

    #[test]
    fn explicit_native_propagates_injected_failure_without_emitting_osc52() {
        let mut output = Vec::new();

        let error =
            copy_text_with_writer_and_native("native", ClipboardMode::Native, &mut output, |_| {
                Err(io::Error::other("injected native failure"))
            })
            .unwrap_err();

        assert_eq!(error.to_string(), "injected native failure");
        assert!(output.is_empty());
    }

    #[test]
    fn auto_fails_when_native_and_terminal_delivery_both_fail() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("injected terminal failure"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error = copy_text_with_writer_and_native(
            "failure",
            ClipboardMode::Auto,
            &mut FailingWriter,
            |_| Err(io::Error::other("injected native failure")),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "injected native failure");
    }

    #[cfg(unix)]
    #[test]
    fn native_command_runner_streams_stdin_and_reports_exit_failure() {
        run_copy_command("cat", &[], "clipboard fixture").unwrap();

        let error = run_copy_command("false", &[], "ignored").unwrap_err();
        assert!(error.to_string().contains("exited with status"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn native_clipboard_candidates_fall_through_in_order_and_keep_the_last_error() {
        let mut attempts = Vec::new();
        copy_with_native_command_runner("clipboard fixture", |program, arguments, text| {
            attempts.push((
                program.to_owned(),
                arguments
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                text.to_owned(),
            ));
            if program == "xsel" {
                Ok(())
            } else {
                Err(io::Error::other(format!("{program} unavailable")))
            }
        })
        .unwrap();

        assert_eq!(
            attempts,
            [
                (
                    "wl-copy".to_owned(),
                    Vec::new(),
                    "clipboard fixture".to_owned()
                ),
                (
                    "xclip".to_owned(),
                    vec!["-selection".to_owned(), "clipboard".to_owned()],
                    "clipboard fixture".to_owned(),
                ),
                (
                    "xsel".to_owned(),
                    vec!["--clipboard".to_owned(), "--input".to_owned()],
                    "clipboard fixture".to_owned(),
                ),
            ]
        );

        let error = copy_with_native_command_runner("ignored", |program, _, _| {
            Err(io::Error::other(format!("{program} unavailable")))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "xsel unavailable");
    }
}
