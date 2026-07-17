#![cfg(all(feature = "agent-observability", unix))]

use std::{
    env, fs,
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

/// Local-only compatibility canary for an installed TraeX.
///
/// It loads the checked-in hooks artifact from an isolated TraeX config home and
/// points model traffic at a loopback failure server. No prompt reaches a real
/// provider and no user configuration, credentials, or hook trust state is
/// read or modified.
#[test]
#[ignore = "requires an installed TraeX; run explicitly for compatibility validation"]
fn installed_traex_session_start_invokes_the_production_latte_lens_hook() {
    let binary = Path::new(env!("CARGO_BIN_EXE_latte-lens"));
    let traex_binary =
        env::var_os("LATTE_LENS_TRAEX_BIN").expect("LATTE_LENS_TRAEX_BIN must select TraeX");
    let artifact = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/traex/hooks.json");
    let sandbox = tempfile::tempdir().expect("sandbox");
    let trae_home = sandbox.path().join("trae-home");
    let cli_home = sandbox.path().join("trae-cli-home");
    let workspace = sandbox.path().join("workspace");
    let state = sandbox.path().join("lens-state");
    let runtime = sandbox.path().join("lens-runtime");
    let home = sandbox.path().join("home");
    fs::create_dir_all(&trae_home).expect("TraeX config home");
    fs::create_dir_all(&cli_home).expect("TraeX hook home");
    fs::create_dir_all(workspace.join(".git")).expect("workspace");
    fs::create_dir_all(&home).expect("home");
    fs::copy(&artifact, cli_home.join("hooks.json")).expect("TraeX hooks artifact");

    let listener = TcpListener::bind("127.0.0.1:0").expect("mock provider");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("mock address");
    let config = format!(
        "model = \"compatibility-canary\"\nmodel_provider = \"local_mock\"\n\n[features]\nhooks = true\n\n[model_providers.local_mock]\nname = \"Local mock\"\nbase_url = \"http://{address}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n"
    );
    fs::write(trae_home.join("traecli.toml"), config).expect("temporary traecli.toml");

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

    let binary_dir = binary.parent().expect("latte-lens binary directory");
    let path = env::join_paths(
        std::iter::once(binary_dir.to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .expect("canary PATH");
    let stderr_path = sandbox.path().join("traex.stderr");
    let stderr = fs::File::create(&stderr_path).expect("TraeX stderr");
    let mut child = Command::new(&traex_binary)
        .args([
            "exec",
            "--dangerously-bypass-hook-trust",
            "compatibility canary; do not call tools",
        ])
        .current_dir(&workspace)
        .env("TRAE_HOME", &trae_home)
        .env("TRAECLI_HOME", &cli_home)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PATH", path)
        .env("LATTE_HOME", home.join(".latte"))
        .env("LATTE_LENS_STATE_DIR", &state)
        .env("LATTE_LENS_RUNTIME_DIR", &runtime)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("OTEL_SDK_DISABLED", "true")
        .env("HTTP_PROXY", format!("http://{address}"))
        .env("HTTPS_PROXY", format!("http://{address}"))
        .env("ALL_PROXY", format!("http://{address}"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start TraeX {:?}: {error}", traex_binary));

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut invoked = false;
    while Instant::now() < deadline {
        if has_session_record(&state) {
            invoked = true;
            break;
        }
        if child.try_wait().expect("TraeX status").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().expect("TraeX status").is_none() {
        child.kill().expect("stop bounded canary");
    }
    let status = child.wait().expect("TraeX output");
    invoked |= has_session_record(&state);
    stop.store(true, Ordering::Release);
    server.join().expect("mock provider thread");
    let stderr = fs::read_to_string(stderr_path).unwrap_or_default();

    assert!(
        invoked,
        "TraeX {:?} did not invoke SessionStart from isolated config; status={:?}; stderr={stderr}",
        traex_binary,
        status.code(),
    );
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
