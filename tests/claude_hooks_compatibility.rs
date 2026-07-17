#![cfg(all(feature = "agent-observability", unix))]

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Local-only compatibility canary for an installed Claude Code CLI.
///
/// It is ignored in normal test/CI runs because the Claude binary is an
/// external versioned dependency. The canary uses explicit temporary settings,
/// an isolated HOME, a dummy API key, and a loopback failure server. It neither
/// reads user configuration nor reaches an Anthropic model.
#[test]
#[ignore = "requires an installed Claude Code CLI; run explicitly for compatibility validation"]
fn installed_claude_session_start_invokes_the_production_latte_lens_hook() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let sandbox = tempfile::tempdir().expect("sandbox");
    let workspace = sandbox.path().join("workspace");
    let state = sandbox.path().join("lens-state");
    let runtime = sandbox.path().join("lens-runtime");
    let home = sandbox.path().join("home");
    let claude_home = home.join(".claude");
    fs::create_dir_all(workspace.join(".git")).expect("workspace");
    fs::create_dir_all(&claude_home).expect("Claude home");

    let settings = format!(
        r#"{{"hooks":{{"SessionStart":[{{"matcher":"startup|resume|clear|compact","hooks":[{{"type":"command","command":"{}","args":["hook","--observer","anthropic/claude-code-hook","--event","SessionStart","--observer-version","compatibility-canary","--workspace","${{CLAUDE_PROJECT_DIR}}"],"timeout":1}}]}}]}}}}"#,
        json_escape(binary)
    );
    let settings_path = sandbox.path().join("settings.json");
    fs::write(&settings_path, settings).expect("temporary settings.json");

    let listener = TcpListener::bind("127.0.0.1:0").expect("mock provider");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("mock address");
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = thread::spawn(move || {
        while !server_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                    let mut request = [0_u8; 2048];
                    let _ = stream.read(&mut request);
                    let response = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(response);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });

    let stderr_path = sandbox.path().join("claude.stderr");
    let stderr = fs::File::create(&stderr_path).expect("Claude stderr");
    let mut child = Command::new("claude")
        .args(["-p", "--settings"])
        .arg(&settings_path)
        .args([
            "--setting-sources",
            "user",
            "--model",
            "compatibility-canary",
            "--tools",
            "",
            "--no-session-persistence",
            "compatibility canary; do not call tools",
        ])
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CLAUDE_CONFIG_DIR", &claude_home)
        .env("ANTHROPIC_API_KEY", "compatibility-canary")
        .env("ANTHROPIC_BASE_URL", format!("http://{address}"))
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("LATTE_HOME", home.join(".latte"))
        .env("LATTE_LENS_STATE_DIR", &state)
        .env("LATTE_LENS_RUNTIME_DIR", &runtime)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("installed Claude CLI");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut invoked = false;
    while Instant::now() < deadline {
        if has_session_record(&state) {
            invoked = true;
            break;
        }
        if child.try_wait().expect("Claude status").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().expect("Claude status").is_none() {
        child.kill().expect("stop bounded canary");
    }
    let status = child.wait().expect("Claude output");
    invoked |= has_session_record(&state);
    stop.store(true, Ordering::Release);
    server.join().expect("mock provider thread");
    let stderr = fs::read_to_string(stderr_path).unwrap_or_default();

    assert!(
        invoked,
        "installed Claude did not invoke the SessionStart hook from isolated settings; status={:?}; stderr={stderr}",
        status.code(),
    );
}

fn has_session_record(state: &Path) -> bool {
    regular_files(state)
        .iter()
        .any(|path| path.components().any(|part| part.as_os_str() == "sessions"))
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn regular_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_dir() {
                pending.push(path);
            } else if metadata.file_type().is_file() {
                files.push(path);
            }
        }
    }
    files
}
