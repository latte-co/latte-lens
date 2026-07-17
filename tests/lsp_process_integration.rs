#![cfg(feature = "navigation-test-support")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use latte_lens::{
    app::{App, SearchMode},
    navigation::{AppOptions, NavigationSettings},
    preview::PreviewRegistry,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

static ENVIRONMENT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(unix)]
#[test]
fn unix_process_group_cleanup_terminates_pipe_holding_descendant() {
    run_pipe_holding_descendant_cleanup("descendant", None);
}

#[cfg(windows)]
#[test]
fn windows_job_cleanup_terminates_pipe_holding_descendant() {
    run_pipe_holding_descendant_cleanup("descendant", None);
}

#[cfg(unix)]
#[test]
fn unix_ready_session_cleanup_terminates_exit_first_descendant() {
    run_pipe_holding_descendant_cleanup("ready-descendant", Some("ready-before-direct-exit"));
}

#[cfg(windows)]
#[test]
fn windows_ready_session_cleanup_terminates_exit_first_descendant() {
    run_pipe_holding_descendant_cleanup("ready-descendant", Some("ready-before-direct-exit"));
}

#[cfg(unix)]
#[test]
fn unix_incompatible_position_encoding_forces_terminal_cleanup() {
    run_incompatible_position_encoding_cleanup();
}

#[cfg(windows)]
#[test]
fn windows_incompatible_position_encoding_forces_terminal_cleanup() {
    run_incompatible_position_encoding_cleanup();
}

#[test]
fn repeated_real_crashes_back_off_and_fifth_failure_stops_spawning() {
    let _environment = ENVIRONMENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let container = tempfile::tempdir().unwrap();
    let workspace = container.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("caller.rs"), "caller!();\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let trace = container.path().join("crash.trace");
    let config = container.path().join("latte-lens.jsonc");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_latte-lens-lsp-test-helper"))
        .canonicalize()
        .unwrap();
    write_navigation_config(&config, &helper, &["crash-initialize"]);
    set_test_env("LATTELENS_CONFIG", config.canonicalize().unwrap());
    set_test_env("LATTELENS_TEST_TRACE", &trace);
    let loaded = NavigationSettings::load_user_config(&workspace);
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    let mut app = App::with_options(
        workspace,
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )
    .unwrap();
    app.wait_for_background();
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    let probe = app.navigation_test_probe();

    for (index, delay) in [1, 2, 4, 8, 30].into_iter().enumerate() {
        app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
        let expected = index + 1;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.poll_background();
            let starts = trace_count(&trace, "crash-started");
            let report = probe.snapshot();
            if starts == expected && report.sessions_cleaned >= expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "crash attempt {expected} timed out: starts={starts}, report={report:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        if expected < 5 {
            std::thread::sleep(Duration::from_secs(delay) + Duration::from_millis(100));
        }
    }

    app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        app.poll_background();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(trace_count(&trace, "crash-started"), 5);
    let report = probe.snapshot();
    assert_eq!(report.sessions_cleaned, 5);
    assert_eq!(report.process_owners_dropped, 5);
    assert_eq!(report.io_threads_joined, 15);
    drop(app);
    remove_test_env("LATTELENS_CONFIG");
    remove_test_env("LATTELENS_TEST_TRACE");
}

