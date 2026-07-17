#![cfg(feature = "agent-observability")]

mod support;

use std::{
    collections::BTreeMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use latte_lens::{agent::*, app::App};
use support::agent::{FakeAdapter, FakeIdentityKeyer, FakeProvider, InMemoryMetadataStore, digest};

struct Fixture {
    adapters: Arc<AdapterRegistry>,
    instances: Arc<RwLock<InstanceRegistry>>,
    identity: Arc<FakeIdentityKeyer>,
    session: SessionRef,
    workspace: WorkspaceHint,
    observer: ObserverId,
    instance: ObserverInstanceId,
    epoch: StreamEpoch,
    contract: InstanceContract,
    observation: AgentObservation,
}

struct CountingProvider {
    inner: FakeProvider,
    snapshots: Arc<AtomicU64>,
    drained: Arc<AtomicBool>,
}

impl ObservationProvider for CountingProvider {
    fn observer_id(&self) -> ObserverId {
        self.inner.observer_id()
    }

    fn discover(
        &mut self,
        selector: &WorkspaceSelector,
        limits: ProviderDiscoveryLimits,
        deadline: Instant,
    ) -> Result<BoundedVec<ProviderInstance, MAX_PROVIDER_INSTANCES>, ProviderError> {
        self.inner.discover(selector, limits, deadline)
    }

    fn probe(
        &mut self,
        instance: &ProviderInstance,
        deadline: Instant,
    ) -> Result<InstanceContract, ProviderError> {
        self.inner.probe(instance, deadline)
    }

    fn snapshot(
        &mut self,
        instance: &ProviderInstance,
        cursor: Option<&ProviderCursor>,
        limits: SnapshotLimits,
        deadline: Instant,
    ) -> Result<RawSnapshot, ProviderError> {
        self.snapshots.fetch_add(1, Ordering::AcqRel);
        self.inner.snapshot(instance, cursor, limits, deadline)
    }

    fn next_event(
        &mut self,
        instance: &ProviderInstance,
        deadline: Instant,
    ) -> ProviderEventOutcome {
        self.inner.next_event(instance, deadline)
    }

    fn begin_draining(&mut self) {
        self.drained.store(true, Ordering::Release);
        self.inner.begin_draining();
    }
}

impl Fixture {
    fn new() -> Self {
        let observer = ObserverId::parse("synthetic/hook").expect("observer");
        let instance = ObserverInstanceId::from_digest(digest(1));
        let epoch = StreamEpoch::from_digest(digest(2));
        let subject = SubjectNamespace::parse("synthetic/agent").expect("subject");
        let workspace = WorkspaceHint::from_digest(digest(3));
        let session = SessionRef::new(
            SessionKey::new(
                subject.clone(),
                InstallId::from_digest(digest(4)),
                AuthorityId::from_digest(digest(5)),
                digest(6),
            ),
            workspace.clone(),
        );
        let capability = |domain| {
            (
                domain,
                CapabilityClaim {
                    support: CapabilitySupport::Confirmed,
                    max_authority: EvidenceAuthority::Authoritative,
                    provenance: EvidenceProvenance::InstrumentedHook,
                    reason: BoundedText::try_new("synthetic runtime").expect("reason"),
                    lease_backed: true,
                },
            )
        };
        let capabilities = [
            EvidenceDomain::Session,
            EvidenceDomain::Lifecycle,
            EvidenceDomain::Activity,
            EvidenceDomain::Turn,
            EvidenceDomain::Tool,
            EvidenceDomain::AgentTopology,
            EvidenceDomain::Change,
            EvidenceDomain::Artifact,
        ]
        .into_iter()
        .map(capability)
        .collect::<BTreeMap<_, _>>();
        let subjects = BoundedSet::try_from_iter([subject]).expect("subjects");
        let acquisition = BoundedSet::try_from_iter([
            AcquisitionMode::HookEvent,
            AcquisitionMode::NativeSnapshot,
            AcquisitionMode::NativeEventStream,
        ])
        .expect("acquisition");
        let snapshot_semantics = SnapshotSemantics {
            supported: true,
            atomic_boundary: true,
            chunked: true,
            provides_watermark: true,
        };
        let stream_semantics = StreamSemantics {
            supported: true,
            sequenced: true,
            reports_reset: true,
            reports_gap: true,
        };
        let contract = InstanceContract {
            observer: observer.clone(),
            instance: instance.clone(),
            revision: ContractRevision::new(1),
            observer_version: None,
            subjects: subjects.clone(),
            acquisition: acquisition.clone(),
            capabilities: capabilities.clone(),
            snapshot_semantics,
            stream_semantics,
            requires_instrumentation: false,
            stability: InterfaceStability::Stable,
        };
        let template = InstanceContractTemplate {
            observer: observer.clone(),
            subjects,
            acquisition,
            capabilities,
            snapshot_semantics,
            stream_semantics,
            requires_instrumentation: false,
            stability: InterfaceStability::Stable,
        };
        let observation = AgentObservation {
            observed_at: Timestamp::from_unix_millis(10),
            valid_until: None,
            presence: None,
            session: Some(session.clone()),
            agent: None,
            turn: None,
            workspace: Some(workspace.clone()),
            kind: ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
            evidence: EvidenceClaim {
                support: CapabilitySupport::Confirmed,
                authority: EvidenceAuthority::Authoritative,
                provenance: EvidenceProvenance::InstrumentedHook,
            },
        };
        let mut adapters = AdapterRegistry::new();
        adapters
            .register(Arc::new(FakeAdapter::new(
                ObserverDescriptor::new(observer.clone(), "Synthetic", "1").expect("descriptor"),
                template,
                DecodeOutcome::Observations(
                    BoundedVec::try_from_vec(vec![observation.clone()]).expect("decode outcome"),
                ),
            )))
            .expect("adapter");
        let mut instances = InstanceRegistry::new();
        instances
            .upsert(contract.clone(), epoch.clone())
            .expect("instance");
        Self {
            adapters: Arc::new(adapters),
            instances: Arc::new(RwLock::new(instances)),
            identity: Arc::new(FakeIdentityKeyer::new()),
            session,
            workspace,
            observer,
            instance,
            epoch,
            contract,
            observation,
        }
    }

    fn event(&self, byte: u8, sequence: u64, observation: AgentObservation) -> EventEnvelope {
        EventEnvelope {
            stream: StreamRef {
                observer: self.observer.clone(),
                instance: self.instance.clone(),
                epoch: self.epoch.clone(),
            },
            event_id: EventId::from_digest(digest(byte)),
            sequence: Some(StreamSequence::new(sequence)),
            op: StreamOp::Upsert(
                BoundedVec::try_from_vec(vec![observation]).expect("observations"),
            ),
        }
    }
}

#[derive(Default)]
struct FakeClock(AtomicU64);

impl FakeClock {
    fn set(&self, millis: u64) {
        self.0.store(millis, Ordering::Release);
    }
}

impl AgentClock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.load(Ordering::Acquire))
    }
}

