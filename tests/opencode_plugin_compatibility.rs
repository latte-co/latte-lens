#![cfg(all(feature = "agent-observability", unix))]

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Local-only compatibility canary for an installed OpenCode CLI.
///
/// It launches an isolated loopback server, loads the real Latte Lens plugin,
/// and creates an empty session through OpenCode's local HTTP API. No prompt,
/// provider credential, model request, user configuration, or public network
/// is involved.
#[test]
#[ignore = "requires an installed OpenCode CLI; run explicitly for compatibility validation"]
fn installed_opencode_loads_the_production_latte_lens_plugin() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/opencode/latte-lens.js");
    let sandbox = tempfile::tempdir().expect("sandbox");
    let workspace = sandbox.path().join("workspace");
    let opencode_dir = workspace.join(".opencode");
    let home = sandbox.path().join("home");
    let global_config = home.join(".config/opencode");
    let state = sandbox.path().join("lens-state");
    let runtime = sandbox.path().join("lens-runtime");
    fs::create_dir_all(workspace.join(".git")).expect("workspace");
    for plugin_dir in [opencode_dir.join("plugin"), opencode_dir.join("plugins")] {
        fs::create_dir_all(&plugin_dir).expect("plugin directory");
        fs::copy(&plugin, plugin_dir.join("latte-lens.js")).expect("plugin artifact");
    }
    freeze_config_directory(&opencode_dir);
    freeze_config_directory(&global_config);

    let port = reserve_loopback_port();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stderr_path = sandbox.path().join("opencode.stderr");
    let stderr = fs::File::create(&stderr_path).expect("OpenCode stderr");
    let mut child = Command::new("opencode")
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--print-logs",
            "--log-level",
            "DEBUG",
        ])
        .current_dir(&workspace)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LATTE_LENS_BIN", binary)
        .env("LATTE_HOME", home.join(".latte"))
        .env("LATTE_LENS_STATE_DIR", &state)
        .env("LATTE_LENS_RUNTIME_DIR", &runtime)
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true")
        .env("OPENCODE_DISABLE_EXTERNAL_SKILLS", "true")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "true")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_SHARE", "true")
        .env("OPENCODE_DISABLE_CLAUDE_CODE", "true")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("installed OpenCode CLI");

    let ready = wait_for_server(&mut child, address, Duration::from_secs(30));
    let response = ready
        .then(|| create_empty_session(address, &workspace))
        .and_then(Result::ok);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !has_session_record(&state) {
        if child.try_wait().expect("OpenCode status").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let invoked = has_session_record(&state);
    stop_child(&mut child);
    thaw_config_directory(&opencode_dir);
    thaw_config_directory(&global_config);
    let stderr = fs::read_to_string(stderr_path).unwrap_or_default();

    assert!(
        ready,
        "OpenCode server did not become ready; stderr={stderr}"
    );
    assert!(
        response
            .as_ref()
            .is_some_and(|response| response.starts_with("HTTP/1.1 200")),
        "OpenCode session.create failed; response={response:?}; stderr={stderr}"
    );
    assert!(
        invoked,
        "installed OpenCode did not load the isolated Latte Lens plugin; stderr={stderr}"
    );
}

fn freeze_config_directory(directory: &Path) {
    fs::create_dir_all(directory).expect("config directory");
    let plugin = directory.join("node_modules/@opencode-ai/plugin");
    fs::create_dir_all(&plugin).expect("local plugin type stub");
    fs::write(
        directory.join("package.json"),
        r#"{"dependencies":{"@opencode-ai/plugin":"*"}}"#,
    )
    .expect("package.json");
    fs::write(
        directory.join("package-lock.json"),
        r#"{"packages":{"":{"dependencies":{"@opencode-ai/plugin":"*"}}}}"#,
    )
    .expect("package-lock.json");
    fs::write(
        plugin.join("package.json"),
        r#"{"name":"@opencode-ai/plugin","version":"0.0.0","type":"module","exports":"./index.js"}"#,
    )
    .expect("plugin package.json");
    fs::write(plugin.join("index.js"), "export {}\n").expect("plugin stub");
    fs::write(
        directory.join(".gitignore"),
        "node_modules\npackage.json\npackage-lock.json\nbun.lock\n.gitignore",
    )
    .expect("config .gitignore");
    fs::set_permissions(directory, fs::Permissions::from_mode(0o555))
        .expect("read-only config directory");
}

fn thaw_config_directory(directory: &Path) {
    let _ = fs::set_permissions(directory, fs::Permissions::from_mode(0o755));
}

fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("loopback port")
        .local_addr()
        .expect("loopback address")
        .port()
}

fn wait_for_server(child: &mut Child, address: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if server_is_healthy(address) {
            return true;
        }
        if child.try_wait().expect("OpenCode status").is_some() {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn server_is_healthy(address: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return false;
    }
    let request =
        format!("GET /global/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    read_http_response(&mut stream).is_ok_and(|response| {
        response.starts_with("HTTP/1.1 200") && response.contains("\"healthy\":true")
    })
}

fn create_empty_session(address: SocketAddr, workspace: &Path) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let request = format!(
        "POST /session HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nx-opencode-directory: {}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
        workspace.display()
    );
    stream.write_all(request.as_bytes())?;
    read_http_response(&mut stream)
}

fn read_http_response(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                response.extend_from_slice(&chunk[..count]);
                if complete_http_response(&response) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && !response.is_empty() =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    String::from_utf8(response)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn complete_http_response(response: &[u8]) -> bool {
    let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    content_length.is_some_and(|length| response.len() >= header_end + 4 + length)
}

fn stop_child(child: &mut Child) {
    if child.try_wait().expect("OpenCode status").is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn has_session_record(state: &Path) -> bool {
    regular_files(state)
        .iter()
        .any(|path| path.components().any(|part| part.as_os_str() == "sessions"))
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