#[test]
fn distinct_session_keys_have_no_fixed_count_cap_and_reuse_identical_key() {
    let _environment = ENVIRONMENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let container = tempfile::tempdir().unwrap();
    let workspace = container.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_test_repo(&workspace);
    let mut roots = Vec::new();
    let mut relative_files = Vec::new();
    for index in 0..9 {
        let relative = PathBuf::from(format!("repo-{index:02}/caller.rs"));
        let root = workspace.join(format!("repo-{index:02}"));
        fs::create_dir_all(&root).unwrap();
        init_test_repo(&root);
        fs::write(workspace.join(&relative), "caller!();\n").unwrap();
        roots.push(root.canonicalize().unwrap());
        relative_files.push(relative);
    }
    let workspace = workspace.canonicalize().unwrap();
    let trace = container.path().join("session-reuse.trace");
    let config = container.path().join("latte-lens.jsonc");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_latte-lens-lsp-test-helper"))
        .canonicalize()
        .unwrap();
    write_navigation_config(&config, &helper, &["session-reuse"]);
    set_test_env("LATTELENS_CONFIG", config.canonicalize().unwrap());
    set_test_env("LATTELENS_TEST_TRACE", &trace);

    let loaded = NavigationSettings::load_user_config(&workspace);
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    let mut app = App::with_options(
        workspace,
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )
    .unwrap();
    app.wait_for_background();

    for (root, relative) in roots.iter().zip(&relative_files) {
        select_file_and_request_definition(&mut app, relative, &trace, root, 1);
        let root_uri = url::Url::from_file_path(root).unwrap().to_string();
        assert_eq!(
            trace_count(&trace, &format!("session-started={root_uri}")),
            1,
            "each distinct root-language key must start exactly one server"
        );
    }

    select_file_and_request_definition(&mut app, &relative_files[0], &trace, &roots[0], 2);
    let first_root_uri = url::Url::from_file_path(&roots[0]).unwrap().to_string();
    assert_eq!(
        trace_count(&trace, &format!("session-started={first_root_uri}")),
        1,
        "revisiting the first key after nine distinct roots must reuse its server"
    );
    assert_eq!(
        trace_prefix_count(&trace, "session-started="),
        9,
        "nine distinct same-language roots must remain independent"
    );

    let probe = app.navigation_test_probe();
    let drop_started = Instant::now();
    drop(app);
    assert!(
        drop_started.elapsed() < Duration::from_secs(10),
        "nine-session teardown exceeded its bounded deadline"
    );
    let report = probe.snapshot();
    assert_eq!(report.sessions_cleaned, 9);
    assert_eq!(report.clean_exits, 9);
    assert_eq!(report.direct_children_reaped, 9);
    assert_eq!(report.io_threads_joined, 27);
    assert_eq!(report.process_owners_dropped, 9);
    assert_eq!(trace_prefix_count(&trace, "session-exited="), 9);

    remove_test_env("LATTELENS_CONFIG");
    remove_test_env("LATTELENS_TEST_TRACE");
}

#[cfg(unix)]
#[test]
fn twelve_stalled_session_trees_shutdown_with_one_process_wide_deadline() {
    let _environment = ENVIRONMENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let container = tempfile::tempdir().unwrap();
    let workspace = container.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_test_repo(&workspace);
    let mut roots = Vec::new();
    let mut relative_files = Vec::new();
    for index in 0..12 {
        let relative = PathBuf::from(format!("stalled-{index:02}/caller.rs"));
        let root = workspace.join(format!("stalled-{index:02}"));
        fs::create_dir_all(&root).unwrap();
        init_test_repo(&root);
        fs::write(workspace.join(&relative), "caller!();\n").unwrap();
        roots.push(root.canonicalize().unwrap());
        relative_files.push(relative);
    }
    let workspace = workspace.canonicalize().unwrap();
    let trace = container.path().join("stalled-session.trace");
    let config = container.path().join("latte-lens.jsonc");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_latte-lens-lsp-test-helper"))
        .canonicalize()
        .unwrap();
    write_navigation_config(&config, &helper, &["stalled-session-tree"]);
    set_test_env("LATTELENS_CONFIG", config.canonicalize().unwrap());
    set_test_env("LATTELENS_TEST_TRACE", &trace);

    let loaded = NavigationSettings::load_user_config(&workspace);
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    let mut app = App::with_options(
        workspace,
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )
    .unwrap();
    app.wait_for_background();
    for (root, relative) in roots.iter().zip(&relative_files) {
        select_file_and_request_definition(&mut app, relative, &trace, root, 1);
    }
    assert_eq!(trace_prefix_count(&trace, "stalled-root="), 12);
    let direct_pids = trace_pids(&trace, "stalled-direct=");
    let descendant_pids = trace_pids(&trace, "stalled-descendant=");
    assert_eq!(direct_pids.len(), 12);
    assert_eq!(descendant_pids.len(), 12);
    assert!(direct_pids.iter().all(|pid| process_is_alive(*pid)));
    assert!(descendant_pids.iter().all(|pid| process_is_alive(*pid)));

    let probe = app.navigation_test_probe();
    let drop_started = Instant::now();
    drop(app);
    let elapsed = drop_started.elapsed();
    assert!(
        elapsed < Duration::from_secs(6),
        "12-session batch cleanup took {elapsed:?}; serial per-session deadlines exceed 40s"
    );
    assert_eq!(trace_prefix_count(&trace, "stalled-shutdown="), 12);
    wait_until(Duration::from_secs(2), || {
        direct_pids
            .iter()
            .chain(&descendant_pids)
            .all(|pid| !process_is_alive(*pid))
    });

    let report = probe.snapshot();
    assert_eq!(report.sessions_cleaned, 12);
    assert_eq!(report.clean_exits, 0);
    assert_eq!(report.forced_tree_cleanups, 12);
    assert_eq!(report.direct_children_reaped, 12);
    assert_eq!(report.io_threads_joined, 36);
    assert_eq!(report.process_owners_dropped, 12);
    assert_eq!(report.quarantined_process_owners, 0);

    remove_test_env("LATTELENS_CONFIG");
    remove_test_env("LATTELENS_TEST_TRACE");
}

