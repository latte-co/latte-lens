use std::process::Command;
#[cfg(feature = "agent-observability")]
use std::{fs, io::Write, path::Path, process::Stdio};
#[cfg(all(feature = "agent-observability", unix))]
use std::{
    sync::{Arc, Barrier, RwLock},
    thread,
    time::{Duration, Instant},
};

#[cfg(feature = "agent-observability")]
use latte_lens::agent::*;

#[test]
fn help_and_version_are_available_without_entering_the_tui() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("A repository viewer built for multi-agent terminals"));
    assert!(help.contains("[PATH]"));
    #[cfg(feature = "agent-observability")]
    assert!(help.contains("hook"));

    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        concat!("latte-lens ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_path_fails_before_terminal_initialization() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let missing = tempfile::tempdir().unwrap().path().join("missing");
    let output = Command::new(binary).arg(&missing).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open"));
}

#[cfg(feature = "agent-observability")]
#[test]
fn hook_cli_is_fail_open_and_silent_when_no_adapter_is_registered() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let mut child = Command::new(binary)
        .args([
            "hook",
            "--observer",
            "synthetic/absent",
            "--event",
            "activity",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start hook CLI");
    if let Err(error) = child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"privacy-canary-hook-payload")
    {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    let output = child.wait_with_output().expect("hook output");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[cfg(feature = "agent-observability")]
#[test]
fn hooks_setup_and_restore_update_user_configs_transactionally() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let sandbox = tempfile::tempdir().expect("sandbox");
    let home = sandbox.path().join("home");
    let state = sandbox.path().join("state");
    let temporary = sandbox.path().join("tmp");
    let codex = home.join(".codex");
    let claude = home.join(".claude");
    let opencode = home.join(".config/opencode");
    let traex = home.join(".trae");
    for directory in [&codex, &claude, &opencode, &traex, &temporary] {
        fs::create_dir_all(directory).expect("setup directory");
    }
    let codex_original = b"{}\n";
    let claude_original = b"{\"other\":true}\n";
    let traex_original = b"{\"other\":true}\n";
    fs::write(codex.join("hooks.json"), codex_original).expect("codex config");
    fs::write(claude.join("settings.json"), claude_original).expect("claude config");
    fs::write(traex.join("hooks.json"), traex_original).expect("traex config");

    let setup = Command::new(binary)
        .args(["hooks", "setup"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("LATTE_LENS_STATE_DIR", &state)
        .env("TMPDIR", &temporary)
        .output()
        .expect("hook setup");
    assert!(
        setup.status.success(),
        "hook setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let stdout = String::from_utf8(setup.stdout).expect("UTF-8 setup output");
    for agent in ["codex", "claude-code", "opencode", "traex"] {
        assert!(stdout.contains(&format!("configured {agent}")));
    }
    let transaction = stdout
        .lines()
        .find_map(|line| line.strip_prefix("hook setup transaction: "))
        .expect("transaction id");
    assert!(
        fs::read_to_string(codex.join("hooks.json"))
            .expect("codex config")
            .contains(CODEX_HOOK_OBSERVER_ID)
    );
    assert!(
        fs::read_to_string(claude.join("settings.json"))
            .expect("claude config")
            .contains(CLAUDE_HOOK_OBSERVER_ID)
    );
    assert!(
        fs::read_to_string(opencode.join("plugins/latte-lens.js"))
            .expect("OpenCode plugin")
            .contains(OPENCODE_PLUGIN_OBSERVER_ID)
    );
    assert!(
        fs::read_to_string(traex.join("hooks.json"))
            .expect("TraeX config")
            .contains(TRAEX_HOOK_OBSERVER_ID)
    );

    let restore = Command::new(binary)
        .args(["hooks", "restore", transaction])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("LATTE_LENS_STATE_DIR", &state)
        .env("TMPDIR", &temporary)
        .output()
        .expect("hook restore");
    assert!(
        restore.status.success(),
        "hook restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(
        fs::read(codex.join("hooks.json")).expect("codex"),
        codex_original
    );
    assert_eq!(
        fs::read(claude.join("settings.json")).expect("claude"),
        claude_original
    );
    assert_eq!(
        fs::read(traex.join("hooks.json")).expect("traex"),
        traex_original
    );
    assert!(!opencode.join("plugins/latte-lens.js").exists());
}

#[cfg(feature = "agent-observability")]
#[test]
fn malformed_hook_cli_is_also_fail_open_and_silent() {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let output = Command::new(binary)
        .args(["hook", "--observer"])
        .output()
        .expect("run malformed hook CLI");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[cfg(feature = "agent-observability")]
#[test]
fn codex_hook_cli_falls_back_to_private_metadata_without_persisting_payload_fields() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let workspace = sandbox.path().join("workspace");
    let child_workspace = workspace.join("crates/child");
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let home = sandbox.path().join("home");
    fs::create_dir_all(workspace.join(".git")).expect("git marker");
    fs::create_dir_all(&child_workspace).expect("child workspace");
    fs::create_dir_all(&home).expect("home");
    let canary = "privacy-canary-codex-prompt-tool-transcript";
    let payload = serde_json::json!({
        "session_id": format!("native-session-{canary}"),
        "transcript_path": format!("/{canary}/transcript.jsonl"),
        "cwd": child_workspace.to_string_lossy(),
        "hook_event_name": "UserPromptSubmit",
        "model": "gpt-test",
        "permission_mode": "default",
        "turn_id": format!("native-turn-{canary}"),
        "prompt": canary,
    })
    .to_string();

    let output = run_hook_child(
        &child_workspace,
        &state,
        &runtime,
        &home,
        (CODEX_HOOK_OBSERVER_ID, "0.144.3"),
        "UserPromptSubmit",
        payload.as_bytes(),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let files = regular_files(&state);
    assert!(
        files
            .iter()
            .any(|path| path.components().any(|part| part.as_os_str() == "sessions")),
        "offline hook must create one bounded metadata session record"
    );
    let retained = files
        .iter()
        .flat_map(|path| fs::read(path).expect("state file"))
        .collect::<Vec<_>>();
    for private in [
        canary.as_bytes(),
        child_workspace.as_os_str().to_string_lossy().as_bytes(),
    ] {
        assert!(!contains_bytes(&retained, private));
    }
}

#[cfg(all(feature = "agent-observability", unix))]
#[test]
fn codex_hook_cli_uses_exact_workspace_and_keeps_ack_loss_fallback_private() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let workspace = sandbox.path().join("workspace");
    let child_workspace = workspace.join("nested/agent");
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let home = sandbox.path().join("home");
    fs::create_dir_all(workspace.join(".git")).expect("git marker");
    fs::create_dir_all(&child_workspace).expect("child workspace");
    fs::create_dir_all(&home).expect("home");

    let identity = load_or_create_install_identity(state.clone()).expect("identity");
    let resolved = resolve_workspace(&workspace, &identity).expect("workspace");
    let adapters = Arc::new(production_adapter_registry());
    let observer = ObserverId::parse(CODEX_HOOK_OBSERVER_ID).expect("observer");
    let adapter = adapters.resolve(&observer).expect("Codex adapter");
    let fixture_payload = br#"{"session_id":"live-session","cwd":"/ignored","hook_event_name":"UserPromptSubmit","model":"gpt-test","turn_id":"live-turn","prompt":"privacy-canary-live"}"#;
    let HookDecodeOutcome::Event(fixture) = adapter
        .decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name: "UserPromptSubmit",
                observer_version: Some("0.144.3"),
                observed_at: Timestamp::from_unix_millis(1),
                workspace: Some(resolved.primary().clone()),
                payload: fixture_payload,
            },
            &identity,
        )
        .expect("fixture decode")
    else {
        panic!("Codex hook must emit an event")
    };
    let contract = adapter.contract_template(Some("0.144.3")).hook_contract(
        fixture.stream.instance.clone(),
        ContractRevision::new(1),
        Some(BoundedText::try_new("0.144.3").expect("version")),
    );
    let instances = Arc::new(RwLock::new(InstanceRegistry::new()));
    instances
        .write()
        .expect("instances")
        .upsert(contract, fixture.stream.epoch.clone())
        .expect("contract");
    let policy = Arc::new(LiveIngressPolicy::new(
        41,
        identity.install_id().clone(),
        resolved.selector().workspaces().iter().cloned(),
        Arc::clone(&adapters),
        instances,
    ));
    let registry =
        FilesystemLiveReceiverRegistry::new(runtime.clone(), identity.install_id().clone())
            .expect("registry");
    let mut receiver =
        bind_registered_live_receiver(&registry, policy, resolved.selector().workspaces())
            .expect("receiver");
    let ready = Arc::new(Barrier::new(2));
    let receiver_ready = Arc::clone(&ready);
    let receiver_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        receiver_ready.wait();
        loop {
            match receiver.receive(deadline) {
                ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                outcome => return outcome,
            }
        }
    });
    ready.wait();

    let child_payload = br#"{"session_id":"child-session","cwd":"/ignored","hook_event_name":"UserPromptSubmit","turn_id":"child-turn","prompt":"privacy-canary-child"}"#;
    let child_output = run_hook_child(
        &child_workspace,
        &state,
        &runtime,
        &home,
        (CODEX_HOOK_OBSERVER_ID, "0.144.3"),
        "UserPromptSubmit",
        child_payload,
    );
    assert!(child_output.status.success());
    assert!(child_output.stdout.is_empty());
    assert!(child_output.stderr.is_empty());
    assert!(
        regular_files(&state)
            .iter()
            .any(|path| path.components().any(|part| part.as_os_str() == "sessions")),
        "a child-directory Hook must fall back instead of reaching the parent Lens receiver"
    );

    let output = run_hook_child(
        &workspace,
        &state,
        &runtime,
        &home,
        (CODEX_HOOK_OBSERVER_ID, "0.144.3"),
        "UserPromptSubmit",
        fixture_payload,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let received = receiver_thread.join().expect("receiver thread");
    let ReceiveOutcome::Event {
        receiver_generation,
        event,
        ..
    } = received
    else {
        panic!("Codex event was not delivered live: {received:?}")
    };
    assert_eq!(receiver_generation, 41);
    let StreamOp::Upsert(observations) = &event.op else {
        panic!("Codex hook must publish an upsert")
    };
    assert!(
        observations
            .iter()
            .any(|observation| matches!(observation.kind, ObservationKind::Turn(TurnOp::Started)))
    );
    assert!(observations.iter().all(|observation| {
        observation
            .session
            .as_ref()
            .is_some_and(|session| session.workspace() == resolved.primary())
    }));
    assert!(!format!("{event:?}").contains("privacy-canary-live"));
    // The receiver accepting a frame and the Hook observing its ACK are
    // distinct facts. Under scheduler pressure the fixed 5 ms production
    // deadline may expire after receipt, in which case the intentional,
    // idempotent metadata fallback is also present. Either outcome must keep
    // raw payload and workspace values out of persistent state.
    let retained = regular_files(&state)
        .iter()
        .flat_map(|path| fs::read(path).expect("state file"))
        .collect::<Vec<_>>();
    for private in [
        b"privacy-canary-live".as_slice(),
        b"privacy-canary-child".as_slice(),
        workspace.as_os_str().to_string_lossy().as_bytes(),
        child_workspace.as_os_str().to_string_lossy().as_bytes(),
    ] {
        assert!(!contains_bytes(&retained, private));
    }
}

#[cfg(all(feature = "agent-observability", unix))]
#[test]
fn claude_hook_cli_uses_exact_workspace_and_preserves_terminal_outcomes() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let workspace = sandbox.path().join("workspace");
    let child_workspace = workspace.join("nested/agent");
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let home = sandbox.path().join("home");
    fs::create_dir_all(workspace.join(".git")).expect("git marker");
    fs::create_dir_all(&child_workspace).expect("child workspace");
    fs::create_dir_all(&home).expect("home");

    let identity = load_or_create_install_identity(state.clone()).expect("identity");
    let resolved = resolve_workspace(&workspace, &identity).expect("workspace");
    let adapters = Arc::new(production_adapter_registry());
    let observer = ObserverId::parse(CLAUDE_HOOK_OBSERVER_ID).expect("observer");
    let adapter = adapters.resolve(&observer).expect("Claude adapter");
    let fixture_payload = br#"{"session_id":"live-session","prompt_id":"live-prompt","cwd":"/ignored","hook_event_name":"SessionEnd","reason":"other","transcript_path":"/privacy-canary-live"}"#;
    let HookDecodeOutcome::Event(fixture) = adapter
        .decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name: "SessionEnd",
                observer_version: Some("2.1.200"),
                observed_at: Timestamp::from_unix_millis(1),
                workspace: Some(resolved.primary().clone()),
                payload: fixture_payload,
            },
            &identity,
        )
        .expect("fixture decode")
    else {
        panic!("Claude hook must emit an event")
    };
    let contract = adapter.contract_template(Some("2.1.200")).hook_contract(
        fixture.stream.instance.clone(),
        ContractRevision::new(1),
        Some(BoundedText::try_new("2.1.200").expect("version")),
    );
    let instances = Arc::new(RwLock::new(InstanceRegistry::new()));
    instances
        .write()
        .expect("instances")
        .upsert(contract, fixture.stream.epoch.clone())
        .expect("contract");
    let policy = Arc::new(LiveIngressPolicy::new(
        73,
        identity.install_id().clone(),
        resolved.selector().workspaces().iter().cloned(),
        Arc::clone(&adapters),
        instances,
    ));
    let registry =
        FilesystemLiveReceiverRegistry::new(runtime.clone(), identity.install_id().clone())
            .expect("registry");
    let mut receiver =
        bind_registered_live_receiver(&registry, policy, resolved.selector().workspaces())
            .expect("receiver");
    let ready = Arc::new(Barrier::new(2));
    let receiver_ready = Arc::clone(&ready);
    let receiver_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        receiver_ready.wait();
        loop {
            match receiver.receive(deadline) {
                ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                outcome => return outcome,
            }
        }
    });
    ready.wait();

    let child_payload = br#"{"session_id":"child-session","prompt_id":"child-prompt","cwd":"/ignored","hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_use_id":"child-tool","tool_input":{"command":"privacy-canary-child"},"error":"failure","error_details":"privacy-canary-child"}"#;
    let child_output = run_hook_child(
        &child_workspace,
        &state,
        &runtime,
        &home,
        (CLAUDE_HOOK_OBSERVER_ID, "2.1.200"),
        "PostToolUseFailure",
        child_payload,
    );
    assert!(child_output.status.success());
    assert!(child_output.stdout.is_empty());
    assert!(child_output.stderr.is_empty());
    assert!(
        regular_files(&state)
            .iter()
            .any(|path| path.components().any(|part| part.as_os_str() == "sessions")),
        "a child-directory Claude Hook must use private metadata fallback"
    );

    let output = run_hook_child(
        &workspace,
        &state,
        &runtime,
        &home,
        (CLAUDE_HOOK_OBSERVER_ID, "2.1.200"),
        "SessionEnd",
        fixture_payload,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let received = receiver_thread.join().expect("receiver thread");
    let ReceiveOutcome::Event {
        receiver_generation,
        event,
        ..
    } = received
    else {
        panic!("Claude event was not delivered live: {received:?}")
    };
    assert_eq!(receiver_generation, 73);
    let StreamOp::Upsert(observations) = &event.op else {
        panic!("Claude hook must publish an upsert")
    };
    assert!(observations.iter().any(|observation| matches!(
        observation.kind,
        ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended))
    )));
    assert!(observations.iter().all(|observation| {
        observation
            .session
            .as_ref()
            .is_some_and(|session| session.workspace() == resolved.primary())
    }));
    assert!(!format!("{event:?}").contains("privacy-canary-live"));

    let retained = regular_files(&state)
        .iter()
        .flat_map(|path| fs::read(path).expect("state file"))
        .collect::<Vec<_>>();
    for private in [
        b"privacy-canary-live".as_slice(),
        b"privacy-canary-child".as_slice(),
        workspace.as_os_str().to_string_lossy().as_bytes(),
        child_workspace.as_os_str().to_string_lossy().as_bytes(),
    ] {
        assert!(!contains_bytes(&retained, private));
    }
}

