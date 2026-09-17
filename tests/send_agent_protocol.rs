//! End-to-end argv-protocol test for the Herdr backend.
//!
//! A tiny shell script stands in for the `herdr` CLI: it answers
//! `agent list` with a fixed JSON envelope and appends every send/focus
//! invocation to a log file. This locks the exact argv contract
//! (`pane send-text <pane> <payload-as-single-arg>`, `agent focus <pane>`)
//! without needing a real workspace manager or PTY. POSIX only, matching the
//! project's E2E platform support.

#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Mutex, MutexGuard},
};

use latte_lens::send_agent::{AgentLifecycle, AgentTargetProvider, HerdrProvider};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    saved_herdr_env: Option<std::ffi::OsString>,
    saved_herdr_bin: Option<std::ffi::OsString>,
}

impl EnvGuard<'_> {
    fn lock(bin_path: &Path) -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let saved_herdr_env = std::env::var_os("HERDR_ENV");
        let saved_herdr_bin = std::env::var_os("HERDR_BIN_PATH");
        // SAFETY: tests touching process-global env are serialized on ENV_LOCK.
        unsafe {
            std::env::set_var("HERDR_ENV", "1");
            std::env::set_var("HERDR_BIN_PATH", bin_path);
        }
        Self {
            _guard: guard,
            saved_herdr_env,
            saved_herdr_bin,
        }
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: serialized on ENV_LOCK.
        unsafe {
            match &self.saved_herdr_env {
                Some(value) => std::env::set_var("HERDR_ENV", value),
                None => std::env::remove_var("HERDR_ENV"),
            }
            match &self.saved_herdr_bin {
                Some(value) => std::env::set_var("HERDR_BIN_PATH", value),
                None => std::env::remove_var("HERDR_BIN_PATH"),
            }
        }
    }
}

fn shell() -> &'static str {
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
        if Path::new(candidate).exists() {
            return candidate;
        }
    }
    "/bin/sh"
}

fn install_fake_herdr(workdir: &Path) -> std::path::PathBuf {
    let script = workdir.join("fake-herdr");
    let log = workdir.join("calls.log");
    // "$@" is NUL-joined into the log so multiline payloads survive verbatim.
    let body = format!(
        r#"#!{shell}
set -eu
log="{log}"
if [ "$1" = "agent" ] && [ "$2" = "list" ]; then
  cat <<'JSON'
{{"id":"cli:agent:list","result":{{"agents":[
  {{"agent":"codex","agent_status":"idle","cwd":"/workspace/repo","pane_id":"w9:p2","terminal_title_stripped":"codex work"}},
  {{"agent":"claude","agent_status":"blocked","cwd":"/workspace/other","pane_id":"w9:p3","terminal_title_stripped":"waiting"}}
]}},"type":"agent_list"}}
JSON
  exit 0
fi
if [ "$1" = "pane" ] && [ "$2" = "send-text" ]; then
  printf '%s\0' "$3" >> "$log"
  printf '%s\0' "$4" >> "$log"
  exit 0
fi
if [ "$1" = "agent" ] && [ "$2" = "focus" ]; then
  printf 'focus:%s\0' "$3" >> "$log"
  exit 0
fi
echo "unexpected invocation: $*" >&2
exit 2
"#,
        shell = shell(),
        log = log.display()
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    // No eager exec here: invoking the freshly-written script directly races
    // the kernel's executable/write window (ETXTBSY) under heavy parallel
    // test load. The provider retries ETXTBSY and the test asserts behavior
    // through it immediately afterwards.
    script
}

#[test]
fn herdr_backend_speaks_the_expected_argv_protocol() {
    let workdir = tempfile::tempdir().unwrap();
    let bin = install_fake_herdr(workdir.path());
    let _env = EnvGuard::lock(&bin);

    let provider = HerdrProvider::from_environment();
    assert!(provider.available());

    let discovery = provider.discover(Path::new("/workspace/repo")).unwrap();
    // Same-workspace idle codex sorts ahead of the blocked claude; the
    // blocked session stays present but is unselectable.
    let panes: Vec<&str> = discovery
        .targets
        .iter()
        .map(|target| target.pane_id.as_str())
        .collect();
    assert_eq!(panes, ["w9:p2", "w9:p3"]);
    assert_eq!(discovery.targets[0].status, AgentLifecycle::Idle);
    assert_eq!(discovery.targets[1].status, AgentLifecycle::Blocked);
    assert!(!discovery.targets[1].selectable);

    // Multiline + spaces travel as ONE literal argv element, with no newline
    // appended by the backend.
    let payload = "fn main() {\n    println!(\"héllo\");\n}\n";
    provider.send_draft("w9:p2", payload).unwrap();
    provider.focus_pane("w9:p2").unwrap();

    let log = fs::read(workdir.path().join("calls.log")).unwrap();
    let parts: Vec<&[u8]> = log.split(|byte| *byte == 0).collect();
    // trailing NUL produces an empty final element
    assert_eq!(parts[0], b"w9:p2");
    assert_eq!(parts[1], payload.as_bytes());
    assert_eq!(parts[2], "focus:w9:p2".as_bytes());
}

#[test]
fn herdr_backend_is_invisible_without_herdr_env() {
    let workdir = tempfile::tempdir().unwrap();
    let bin = install_fake_herdr(workdir.path());
    let guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let saved = std::env::var_os("HERDR_ENV");
    // SAFETY: serialized on ENV_LOCK.
    unsafe {
        std::env::set_var("HERDR_BIN_PATH", &bin);
        std::env::remove_var("HERDR_ENV");
    }
    let provider = HerdrProvider::from_environment();
    assert!(!provider.available());
    // SAFETY: serialized on ENV_LOCK.
    unsafe {
        if let Some(value) = saved {
            std::env::set_var("HERDR_ENV", value);
        }
    }
    drop(guard);
}