#[cfg(unix)]
#[test]
fn escaped_pipe_owner_is_quarantined_without_fake_cleanup_or_unbounded_drop() {
    let _environment = ENVIRONMENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let container = tempfile::tempdir().unwrap();
    let workspace = container.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("caller.rs"), "caller!();\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let trace = container.path().join("escaped.trace");
    let config = container.path().join("latte-lens.jsonc");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_latte-lens-lsp-test-helper"))
        .canonicalize()
        .unwrap();
    write_navigation_config(&config, &helper, &["escaped-pipe-owner"]);
    set_test_env("LATTELENS_CONFIG", config.canonicalize().unwrap());
    set_test_env("LATTELENS_TEST_TRACE", &trace);
    let loaded = NavigationSettings::load_user_config(&workspace);
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    let mut app = App::with_options(
        workspace,
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )
    .unwrap();
    app.wait_for_background();
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
    let escaped_pid = wait_for_trace_pid(&trace, "escaped=", Duration::from_secs(5));
    let probe = app.navigation_test_probe();
    wait_until(Duration::from_secs(3), || {
        app.poll_background();
        probe.snapshot().quarantined_process_owners == 1
    });
    assert_eq!(probe.snapshot().quarantined_process_owners, 1);
    assert_eq!(trace_count(&trace, "escaped-started"), 1);
    for _ in 0..2 {
        app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
        app.poll_background();
    }
    let no_respawn_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < no_respawn_deadline {
        app.poll_background();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        trace_count(&trace, "escaped-started"),
        1,
        "a quarantined process owner must disable respawn"
    );
    let started = Instant::now();
    drop(app);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "quarantined runtime drop was not bounded"
    );
    let report = probe.snapshot();
    assert_eq!(report.sessions_cleaned, 0);
    assert_eq!(report.io_threads_joined, 0);
    assert_eq!(report.process_owners_dropped, 0);
    assert_eq!(report.quarantined_process_owners, 1);
    assert!(process_is_alive(escaped_pid));
    kill_process(escaped_pid);
    wait_until(Duration::from_secs(2), || !process_is_alive(escaped_pid));
    remove_test_env("LATTELENS_CONFIG");
    remove_test_env("LATTELENS_TEST_TRACE");
}

fn run_pipe_holding_descendant_cleanup(role: &str, ready_marker: Option<&str>) {
    let _environment = ENVIRONMENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let container = tempfile::tempdir().unwrap();
    let workspace = container.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("caller.rs"), "caller!();\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let trace = container.path().join("descendant.trace");
    let config = container.path().join("latte-lens.jsonc");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_latte-lens-lsp-test-helper"))
        .canonicalize()
        .unwrap();
    write_navigation_config(&config, &helper, &[role]);
    let config = config.canonicalize().unwrap();
    set_test_env("LATTELENS_CONFIG", config.as_os_str());
    set_test_env("LATTELENS_TEST_TRACE", trace.as_os_str());

    let loaded = NavigationSettings::load_user_config(&workspace);
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    let mut app = App::with_options(
        workspace,
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )
    .unwrap();
    app.wait_for_background();
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));

    let descendant_pid = wait_for_descendant_pid(&trace, Duration::from_secs(5));
    if let Some(marker) = ready_marker {
        wait_until(Duration::from_secs(2), || trace_contains(&trace, marker));
    }
    assert!(process_is_alive(descendant_pid));
    let probe = app.navigation_test_probe();
    let started = Instant::now();
    drop(app);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "process-tree cleanup exceeded its bounded deadline"
    );
    wait_until(Duration::from_secs(2), || !process_is_alive(descendant_pid));

    let report = probe.snapshot();
    assert_eq!(report.sessions_cleaned, 1);
    assert_eq!(report.clean_exits, 0);
    assert_eq!(report.forced_tree_cleanups, 1);
    assert_eq!(report.direct_children_reaped, 1);
    assert_eq!(report.io_threads_joined, 3);
    assert_eq!(report.process_owners_dropped, 1);

    remove_test_env("LATTELENS_CONFIG");
    remove_test_env("LATTELENS_TEST_TRACE");
}