fn metadata_snapshot(fixture: &Fixture) -> MetadataSnapshot {
    MetadataSnapshot {
        workspaces: BoundedVec::new(),
        sessions: BoundedVec::try_from_vec(vec![SessionMetadata {
            session: fixture.session.clone(),
            observers: BoundedVec::try_from_vec(vec![fixture.observer.clone()]).expect("observers"),
            observers_truncated: false,
            discovery: SessionDiscovery::DiscoveredMidSession,
            first_observed_at: Timestamp::from_unix_millis(1),
            last_observed_at: Timestamp::from_unix_millis(2),
            lifecycle_hint: SessionLifecycleHint::Open,
            last_activity_hint: ActivityStateHint::Working,
            last_event_kind: ObservationKindTag::Activity,
            known_agents: BoundedVec::new(),
            agents_truncated: false,
            start_observed: false,
            terminal: None,
            generation: 1,
            partial: true,
            revived: false,
        }])
        .expect("sessions"),
        truncated: false,
        corrupt_records_ignored: 0,
    }
}

fn selector(fixture: &Fixture) -> WorkspaceSelector {
    WorkspaceSelector::new(
        BoundedVec::try_from_vec(vec![fixture.workspace.clone()]).expect("selector"),
    )
}

fn app_with_services(
    services: AgentRuntimeServices,
    selector: WorkspaceSelector,
) -> (tempfile::TempDir, App) {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut app = App::new(workspace.path().to_path_buf()).expect("app");
    app.attach_agent_runtime(AgentRuntime::start(services), selector)
        .expect("attach runtime");
    (workspace, app)
}

fn poll_until(app: &mut App, predicate: impl Fn(&AgentViewState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        app.poll_background();
        if predicate(app.agent_view()) {
            return;
        }
        thread::yield_now();
    }
    panic!("agent view did not converge: {:?}", app.agent_view());
}