#[cfg(all(feature = "agent-observability", unix))]
#[test]
fn traex_hook_cli_uses_exact_workspace_and_preserves_terminal_outcomes() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let workspace = sandbox.path().join("workspace");
    let child_workspace = workspace.join("nested/agent");
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let home = sandbox.path().join("home");
    fs::create_dir_all(workspace.join(".git")).expect("git marker");
    fs::create_dir_all(&child_workspace).expect("child workspace");
    fs::create_dir_all(&home).expect("home");

    let identity = load_or_create_install_identity(state.clone()).expect("identity");
    let resolved = resolve_workspace(&workspace, &identity).expect("workspace");
    let adapters = Arc::new(production_adapter_registry());
    let observer = ObserverId::parse(TRAEX_HOOK_OBSERVER_ID).expect("observer");
    let adapter = adapters.resolve(&observer).expect("TraeX adapter");
    let fixture_payload = br#"{"session_id":"live-session","cwd":"/ignored","hook_event_name":"SessionEnd","reason":"other","transcript_path":"/privacy-canary-traex-live","thread_name":"privacy-canary-traex-live"}"#;
    let HookDecodeOutcome::Event(fixture) = adapter
        .decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name: "SessionEnd",
                observer_version: Some("0.120.46"),
                observed_at: Timestamp::from_unix_millis(1),
                workspace: Some(resolved.primary().clone()),
                payload: fixture_payload,
            },
            &identity,
        )
        .expect("fixture decode")
    else {
        panic!("TraeX hook must emit an event")
    };
    let contract = adapter.contract_template(Some("0.120.46")).hook_contract(
        fixture.stream.instance.clone(),
        ContractRevision::new(1),
        Some(BoundedText::try_new("0.120.46").expect("version")),
    );
    let instances = Arc::new(RwLock::new(InstanceRegistry::new()));
    instances
        .write()
        .expect("instances")
        .upsert(contract, fixture.stream.epoch.clone())
        .expect("contract");
    let policy = Arc::new(LiveIngressPolicy::new(
        97,
        identity.install_id().clone(),
        resolved.selector().workspaces().iter().cloned(),
        Arc::clone(&adapters),
        instances,
    ));
    let registry =
        FilesystemLiveReceiverRegistry::new(runtime.clone(), identity.install_id().clone())
            .expect("registry");
    let mut receiver =
        bind_registered_live_receiver(&registry, policy, resolved.selector().workspaces())
            .expect("receiver");
    let ready = Arc::new(Barrier::new(2));
    let receiver_ready = Arc::clone(&ready);
    let receiver_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        receiver_ready.wait();
        loop {
            match receiver.receive(deadline) {
                ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                outcome => return outcome,
            }
        }
    });
    ready.wait();

    let child_payload = br#"{"session_id":"child-session","cwd":"/ignored","hook_event_name":"PostToolUseFailure","turn_id":"child-turn","tool_name":"Bash","tool_use_id":"child-tool","tool_input":{"command":"privacy-canary-traex-child"},"tool_response":"privacy-canary-traex-child"}"#;
    let child_output = run_hook_child(
        &child_workspace,
        &state,
        &runtime,
        &home,
        (TRAEX_HOOK_OBSERVER_ID, "0.120.46"),
        "PostToolUseFailure",
        child_payload,
    );
    assert!(child_output.status.success());
    assert!(child_output.stdout.is_empty());
    assert!(child_output.stderr.is_empty());
    assert!(
        regular_files(&state)
            .iter()
            .any(|path| path.components().any(|part| part.as_os_str() == "sessions")),
        "a child-directory TraeX Hook must use private metadata fallback"
    );

    let output = run_hook_child(
        &workspace,
        &state,
        &runtime,
        &home,
        (TRAEX_HOOK_OBSERVER_ID, "0.120.46"),
        "SessionEnd",
        fixture_payload,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let received = receiver_thread.join().expect("receiver thread");
    let ReceiveOutcome::Event {
        receiver_generation,
        event,
        ..
    } = received
    else {
        panic!("TraeX event was not delivered live: {received:?}")
    };
    assert_eq!(receiver_generation, 97);
    let StreamOp::Upsert(observations) = &event.op else {
        panic!("TraeX hook must publish an upsert")
    };
    assert!(observations.iter().any(|observation| matches!(
        observation.kind,
        ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended))
    )));
    assert!(observations.iter().all(|observation| {
        observation
            .session
            .as_ref()
            .is_some_and(|session| session.workspace() == resolved.primary())
    }));
    assert!(!format!("{event:?}").contains("privacy-canary-traex-live"));

    let retained = regular_files(&state)
        .iter()
        .flat_map(|path| fs::read(path).expect("state file"))
        .collect::<Vec<_>>();
    for private in [
        b"privacy-canary-traex-live".as_slice(),
        b"privacy-canary-traex-child".as_slice(),
        workspace.as_os_str().to_string_lossy().as_bytes(),
        child_workspace.as_os_str().to_string_lossy().as_bytes(),
    ] {
        assert!(!contains_bytes(&retained, private));
    }
}