fn run_incompatible_position_encoding_cleanup() {
    let _environment = ENVIRONMENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let container = tempfile::tempdir().unwrap();
    let workspace = container.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("caller.rs"), "caller!(); // 😀中\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let trace = container.path().join("utf8-initialize.trace");
    let config = container.path().join("latte-lens.jsonc");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_latte-lens-lsp-test-helper"))
        .canonicalize()
        .unwrap();
    write_navigation_config(&config, &helper, &["utf8-initialize"]);
    set_test_env("LATTELENS_CONFIG", config.canonicalize().unwrap());
    set_test_env("LATTELENS_TEST_TRACE", &trace);
    let loaded = NavigationSettings::load_user_config(&workspace);
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    let mut app = App::with_options(
        workspace,
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )
    .unwrap();
    app.wait_for_background();
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
    wait_until(Duration::from_secs(5), || {
        app.poll_background();
        trace_contains(&trace, "utf8-initialize-sent")
    });
    let probe = app.navigation_test_probe();
    wait_until(Duration::from_secs(5), || {
        app.poll_background();
        probe.snapshot().sessions_cleaned == 1
    });
    let report = probe.snapshot();
    assert_eq!(report.clean_exits, 0);
    assert_eq!(report.forced_tree_cleanups, 1);
    assert_eq!(report.direct_children_reaped, 1);
    assert_eq!(report.io_threads_joined, 3);
    assert_eq!(report.process_owners_dropped, 1);
    let started = Instant::now();
    drop(app);
    assert!(started.elapsed() < Duration::from_secs(2));
    remove_test_env("LATTELENS_CONFIG");
    remove_test_env("LATTELENS_TEST_TRACE");
}

fn write_navigation_config(path: &Path, helper: &Path, args: &[&str]) {
    let mut command = vec![helper.to_string_lossy().into_owned()];
    command.extend(args.iter().map(|argument| (*argument).to_owned()));
    fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "code_navigation": {
                "enabled": true,
                "languages": {
                    "rust": {
                        "enabled": true,
                        "engine": {
                            "type": "language_server",
                            "command": command
                        }
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn init_test_repo(root: &Path) {
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn select_file_and_request_definition(
    app: &mut App,
    relative: &Path,
    trace: &Path,
    server_root: &Path,
    expected_definitions: usize,
) {
    app.open_search(SearchMode::Files);
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for character in relative.to_string_lossy().chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    wait_until(Duration::from_secs(5), || {
        app.poll_background();
        app.selected_search_result()
            .is_some_and(|result| result.path == relative)
    });
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.wait_for_background();
    assert_eq!(app.selected_relative_path().as_deref(), Some(relative));
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE));
    let root_uri = url::Url::from_file_path(server_root).unwrap().to_string();
    let marker = format!("definition={root_uri}");
    wait_until(Duration::from_secs(5), || {
        app.poll_background();
        trace_count(trace, &marker) == expected_definitions
    });
    let settle_deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < settle_deadline {
        app.poll_background();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn trace_contains(trace: &Path, marker: &str) -> bool {
    fs::read_to_string(trace).is_ok_and(|contents| contents.lines().any(|line| line == marker))
}

fn trace_prefix_count(trace: &Path, prefix: &str) -> usize {
    fs::read_to_string(trace).map_or(0, |contents| {
        contents
            .lines()
            .filter(|line| line.starts_with(prefix))
            .count()
    })
}

fn trace_count(trace: &Path, marker: &str) -> usize {
    fs::read_to_string(trace).map_or(0, |contents| {
        contents.lines().filter(|line| *line == marker).count()
    })
}

fn trace_pids(trace: &Path, prefix: &str) -> Vec<u32> {
    fs::read_to_string(trace).map_or_else(
        |_| Vec::new(),
        |contents| {
            contents
                .lines()
                .filter_map(|line| line.strip_prefix(prefix))
                .filter_map(|value| value.parse().ok())
                .collect()
        },
    )
}

fn wait_for_descendant_pid(trace: &Path, timeout: Duration) -> u32 {
    wait_for_trace_pid(trace, "descendant=", timeout)
}

fn wait_for_trace_pid(trace: &Path, prefix: &str, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(trace)
            && let Some(pid) = contents
                .lines()
                .find_map(|line| line.strip_prefix(prefix))
                .and_then(|value| value.parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "descendant helper did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    let pid = libc::pid_t::try_from(pid).unwrap();
    // SAFETY: the pid came from the dedicated test helper trace and SIGKILL
    // is used only to clean the deliberately escaped test process.
    let result = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(result, 0, "cannot clean escaped test process");
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal zero only probes whether the process id exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Storage::FileSystem::SYNCHRONIZE,
        System::Threading::{OpenProcess, WaitForSingleObject},
    };
    // SAFETY: OpenProcess returns an owned probe handle or null.
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: handle is live for this non-blocking query and then closed once.
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    match result {
        WAIT_OBJECT_0 => false,
        WAIT_TIMEOUT => true,
        _ => false,
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out after {timeout:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn set_test_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: this integration binary serializes its test-only environment.
    unsafe { std::env::set_var(key, value) };
}

fn remove_test_env(key: &str) {
    // SAFETY: paired with the serialized mutation above.
    unsafe { std::env::remove_var(key) };
}