#[test]
fn metadata_bootstrap_crosses_runtime_and_app_without_inventing_live_state() {
    let fixture = Fixture::new();
    let metadata = Arc::new(InMemoryMetadataStore::new(metadata_snapshot(&fixture)));
    let mut services = AgentRuntimeServices::new(
        Arc::clone(&fixture.adapters),
        fixture.identity.clone(),
        metadata,
    );
    services.instances = Arc::clone(&fixture.instances);
    let (_workspace, mut app) = app_with_services(services, selector(&fixture));
    poll_until(&mut app, |view| view.known_count == 1);
    let row = &app.agent_view().sessions[0];
    assert_eq!(row.mode, ObservationMode::MetadataOnly);
    assert_eq!(row.activity, ActivityState::Unknown);
    assert_eq!(row.lifecycle, SessionLifecycle::Unknown);
    assert_eq!(app.agent_view().live_count, 0);
}

#[test]
fn metadata_refresh_makes_an_event_observed_by_another_lens_visible() {
    let fixture = Fixture::new();
    let metadata = Arc::new(InMemoryMetadataStore::default());
    let mut services = AgentRuntimeServices::new(
        Arc::clone(&fixture.adapters),
        fixture.identity.clone(),
        metadata.clone(),
    );
    services.instances = Arc::clone(&fixture.instances);
    let (_workspace, mut app) = app_with_services(services, selector(&fixture));
    app.poll_background();
    assert_eq!(app.agent_view().known_count, 0);

    metadata.set_snapshot(metadata_snapshot(&fixture));
    app.set_tree_scope(latte_lens::app::TreeScope::Agents);
    poll_until(&mut app, |view| view.known_count == 1);
    assert_eq!(
        app.agent_view().sessions[0].mode,
        ObservationMode::MetadataOnly
    );
}

#[test]
fn production_runtime_schedules_bounded_default_metadata_retention() {
    let fixture = Fixture::new();
    let metadata = Arc::new(InMemoryMetadataStore::default());
    let services = AgentRuntimeServices::new(
        Arc::clone(&fixture.adapters),
        fixture.identity.clone(),
        metadata.clone(),
    );
    let (_workspace, mut app) = app_with_services(services, selector(&fixture));
    let deadline = Instant::now() + Duration::from_secs(1);
    while metadata.prunes().is_empty() && Instant::now() < deadline {
        app.poll_background();
        std::thread::yield_now();
    }
    let (policy, budget) = metadata
        .prunes()
        .into_iter()
        .next()
        .expect("startup maintenance");
    assert_eq!(
        policy.ended_retention_ms,
        DEFAULT_ENDED_RETENTION.as_millis() as u64
    );
    assert_eq!(
        policy.non_terminal_retention_ms,
        DEFAULT_NON_TERMINAL_RETENTION.as_millis() as u64
    );
    assert_eq!(policy.max_sessions, MAX_METADATA_SESSIONS);
    assert_eq!(budget.max_records, DEFAULT_MAINTENANCE_RECORD_BUDGET);
}

#[test]
fn accepted_live_ingress_reaches_view_and_expiry_fires_without_another_event() {
    let fixture = Fixture::new();
    let metadata = Arc::new(InMemoryMetadataStore::default());
    let policy = Arc::new(LiveIngressPolicy::new(
        1,
        fixture.session.key().install_id().clone(),
        [fixture.workspace.clone()],
        Arc::clone(&fixture.adapters),
        Arc::clone(&fixture.instances),
    ));
    let (publisher, receiver) = in_memory_live_transport(8, policy);
    let clock = Arc::new(FakeClock::default());
    let mut services = AgentRuntimeServices::new(
        Arc::clone(&fixture.adapters),
        fixture.identity.clone(),
        metadata.clone(),
    );
    services.instances = Arc::clone(&fixture.instances);
    services.receiver = Some(Box::new(receiver));
    services.clock = clock.clone();
    let (_workspace, mut app) = app_with_services(services, selector(&fixture));

    let mut observation = fixture.observation.clone();
    observation.valid_until = Some(Timestamp::from_unix_millis(20));
    let event = fixture.event(30, 1, observation);
    let dispatch =
        ObservationDispatcher::new(fixture.adapters.as_ref(), &publisher, metadata.as_ref())
            .dispatch_with_budget(
                event,
                &fixture.contract,
                Instant::now() + Duration::from_secs(1),
                Duration::from_millis(2),
            );
    assert_eq!(
        dispatch,
        DispatchOutcome::LiveAccepted {
            receiver_generation: 1
        }
    );
    assert!(metadata.writes().is_empty());
    poll_until(&mut app, |view| {
        view.sessions
            .first()
            .is_some_and(|row| row.activity == ActivityState::Working)
    });
    assert_eq!(
        app.agent_view().sessions[0].mode,
        ObservationMode::LiveObserved
    );
    let metadata_deadline = Instant::now() + Duration::from_secs(1);
    while metadata.writes().is_empty() && Instant::now() < metadata_deadline {
        app.poll_background();
        std::thread::yield_now();
    }
    assert_eq!(metadata.writes()[0].session.workspace(), &fixture.workspace);

    clock.set(20);
    poll_until(&mut app, |view| {
        view.sessions.first().is_some_and(|row| {
            row.activity == ActivityState::Unknown && row.freshness == ObservationFreshness::Stale
        })
    });
    assert_ne!(
        app.agent_view().sessions[0].lifecycle,
        SessionLifecycle::Ended
    );
}