#[cfg(all(feature = "agent-observability", unix))]
#[test]
fn opencode_plugin_hook_cli_uses_exact_workspace_and_native_idle_status() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let workspace = sandbox.path().join("workspace");
    let child_workspace = workspace.join("nested/agent");
    let state = sandbox.path().join("state");
    let runtime = sandbox.path().join("runtime");
    let home = sandbox.path().join("home");
    fs::create_dir_all(workspace.join(".git")).expect("git marker");
    fs::create_dir_all(&child_workspace).expect("child workspace");
    fs::create_dir_all(&home).expect("home");

    let identity = load_or_create_install_identity(state.clone()).expect("identity");
    let resolved = resolve_workspace(&workspace, &identity).expect("workspace");
    let adapters = Arc::new(production_adapter_registry());
    let observer = ObserverId::parse(OPENCODE_PLUGIN_OBSERVER_ID).expect("observer");
    let adapter = adapters.resolve(&observer).expect("OpenCode adapter");
    let fixture_payload = br#"{"session_id":"live-session","hook_event_name":"session.status","event_id":"live:1","status":"idle","turn_id":"live-turn","message":"privacy-canary-opencode-live","diff":[{"file":"privacy-canary-opencode-live"}]}"#;
    let HookDecodeOutcome::Event(fixture) = adapter
        .decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name: "session.status",
                observer_version: Some("1.15.11"),
                observed_at: Timestamp::from_unix_millis(1),
                workspace: Some(resolved.primary().clone()),
                payload: fixture_payload,
            },
            &identity,
        )
        .expect("fixture decode")
    else {
        panic!("OpenCode plugin event must emit an event")
    };
    let contract = adapter.contract_template(Some("1.15.11")).hook_contract(
        fixture.stream.instance.clone(),
        ContractRevision::new(1),
        Some(BoundedText::try_new("1.15.11").expect("version")),
    );
    let instances = Arc::new(RwLock::new(InstanceRegistry::new()));
    instances
        .write()
        .expect("instances")
        .upsert(contract, fixture.stream.epoch.clone())
        .expect("contract");
    let policy = Arc::new(LiveIngressPolicy::new(
        89,
        identity.install_id().clone(),
        resolved.selector().workspaces().iter().cloned(),
        Arc::clone(&adapters),
        instances,
    ));
    let registry =
        FilesystemLiveReceiverRegistry::new(runtime.clone(), identity.install_id().clone())
            .expect("registry");
    let mut receiver =
        bind_registered_live_receiver(&registry, policy, resolved.selector().workspaces())
            .expect("receiver");
    let ready = Arc::new(Barrier::new(2));
    let receiver_ready = Arc::clone(&ready);
    let receiver_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        receiver_ready.wait();
        loop {
            match receiver.receive(deadline) {
                ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                outcome => return outcome,
            }
        }
    });
    ready.wait();

    let child_payload = br#"{"session_id":"child-session","hook_event_name":"permission.replied","event_id":"child:1","permission_id":"per_child","reply":"reject","metadata":{"secret":"privacy-canary-opencode-child"}}"#;
    let child_output = run_hook_child(
        &child_workspace,
        &state,
        &runtime,
        &home,
        (OPENCODE_PLUGIN_OBSERVER_ID, "1.15.11"),
        "permission.replied",
        child_payload,
    );
    assert!(child_output.status.success());
    assert!(child_output.stdout.is_empty());
    assert!(child_output.stderr.is_empty());
    assert!(
        regular_files(&state)
            .iter()
            .any(|path| path.components().any(|part| part.as_os_str() == "sessions")),
        "a child-directory OpenCode plugin event must use private metadata fallback"
    );

    let output = run_hook_child(
        &workspace,
        &state,
        &runtime,
        &home,
        (OPENCODE_PLUGIN_OBSERVER_ID, "1.15.11"),
        "session.status",
        fixture_payload,
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let received = receiver_thread.join().expect("receiver thread");
    let ReceiveOutcome::Event {
        receiver_generation,
        event,
        ..
    } = received
    else {
        panic!("OpenCode event was not delivered live: {received:?}")
    };
    assert_eq!(receiver_generation, 89);
    let StreamOp::Upsert(observations) = &event.op else {
        panic!("OpenCode plugin must publish an upsert")
    };
    assert!(
        observations.iter().any(|observation| matches!(
            observation.kind,
            ObservationKind::Turn(TurnOp::Completed)
        ))
    );
    assert!(observations.iter().any(|observation| matches!(
        observation.kind,
        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle))
    )));
    assert!(observations.iter().all(|observation| {
        observation
            .session
            .as_ref()
            .is_some_and(|session| session.workspace() == resolved.primary())
    }));
    assert!(!format!("{event:?}").contains("privacy-canary-opencode-live"));

    let retained = regular_files(&state)
        .iter()
        .flat_map(|path| fs::read(path).expect("state file"))
        .collect::<Vec<_>>();
    for private in [
        b"privacy-canary-opencode-live".as_slice(),
        b"privacy-canary-opencode-child".as_slice(),
        workspace.as_os_str().to_string_lossy().as_bytes(),
        child_workspace.as_os_str().to_string_lossy().as_bytes(),
    ] {
        assert!(!contains_bytes(&retained, private));
    }
}

#[cfg(feature = "agent-observability")]
fn run_hook_child(
    workspace: &Path,
    state: &Path,
    runtime: &Path,
    home: &Path,
    adapter: (&str, &str),
    event: &str,
    payload: &[u8],
) -> std::process::Output {
    let binary = env!("CARGO_BIN_EXE_latte-lens");
    let (observer, observer_version) = adapter;
    let mut child = Command::new(binary)
        .args([
            "hook",
            "--observer",
            observer,
            "--event",
            event,
            "--observer-version",
            observer_version,
        ])
        .current_dir(workspace)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("LATTE_HOME", home.join(".latte"))
        .env("LATTE_LENS_STATE_DIR", state)
        .env("LATTE_LENS_RUNTIME_DIR", runtime)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start hook CLI");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload)
        .expect("write hook payload");
    child.wait_with_output().expect("hook output")
}

#[cfg(feature = "agent-observability")]
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
    files.sort();
    files
}

#[cfg(feature = "agent-observability")]
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
