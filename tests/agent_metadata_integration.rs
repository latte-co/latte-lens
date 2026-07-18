#![cfg(feature = "agent-observability")]

mod support;

use std::{
    fs,
    time::{Duration, Instant},
};

use latte_lens::agent::*;
use support::agent::digest;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    store: FilesystemMetadataStore,
    install: InstallId,
    session: SessionRef,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let install = InstallId::from_digest(digest(1));
        let session = SessionRef::new(
            SessionKey::new(
                SubjectNamespace::parse("synthetic/agent").expect("subject"),
                install.clone(),
                AuthorityId::from_digest(digest(2)),
                digest(3),
            ),
            WorkspaceHint::from_digest(digest(4)),
        );
        let store = FilesystemMetadataStore::new(temp.path().join("state"), install.clone())
            .expect("store")
            .with_write_interval(Duration::ZERO);
        Self {
            _temp: temp,
            store,
            install,
            session,
        }
    }

    fn delta(&self, observed_at: u64) -> SessionMetadataDelta {
        SessionMetadataDelta {
            session: self.session.clone(),
            observer: ObserverId::parse("synthetic/hook").expect("observer"),
            observed_at: Timestamp::from_unix_millis(observed_at),
            discovery: Some(SessionDiscovery::DiscoveredMidSession),
            lifecycle_hint: None,
            activity_hint: None,
            event_kind: ObservationKindTag::Session,
            agents: BoundedVec::new(),
            terminal: None,
            write_class: MetadataWriteClass::Structural,
            generation: 1,
        }
    }

    fn merge(&self, delta: &SessionMetadataDelta) -> MetadataWriteOutcome {
        self.store
            .merge(delta, Instant::now() + Duration::from_secs(1))
    }

    fn load(&self) -> MetadataSnapshot {
        self.store
            .load_workspace(
                &WorkspaceSelector::new(
                    BoundedVec::try_from_vec(vec![self.session.workspace().clone()])
                        .expect("selector"),
                ),
                MetadataLoadLimits::default(),
            )
            .expect("load")
    }

    fn session_path(&self) -> std::path::PathBuf {
        self.store
            .state_root()
            .join("session-index")
            .join("installs")
            .join(self.install.digest().to_hex())
            .join("workspaces")
            .join(self.session.workspace().digest().to_hex())
            .join("sessions")
            .join(format!("{}.meta", self.session.key().stable_id().to_hex()))
    }

    fn session_lock_path(&self) -> std::path::PathBuf {
        self.store
            .state_root()
            .join("session-index")
            .join("installs")
            .join(self.install.digest().to_hex())
            .join("locks")
            .join("sessions")
            .join(format!("{}.lock", self.session.key().stable_id().to_hex()))
    }
}

#[test]
fn merge_is_monotonic_and_terminal_revival_is_explicit() {
    let fixture = Fixture::new();
    let mut terminal = fixture.delta(20);
    terminal.lifecycle_hint = Some(SessionLifecycleHint::Ended);
    terminal.terminal = Some(TerminalSummary {
        lifecycle: SessionLifecycleHint::Ended,
        observed_at: Timestamp::from_unix_millis(20),
    });
    terminal.event_kind = ObservationKindTag::Lifecycle;
    assert_eq!(fixture.merge(&terminal), MetadataWriteOutcome::Updated);

    let mut older = fixture.delta(10);
    older.activity_hint = Some(ActivityStateHint::Idle);
    assert_eq!(fixture.merge(&older), MetadataWriteOutcome::Updated);
    let record = fixture.load().sessions[0].clone();
    assert_eq!(record.first_observed_at, Timestamp::from_unix_millis(10));
    assert_eq!(record.last_observed_at, Timestamp::from_unix_millis(20));
    assert_eq!(record.lifecycle_hint, SessionLifecycleHint::Ended);
    assert!(!record.revived);

    let mut revival = fixture.delta(30);
    revival.activity_hint = Some(ActivityStateHint::Working);
    revival.event_kind = ObservationKindTag::Activity;
    assert_eq!(fixture.merge(&revival), MetadataWriteOutcome::Updated);
    let record = fixture.load().sessions[0].clone();
    assert!(record.revived);
    assert_eq!(record.last_activity_hint, ActivityStateHint::Working);

    let mut state = AgentState::new(1);
    state.bootstrap_metadata(1, fixture.load());
    assert_eq!(
        state.view().sessions[0].lifecycle,
        SessionLifecycle::Unknown
    );

    let mut failed = fixture.delta(40);
    failed.lifecycle_hint = Some(SessionLifecycleHint::Failed);
    failed.terminal = Some(TerminalSummary {
        lifecycle: SessionLifecycleHint::Failed,
        observed_at: Timestamp::from_unix_millis(40),
    });
    failed.event_kind = ObservationKindTag::Lifecycle;
    assert_eq!(fixture.merge(&failed), MetadataWriteOutcome::Updated);
    let record = fixture.load().sessions[0].clone();
    assert!(!record.revived);
    assert_eq!(
        record.terminal.expect("terminal").lifecycle,
        SessionLifecycleHint::Failed
    );
}

