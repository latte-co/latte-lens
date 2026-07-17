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

/// Local-only compatibility canary for an installed Codex CLI.
///
/// It is ignored in normal test/CI runs because the Codex binary is an external
/// versioned dependency. The canary uses a temporary CODEX_HOME and a loopback
/// failure server, so it neither reads user configuration nor calls a model.
#[test]
#[ignore = "requires an installed Codex CLI; run explicitly for compatibility validation"]
fn installed_codex_session_start_invokes_the_production_latte_lens_hook() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let sandbox = tempfile::tempdir().expect("sandbox");
    let codex_home = sandbox.path().join("codex-home");
    let workspace = sandbox.path().join("workspace");
    let state = sandbox.path().join("lens-state");
    let runtime = sandbox.path().join("lens-runtime");
    let home = sandbox.path().join("home");
    fs::create_dir_all(&codex_home).expect("Codex home");
    fs::create_dir_all(workspace.join(".git")).expect("workspace");
    fs::create_dir_all(&home).expect("home");

    let hook_command = format!(
        "{} hook --observer openai/codex-hook --event SessionStart --observer-version compatibility-canary",
        shell_quote(binary)
    );
    let hooks = format!(
        r#"{{"hooks":{{"SessionStart":[{{"matcher":"startup|resume|clear|compact","hooks":[{{"type":"command","command":"{}","timeout":1}}]}}]}}}}"#,
        json_escape(&hook_command)
    );
    fs::write(codex_home.join("hooks.json"), hooks).expect("temporary hooks.json");

    let listener = TcpListener::bind("127.0.0.1:0").expect("mock provider");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("mock address");
    let config = format!(
        "model = \"compatibility-canary\"\nmodel_provider = \"local_mock\"\n\n[model_providers.local_mock]\nname = \"Local mock\"\nbase_url = \"http://{address}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n"
    );
    fs::write(codex_home.join("config.toml"), config).expect("temporary config.toml");
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

    let stderr_path = sandbox.path().join("codex.stderr");
    let stderr = fs::File::create(&stderr_path).expect("Codex stderr");
    let mut child = Command::new("codex")
        .args(["--dangerously-bypass-hook-trust", "-C"])
        .arg(&workspace)
        .args(["exec", "compatibility canary; do not call tools"])
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
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
        .expect("installed Codex CLI");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut invoked = false;
    while Instant::now() < deadline {
        if has_session_record(&state) {
            invoked = true;
            break;
        }
        if child.try_wait().expect("Codex status").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().expect("Codex status").is_none() {
        child.kill().expect("stop bounded canary");
    }
    let status = child.wait().expect("Codex output");
    invoked |= has_session_record(&state);
    stop.store(true, Ordering::Release);
    server.join().expect("mock provider thread");
    let stderr = fs::read_to_string(stderr_path).unwrap_or_default();

    assert!(
        invoked,
        "installed Codex did not invoke the SessionStart hook from the isolated CODEX_HOME; status={:?}; stderr={stderr}",
        status.code(),
    );
}

fn has_session_record(state: &Path) -> bool {
    regular_files(state)
        .iter()
        .any(|path| path.components().any(|part| part.as_os_str() == "sessions"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
