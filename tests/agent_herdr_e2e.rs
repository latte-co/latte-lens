#![cfg(unix)]
#![cfg(feature = "agent-observability")]
//! Headless runtime journey for the Herdr read-only snapshot provider.
//!
//! Unlike `agent_herdr_arbitration.rs` (pure reducer), this drives the real
//! production stack: a fake `herdr` CLI on disk, `HerdrSnapshotProvider`
//! entered through its public environment gate, the real poll thread, the
//! real `HerdrSnapshotAdapter` decode, the Agent runtime worker, and App
//! reduction. It is the §10.5 cold-start blind-spot acceptance journey: at
//! Lens startup the provider already reports two sessions and no Hook event
//! ever exists, yet both must reach the Agents view and metadata projection.
//!
//! Unix-only because the journey stands a shell-script fake binary; Windows
//! coverage of the same reducer/contract behavior is cross-platform in
//! `agent_herdr_arbitration.rs`.

mod support;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use latte_lens::{agent::*, app::App};
use support::agent::{FakeIdentityKeyer, InMemoryMetadataStore};

/// All env mutation is process-global; this binary runs one journey at a
/// time behind this lock and always restores the prior values.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvGuard<'a> {
    _guard: std::sync::MutexGuard<'a, ()>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard<'_> {
    fn configure(socket: &Path, bin: &Path) -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let names = ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_BIN_PATH"];
        let saved = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        // SAFETY: serialized for the whole test binary by ENV_LOCK.
        unsafe {
            std::env::set_var("HERDR_ENV", "1");
            std::env::set_var("HERDR_SOCKET_PATH", socket);
            std::env::set_var("HERDR_BIN_PATH", bin);
        }
        Self {
            _guard: guard,
            saved,
        }
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: serialized for the whole test binary by ENV_LOCK.
        unsafe {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
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

/// Install a fake `herdr` that answers `--version` and `agent list` from
/// fixture files.
fn install_fake_herdr(dir: &Path) -> PathBuf {
    let script = dir.join("fake-herdr");
    let body = format!(
        r#"#!{shell}
set -eu
if [ "$1" = "--version" ]; then
  cat "{version}"
  exit 0
fi
if [ "$1" = "agent" ] && [ "$2" = "list" ]; then
  cat "{agents}"
  exit 0
fi
echo "unexpected invocation: $*" >&2
exit 2
"#,
        shell = shell(),
        version = dir.join("version.txt").display(),
        agents = dir.join("agents.json").display(),
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn agent_entry(pane: &str, session: &str, status: &str, cwd: &str, seq: u64) -> String {
    // Single-line JSON: raw strings do not process `\` line continuation.
    format!(
        r#"{{"agent":"claude","agent_session":{{"agent":"claude","kind":"id","source":"herdr:claude","value":"{session}"}},"agent_status":"{status}","cwd":"{cwd}","focused":false,"pane_id":"{pane}","revision":{seq},"state_change_seq":{seq},"tab_id":"t1","terminal_id":"term1","terminal_title":"title-canary","workspace_id":"w1"}}"#
    )
}

fn agents_json(entries: &[String]) -> String {
    format!(
        r#"{{"id":"cli:agent:list","result":{{"agents":[{entries}],"type":"agent_list"}}}}"#,
        entries = entries.join(",")
    )
}

fn poll_until(app: &mut App, predicate: impl Fn(&AgentViewState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        app.poll_background();
        if predicate(app.agent_view()) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("agent view did not converge: {:?}", app.agent_view());
}

#[test]
fn cold_start_herdr_snapshot_populates_sessions_without_any_hook_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let workspace = root.join("repo");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(root.join("version.txt"), "herdr 0.9.1\n").expect("version");

    // Two live Claude panes with distinct native session ids; one is
    // blocked, mapping to WaitingPermission as Observational evidence.
    let entries = [
        agent_entry(
            "w1:p1",
            "cold-session-aaaa",
            "working",
            workspace.to_str().unwrap(),
            1,
        ),
        agent_entry(
            "w1:p2",
            "cold-session-bbbb",
            "blocked",
            workspace.to_str().unwrap(),
            2,
        ),
    ];
    fs::write(root.join("agents.json"), agents_json(&entries)).expect("agents");

    let bin = install_fake_herdr(root);
    let socket = root.join("herdr.sock");
    let _env = EnvGuard::configure(&socket, &bin);

    let identity = Arc::new(FakeIdentityKeyer::new());
    let workspace_hint = identity
        .workspace_hint(SensitiveWorkspaceLocator::new(
            workspace.to_str().unwrap().as_bytes(),
        ))
        .expect("workspace hint");
    let selector =
        WorkspaceSelector::new(BoundedVec::try_from_vec(vec![workspace_hint]).expect("selector"));

    let metadata = Arc::new(InMemoryMetadataStore::default());
    let mut services = AgentRuntimeServices::new(
        Arc::new(production_adapter_registry()),
        identity,
        metadata.clone(),
    );
    // The environment gate is the only production construction path.
    let provider =
        HerdrSnapshotProvider::from_environment().expect("gate registers the herdr provider");
    services.providers.push(Box::new(provider));

    let mut app = App::new(workspace.clone()).expect("app");
    app.attach_agent_runtime(AgentRuntime::start(services), selector)
        .expect("attach runtime");

    // Blind spots 1 (cold start) and 3 (liveness): no Hook event exists, but
    // the first provider reconcile must surface both sessions immediately.
    poll_until(&mut app, |view| {
        view.live_count == 2 && view.known_count == 2
    });

    let view = app.agent_view();
    let mut activities = view
        .sessions
        .iter()
        .map(|row| row.activity)
        .collect::<Vec<_>>();
    activities.sort_by_key(|activity| match activity {
        ActivityState::Working => 0,
        ActivityState::WaitingPermission => 1,
        other => 2 + *other as u8 as usize,
    });
    assert_eq!(
        activities,
        vec![ActivityState::Working, ActivityState::WaitingPermission]
    );
    for row in &view.sessions {
        assert_eq!(row.mode, ObservationMode::LiveObserved);
        assert_eq!(row.observers.len(), 1);
        assert_eq!(
            row.observers[0],
            ObserverId::parse(HERDR_SNAPSHOT_OBSERVER_ID).expect("observer")
        );
        assert_eq!(row.freshness, ObservationFreshness::Current);
        let trace = row
            .decisions
            .iter()
            .find(|decision| decision.domain == EvidenceDomain::Activity)
            .expect("activity trace");
        assert_eq!(trace.disposition, DecisionDisposition::Degraded);
        assert_eq!(trace.authority, EvidenceAuthority::Observational);
    }

    // The cold-start sessions also flow through the metadata projection
    // (SessionRef-carrying observations upsert independently of hooks).
    let deadline = Instant::now() + Duration::from_secs(5);
    while metadata.writes().is_empty() && Instant::now() < deadline {
        app.poll_background();
        thread::sleep(Duration::from_millis(5));
    }
    let writes = metadata.writes();
    assert_eq!(writes.len(), 2, "one metadata delta per cold-start session");
    assert!(
        writes
            .iter()
            .all(|delta| delta.observer.as_str() == HERDR_SNAPSHOT_OBSERVER_ID)
    );
}

/// §10.6 optional canary (never runs in CI; `make herdr-snapshot-canary`).
///
/// Validates one real `herdr agent list` round trip against a locally running
/// Herdr server through the production environment gate: discovery, probe,
/// snapshot serving, and adapter decode of the real JSON shape. It never
/// starts, stops, configures, or writes to Herdr; the operator must export
/// `HERDR_ENV=1` and `HERDR_SOCKET_PATH` for a reachable local server (and may
/// point `HERDR_BIN_PATH` at a specific binary).
#[test]
#[ignore = "requires a locally installed/running Herdr; run via make herdr-snapshot-canary"]
fn installed_herdr_agent_list_decodes_through_the_production_gate() {
    // Hold the shared lock for the whole journey so it cannot overlap the
    // env-mutating cold-start test under `--include-ignored`. This canary
    // reads but never sets the Herdr environment.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());

    let enabled = std::env::var("HERDR_ENV").is_ok_and(|value| value == "1");
    let socket = std::env::var("HERDR_SOCKET_PATH").ok();
    assert!(
        enabled && socket.is_some_and(|value| !value.trim().is_empty()),
        "export HERDR_ENV=1 and HERDR_SOCKET_PATH for a reachable local Herdr server"
    );

    let mut provider = HerdrSnapshotProvider::from_environment()
        .expect("production gate must register the provider");
    let instances = provider
        .discover(
            &WorkspaceSelector::default(),
            ProviderDiscoveryLimits { max_instances: 32 },
            Instant::now(),
        )
        .expect("discover");
    assert_eq!(instances.len(), 1, "one local Herdr server");

    // Wait for the poll thread to capture at least one valid list.
    let deadline = Instant::now() + Duration::from_secs(15);
    let contract = loop {
        if let Ok(contract) = provider.probe(&instances[0], Instant::now()) {
            break contract;
        }
        assert!(Instant::now() < deadline, "probe never produced a contract");
        thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        contract.observer,
        ObserverId::parse(HERDR_SNAPSHOT_OBSERVER_ID).expect("observer")
    );

    let snapshot = loop {
        match provider.snapshot(
            &instances[0],
            None,
            SnapshotLimits {
                max_items: 64,
                max_total_bytes: 256 * 1024,
            },
            Instant::now(),
        ) {
            Ok(snapshot) => break snapshot,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "snapshot never served: {error:?}"
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    };

    // Every served item must decode through the real adapter without error.
    // An empty live server is a valid canary outcome (no agents right now),
    // so only decode the items that exist rather than requiring one.
    let keyer = HmacIdentityKeyer::new(SensitiveId::new(&[0x48; 32])).expect("canary keyer");
    let adapter = HerdrSnapshotAdapter::new();
    for item in snapshot.items() {
        let input = AdapterInput {
            delivery: AdapterDelivery::ProviderSnapshotItem,
            event_name: item.event_name.as_str(),
            observer_version: None,
            observed_at: item.observed_at,
            workspace: None,
            payload: item.payload.as_slice(),
        };
        // The canary only asserts the shape is decodable; Ignore outcomes
        // (e.g. an unmapped agent pane) are acceptable real-world results.
        let _ = adapter
            .decode(input, &keyer)
            .expect("a served herdr entry must decode");
    }
    provider.begin_draining();
}