#[test]
fn metadata_is_bounded_private_and_contains_no_raw_canary() {
    let fixture = Fixture::new();
    let mut delta = fixture.delta(10);
    for index in 0..40_u8 {
        let key = AgentKey::new(
            fixture.session.key().clone(),
            digest(index.saturating_add(20)),
        );
        let _ = delta.agents.try_push(AgentSummaryDelta {
            agent: AgentRef::new(key, None, Some(AgentKind::Subagent)),
            observed_at: Timestamp::from_unix_millis(10),
        });
    }
    assert_eq!(
        delta.agents.len(),
        8,
        "a single projected delta stays bounded"
    );
    assert_eq!(fixture.merge(&delta), MetadataWriteOutcome::Updated);
    let path = fixture.session_path();
    let bytes = fs::read(&path).expect("metadata bytes");
    assert!(bytes.len() <= MAX_METADATA_FILE_BYTES);
    for canary in [
        b"prompt-canary".as_slice(),
        b"tool-body-canary",
        b"token-canary",
    ] {
        assert!(!bytes.windows(canary.len()).any(|window| window == canary));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().expect("parent"))
                .expect("directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn different_workspace_does_not_discover_persisted_session() {
    let fixture = Fixture::new();
    let different = WorkspaceHint::from_digest(digest(40));
    let delta = fixture.delta(10);
    assert_eq!(fixture.merge(&delta), MetadataWriteOutcome::Updated);

    let snapshot = fixture
        .store
        .load_workspace(
            &WorkspaceSelector::new(
                BoundedVec::try_from_vec(vec![different.clone()]).expect("selector"),
            ),
            MetadataLoadLimits::default(),
        )
        .expect("load different workspace");

    assert!(snapshot.sessions.is_empty());
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].workspace, different);
}

#[test]
fn stale_lock_file_and_corruption_fail_open_without_unbounded_wait() {
    let fixture = Fixture::new();
    let delta = fixture.delta(10);
    assert_eq!(fixture.merge(&delta), MetadataWriteOutcome::Updated);
    let path = fixture.session_path();
    let lock = fixture.session_lock_path();
    fs::write(&lock, []).expect("stale lock file");
    let started = Instant::now();
    assert_eq!(
        fixture
            .store
            .merge(&delta, started + Duration::from_millis(5)),
        MetadataWriteOutcome::Updated
    );
    // The deadline bounds lock contention, while Windows ACL and filesystem
    // work can be delayed by the loaded CI host. Keep a separate generous
    // wall-clock guard to catch a genuinely unbounded stale-lock wait.
    assert!(started.elapsed() < Duration::from_secs(5));

    fs::write(&path, b"prompt-canary-corrupt-record").expect("corrupt record");
    let snapshot = fixture.load();
    assert!(snapshot.sessions.is_empty());
    assert_eq!(snapshot.corrupt_records_ignored, 1);
}

#[test]
fn load_and_prune_respect_explicit_limits() {
    let fixture = Fixture::new();
    let mut old = fixture.delta(10);
    old.lifecycle_hint = Some(SessionLifecycleHint::Ended);
    old.terminal = Some(TerminalSummary {
        lifecycle: SessionLifecycleHint::Ended,
        observed_at: Timestamp::from_unix_millis(10),
    });
    assert_eq!(fixture.merge(&old), MetadataWriteOutcome::Updated);

    let limited = fixture
        .store
        .load_workspace(
            &WorkspaceSelector::new(
                BoundedVec::try_from_vec(vec![fixture.session.workspace().clone()])
                    .expect("selector"),
            ),
            MetadataLoadLimits {
                max_workspaces: 1,
                max_sessions: 0,
                max_total_bytes: 0,
            },
        )
        .expect("limited load");
    assert!(limited.sessions.is_empty());
    assert!(limited.truncated);

    let summary = fixture
        .store
        .prune(
            &RetentionPolicy {
                now: Timestamp::from_unix_millis(100),
                ended_retention_ms: 20,
                non_terminal_retention_ms: 100,
                max_sessions: 10,
            },
            MaintenanceBudget { max_records: 1 },
        )
        .expect("prune");
    assert_eq!(summary.inspected, 1);
    assert_eq!(summary.removed, 1);
    assert!(fixture.load().sessions.is_empty());
    assert!(
        !fixture
            .session_path()
            .parent()
            .expect("sessions directory")
            .parent()
            .expect("workspace directory")
            .exists(),
        "empty workspace capacity must be reclaimed"
    );
}

#[test]
fn install_identity_is_private_atomic_and_stable_across_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_root = temp.path().join("state");
    let first = load_or_create_install_identity(state_root.clone()).expect("first identity");
    let second = load_or_create_install_identity(state_root.clone()).expect("second identity");
    assert_eq!(first.install_id(), second.install_id());
    let path = state_root.join("session-index").join("install.key");
    assert_eq!(fs::metadata(&path).expect("secret metadata").len(), 32);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path)
                .expect("secret metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::write(&path, b"short-corrupt-secret").expect("corrupt secret");
    assert!(matches!(
        load_or_create_install_identity(state_root),
        Err(MetadataError::Corrupt)
    ));
}