#[test]
fn provider_snapshot_and_gap_cross_the_same_runtime_reducer_path() {
    let fixture = Fixture::new();
    let provider_instance = ProviderInstance {
        observer: fixture.observer.clone(),
        instance: fixture.instance.clone(),
        version: None,
        endpoint_kind: ProviderEndpointKind::LocalSocket,
        health: ProviderHealth::Available,
    };
    let raw_item = RawProviderItem {
        event_name: BoundedText::try_new("activity").expect("event name"),
        observed_at: Timestamp::from_unix_millis(10),
        payload: BoundedBytes::try_new(b"synthetic-safe-fact".to_vec()).expect("payload"),
    };
    let scope = SnapshotScope {
        workspaces: WorkspaceScope::Explicit(
            BoundedVec::try_from_vec(vec![fixture.workspace.clone()]).expect("workspaces"),
        ),
        subjects: fixture.contract.subjects.clone(),
        entity_kinds: BoundedSet::try_from_iter([ObservedEntityKind::Session]).expect("entities"),
        domains: BoundedSet::try_from_iter([EvidenceDomain::Activity]).expect("domains"),
    };
    let snapshot = RawSnapshot::try_new(
        None,
        Some(StreamSequence::new(1)),
        true,
        BoundedVec::try_from_vec(vec![raw_item]).expect("items"),
    )
    .expect("snapshot")
    .with_scope(scope);
    let snapshots = Arc::new(AtomicU64::new(0));
    let drained = Arc::new(AtomicBool::new(false));
    let provider = CountingProvider {
        inner: FakeProvider::new(
            fixture.observer.clone(),
            BoundedVec::try_from_vec(vec![provider_instance]).expect("instances"),
            fixture.contract.clone(),
            snapshot,
            [
                ProviderEventOutcome::Idle,
                ProviderEventOutcome::Gap {
                    expected: Some(StreamSequence::new(2)),
                    received: Some(StreamSequence::new(4)),
                },
            ],
        ),
        snapshots: Arc::clone(&snapshots),
        drained: Arc::clone(&drained),
    };
    let metadata = Arc::new(InMemoryMetadataStore::default());
    let mut services = AgentRuntimeServices::new(
        Arc::clone(&fixture.adapters),
        fixture.identity.clone(),
        metadata,
    );
    services.providers.push(Box::new(provider));
    let (_workspace, mut app) = app_with_services(services, selector(&fixture));
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        app.poll_background();
        if app.agent_view().sessions.first().is_some_and(|row| {
            row.gap_count >= 1 && !row.reconciling && row.activity == ActivityState::Working
        }) {
            break;
        }
        thread::yield_now();
    }
    assert!(
        snapshots.load(Ordering::Acquire) >= 2,
        "provider was not reconciled"
    );
    assert!(
        app.agent_view().sessions.first().is_some_and(|row| {
            row.gap_count >= 1 && !row.reconciling && row.activity == ActivityState::Working
        }),
        "agent view did not reconcile: {:?}",
        app.agent_view()
    );
    let row = &app.agent_view().sessions[0];
    assert_eq!(row.mode, ObservationMode::LiveObserved);
    assert!(!row.reconciling);
    assert!(row.gap_count >= 1);
    assert_eq!(row.completeness, ViewCompleteness::Complete);
    drop(app);
    assert!(drained.load(Ordering::Acquire));
}

#[test]
fn stale_generation_completion_cannot_pollute_a_new_workspace_state() {
    let fixture = Fixture::new();
    let (handle, endpoint) = agent_runtime_channel(4, 4);
    let workspace = tempfile::tempdir().expect("workspace");
    let mut app = App::new(workspace.path().to_path_buf()).expect("app");
    app.attach_agent_runtime(AgentRuntime::from_channel(handle), selector(&fixture))
        .expect("attach");
    // Drain App's select/refresh requests, then inject an old completion through
    // the exact channel consumed by App::poll_background.
    assert!(endpoint.try_request().is_some());
    assert!(endpoint.try_request().is_some());
    endpoint
        .complete(AgentRuntimeCompletion::MetadataLoaded {
            generation: 0,
            snapshot: metadata_snapshot(&fixture),
        })
        .expect("completion");
    app.poll_background();
    assert_eq!(app.agent_view().known_count, 0);
}
