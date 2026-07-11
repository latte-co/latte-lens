use std::process::Command;

#[test]
fn help_and_version_are_available_without_entering_the_tui() {
    let binary = env!("CARGO_BIN_EXE_lattelens");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("A repository viewer built for multi-agent terminals"));
    assert!(help.contains("[PATH]"));

    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        concat!("lattelens ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_path_fails_before_terminal_initialization() {
    let binary = env!("CARGO_BIN_EXE_lattelens");
    let missing = tempfile::tempdir().unwrap().path().join("missing");
    let output = Command::new(binary).arg(&missing).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open"));
}
