#![cfg(feature = "agent-observability")]

mod support;

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use latte_lens::agent::*;
use support::agent::{
    FakeAdapter, FakeIdentityKeyer, FakePublisher, InMemoryMetadataStore, digest,
};

struct Fixture {
    adapters: Arc<AdapterRegistry>,
    instances: Arc<RwLock<InstanceRegistry>>,
    workspace: WorkspaceHint,
    event: EventEnvelope,
}

struct HookAdapter {
    descriptor: ObserverDescriptor,
    template: InstanceContractTemplate,
    event: EventEnvelope,
}

struct OutcomeHookAdapter {
    descriptor: ObserverDescriptor,
    template: InstanceContractTemplate,
    outcome: Result<HookDecodeOutcome, AdapterError>,
}

impl CodeAgentAdapter for OutcomeHookAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        self.descriptor.clone()
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        self.template.clone()
    }

    fn decode(
        &self,
        input: AdapterInput<'_>,
        _identity: &dyn IdentityKeyer,
    ) -> Result<DecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        Ok(DecodeOutcome::Ignore(IgnoreReason::NoObservableFact))
    }

    fn decode_hook(
        &self,
        input: AdapterInput<'_>,
        _identity: &dyn IdentityKeyer,
    ) -> Result<HookDecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        self.outcome.clone()
    }
}

impl CodeAgentAdapter for HookAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        self.descriptor.clone()
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        self.template.clone()
    }

    fn decode(
        &self,
        input: AdapterInput<'_>,
        _identity: &dyn IdentityKeyer,
    ) -> Result<DecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        Ok(DecodeOutcome::Ignore(IgnoreReason::NoObservableFact))
    }

    fn decode_hook(
        &self,
        input: AdapterInput<'_>,
        _identity: &dyn IdentityKeyer,
    ) -> Result<HookDecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        assert_eq!(input.workspace.as_ref(), self.event_workspace());
        Ok(HookDecodeOutcome::Event(Box::new(self.event.clone())))
    }
}

impl HookAdapter {
    fn event_workspace(&self) -> Option<&WorkspaceHint> {
        let StreamOp::Upsert(observations) = &self.event.op else {
            return None;
        };
        observations
            .first()
            .and_then(|observation| observation.workspace.as_ref())
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
        let claim = CapabilityClaim {
            support: CapabilitySupport::Confirmed,
            max_authority: EvidenceAuthority::Authoritative,
            provenance: EvidenceProvenance::InstrumentedHook,
            reason: BoundedText::try_new("synthetic transport").expect("reason"),
            lease_backed: false,
        };
        let subjects = BoundedSet::try_from_iter([subject]).expect("subjects");
        let acquisition =
            BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).expect("acquisition");
        let capabilities = BTreeMap::from([(EvidenceDomain::Activity, claim)]);
        let contract = InstanceContract {
            observer: observer.clone(),
            instance: instance.clone(),
            revision: ContractRevision::new(1),
            observer_version: None,
            subjects: subjects.clone(),
            acquisition: acquisition.clone(),
            capabilities: capabilities.clone(),
            snapshot_semantics: SnapshotSemantics::unsupported(),
            stream_semantics: StreamSemantics::unsupported(),
            requires_instrumentation: true,
            stability: InterfaceStability::Stable,
        };
        let template = InstanceContractTemplate {
            observer: observer.clone(),
            subjects,
            acquisition,
            capabilities,
            snapshot_semantics: SnapshotSemantics::unsupported(),
            stream_semantics: StreamSemantics::unsupported(),
            requires_instrumentation: true,
            stability: InterfaceStability::Stable,
        };
        let observation = AgentObservation {
            observed_at: Timestamp::from_unix_millis(10),
            valid_until: None,
            presence: None,
            session: Some(session),
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
        let event = EventEnvelope {
            stream: StreamRef {
                observer: observer.clone(),
                instance: instance.clone(),
                epoch: epoch.clone(),
            },
            event_id: EventId::from_digest(digest(7)),
            sequence: Some(StreamSequence::new(1)),
            op: StreamOp::Upsert(
                BoundedVec::try_from_vec(vec![observation.clone()]).expect("observations"),
            ),
        };
        let mut adapters = AdapterRegistry::new();
        adapters
            .register(Arc::new(FakeAdapter::new(
                ObserverDescriptor::new(observer, "Synthetic", "1").expect("descriptor"),
                template,
                DecodeOutcome::Observations(
                    BoundedVec::try_from_vec(vec![observation]).expect("decode outcome"),
                ),
            )))
            .expect("adapter");
        let mut instances = InstanceRegistry::new();
        instances.upsert(contract, epoch).expect("instance");
        Self {
            adapters: Arc::new(adapters),
            instances: Arc::new(RwLock::new(instances)),
            workspace,
            event,
        }
    }

    fn install(&self) -> InstallId {
        match &self.event.op {
            StreamOp::Upsert(observations) => observations[0]
                .session
                .as_ref()
                .expect("session")
                .key()
                .install_id()
                .clone(),
            _ => unreachable!("fixture is an upsert"),
        }
    }

    fn policy(&self, workspace: WorkspaceHint) -> Arc<LiveIngressPolicy> {
        Arc::new(LiveIngressPolicy::new(
            3,
            self.install(),
            [workspace],
            Arc::clone(&self.adapters),
            Arc::clone(&self.instances),
        ))
    }
}

#[test]
fn in_memory_transport_contract_covers_acceptance_backpressure_membership_and_drain() {
    let fixture = Fixture::new();
    let (publisher, mut receiver) =
        in_memory_live_transport(1, fixture.policy(fixture.workspace.clone()));
    assert_eq!(
        publisher.publish(&fixture.event, Instant::now() + Duration::from_secs(1)),
        PublishOutcome::Accepted {
            receiver_generation: 3
        }
    );
    assert_eq!(
        publisher.publish(&fixture.event, Instant::now() + Duration::from_secs(1)),
        PublishOutcome::Busy
    );
    match receiver.receive(Instant::now() + Duration::from_secs(1)) {
        ReceiveOutcome::Event {
            receiver_generation,
            workspace_hints,
            event,
        } => {
            assert_eq!(receiver_generation, 3);
            assert_eq!(
                workspace_hints.as_ref(),
                std::slice::from_ref(&fixture.workspace)
            );
            assert_eq!(*event, fixture.event);
        }
        other => panic!("expected event, got {other:?}"),
    }
    receiver.begin_draining();
    assert_eq!(
        publisher.publish(&fixture.event, Instant::now() + Duration::from_secs(1)),
        PublishOutcome::Unavailable
    );
    assert_eq!(
        receiver.receive(Instant::now() + Duration::from_secs(1)),
        ReceiveOutcome::Closed
    );

    let (not_member, _receiver) =
        in_memory_live_transport(1, fixture.policy(WorkspaceHint::from_digest(digest(90))));
    assert_eq!(
        not_member.publish(&fixture.event, Instant::now() + Duration::from_secs(1)),
        PublishOutcome::NotMember
    );
}

#[test]
fn ingress_rejects_a_session_key_from_a_different_state_root_install() {
    let fixture = Fixture::new();
    let (publisher, mut receiver) =
        in_memory_live_transport(1, fixture.policy(fixture.workspace.clone()));
    let mut event = fixture.event.clone();
    let StreamOp::Upsert(observations) = &event.op else {
        unreachable!("fixture is an upsert");
    };
    let mut observations = observations.clone().into_vec();
    let observation = &mut observations[0];
    let current = observation.session.as_ref().expect("session");
    observation.session = Some(SessionRef::new(
        SessionKey::new(
            current.key().subject().clone(),
            InstallId::from_digest(digest(88)),
            current.key().authority_id().clone(),
            current.key().stable_id().clone(),
        ),
        current.workspace().clone(),
    ));
    event.op = StreamOp::Upsert(BoundedVec::try_from_vec(observations).expect("observations"));
    assert_eq!(
        publisher.publish(&event, Instant::now() + Duration::from_secs(1)),
        PublishOutcome::Rejected
    );
    assert_eq!(
        receiver.receive(Instant::now() + Duration::from_millis(1)),
        ReceiveOutcome::Idle
    );
}

#[test]
fn ingress_rejects_a_code_agent_started_in_a_different_workspace() {
    let fixture = Fixture::new();
    let other_workspace = WorkspaceHint::from_digest(digest(88));
    let (publisher, mut receiver) = in_memory_live_transport(1, fixture.policy(other_workspace));

    assert_eq!(
        publisher.publish(&fixture.event, Instant::now() + Duration::from_secs(1)),
        PublishOutcome::NotMember
    );
    assert_eq!(
        receiver.receive(Instant::now() + Duration::from_secs(1)),
        ReceiveOutcome::Idle
    );
}

#[test]
fn observation_frame_round_trips_and_rejects_version_and_oversize_before_decode() {
    let fixture = Fixture::new();
    let encoded = ObservationFrame::new(fixture.event.clone())
        .expect("frame shape")
        .encode()
        .expect("frame");
    assert!(encoded.len() <= MAX_LIVE_FRAME_BYTES);
    assert_eq!(
        ObservationFrame::decode(&encoded).expect("decoded").event,
        fixture.event
    );

    let mut wrong_version = encoded;
    wrong_version[4..6].copy_from_slice(&2_u16.to_be_bytes());
    assert_eq!(
        ObservationFrame::decode(&wrong_version),
        Err(FrameError::VersionMismatch)
    );
    assert_eq!(
        ObservationFrame::decode(&vec![0; MAX_LIVE_FRAME_BYTES + 1]),
        Err(FrameError::TooLarge)
    );
}

#[test]
fn generic_hook_emitter_uses_adapter_decode_and_the_shared_dispatcher() {
    let fixture = Fixture::new();
    let source_adapter = fixture
        .adapters
        .resolve(&fixture.event.stream.observer)
        .expect("source adapter");
    let mut adapters = AdapterRegistry::new();
    adapters
        .register(Arc::new(HookAdapter {
            descriptor: source_adapter.descriptor(),
            template: source_adapter.contract_template(None),
            event: fixture.event.clone(),
        }))
        .expect("hook adapter");
    let adapters = Arc::new(adapters);
    let policy = Arc::new(LiveIngressPolicy::new(
        3,
        fixture.install(),
        [fixture.workspace.clone()],
        Arc::clone(&adapters),
        Arc::clone(&fixture.instances),
    ));
    let (publisher, mut receiver) = in_memory_live_transport(1, policy);
    let metadata = InMemoryMetadataStore::default();
    let identity = FakeIdentityKeyer::new();
    let observer = fixture.event.stream.observer.clone();

    assert_eq!(
        emit_hook_invocation(
            HookInvocation {
                observer: &observer,
                event_name: "activity",
                observer_version: None,
                observed_at: Timestamp::from_unix_millis(10),
                workspace: fixture.workspace.clone(),
                payload: b"bounded synthetic payload",
            },
            &adapters,
            &identity,
            &publisher,
            &metadata,
            Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        ),
        HookEmitOutcome::Dispatched(DispatchOutcome::LiveAccepted {
            receiver_generation: 3
        })
    );
    assert!(matches!(
        receiver.receive(Instant::now() + Duration::from_secs(1)),
        ReceiveOutcome::Event { .. }
    ));
    assert!(metadata.writes().is_empty());
}

#[test]
fn hook_emitter_fails_closed_for_unknown_oversize_ignored_and_malformed_input() {
    let fixture = Fixture::new();
    let metadata = InMemoryMetadataStore::default();
    let publisher = FakePublisher::new(PublishOutcome::Unavailable);
    let identity = FakeIdentityKeyer::new();
    let observer = fixture.event.stream.observer.clone();
    let source_adapter = fixture.adapters.resolve(&observer).expect("source adapter");
    let descriptor = source_adapter.descriptor();
    let template = source_adapter.contract_template(None);
    let registry_for = |outcome| {
        let mut adapters = AdapterRegistry::new();
        adapters
            .register(Arc::new(OutcomeHookAdapter {
                descriptor: descriptor.clone(),
                template: template.clone(),
                outcome,
            }))
            .expect("hook adapter");
        adapters
    };
    let invoke = |adapters: &AdapterRegistry,
                  event_name: &str,
                  observer_version: Option<&str>,
                  payload: &[u8]| {
        emit_hook_invocation(
            HookInvocation {
                observer: &observer,
                event_name,
                observer_version,
                observed_at: Timestamp::from_unix_millis(10),
                workspace: fixture.workspace.clone(),
                payload,
            },
            adapters,
            &identity,
            &publisher,
            &metadata,
            Instant::now() + Duration::from_millis(10),
            Duration::from_millis(10),
        )
    };

    assert_eq!(
        invoke(&AdapterRegistry::new(), "activity", None, b"{}"),
        HookEmitOutcome::Ignored
    );

    let event_registry = registry_for(Ok(HookDecodeOutcome::Event(Box::new(
        fixture.event.clone(),
    ))));
    assert_eq!(
        invoke(
            &event_registry,
            "activity",
            None,
            &vec![0; MAX_ADAPTER_INPUT_BYTES + 1],
        ),
        HookEmitOutcome::Rejected
    );
    assert_eq!(
        invoke(&event_registry, "", None, b"{}"),
        HookEmitOutcome::Rejected
    );
    let oversized_version = "v".repeat(65);
    assert_eq!(
        invoke(&event_registry, "activity", Some(&oversized_version), b"{}",),
        HookEmitOutcome::Rejected
    );

    let ignored_registry = registry_for(Ok(HookDecodeOutcome::Ignore(
        IgnoreReason::NoObservableFact,
    )));
    assert_eq!(
        invoke(&ignored_registry, "activity", None, b"{}"),
        HookEmitOutcome::Ignored
    );
    let malformed_registry = registry_for(Err(AdapterError::MalformedInput));
    assert_eq!(
        invoke(&malformed_registry, "activity", None, b"{}"),
        HookEmitOutcome::Rejected
    );
    assert!(metadata.writes().is_empty());
}

#[test]
fn hook_emitter_fallback_keeps_only_the_event_workspace() {
    let fixture = Fixture::new();
    let source_adapter = fixture
        .adapters
        .resolve(&fixture.event.stream.observer)
        .expect("source adapter");
    let mut adapters = AdapterRegistry::new();
    adapters
        .register(Arc::new(HookAdapter {
            descriptor: source_adapter.descriptor(),
            template: source_adapter.contract_template(None),
            event: fixture.event.clone(),
        }))
        .expect("hook adapter");
    let metadata = InMemoryMetadataStore::default();
    let publisher = FakePublisher::new(PublishOutcome::Unavailable);
    let identity = FakeIdentityKeyer::new();
    assert_eq!(
        emit_hook_invocation(
            HookInvocation {
                observer: &fixture.event.stream.observer,
                event_name: "activity",
                observer_version: Some("1.2.3"),
                observed_at: Timestamp::from_unix_millis(10),
                workspace: fixture.workspace.clone(),
                payload: b"bounded synthetic payload",
            },
            &adapters,
            &identity,
            &publisher,
            &metadata,
            Instant::now() + Duration::from_millis(10),
            Duration::from_millis(10),
        ),
        HookEmitOutcome::Dispatched(DispatchOutcome::Metadata(MetadataWriteOutcome::Updated))
    );
    let writes = metadata.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].session.workspace(), &fixture.workspace);
}

#[cfg(unix)]
#[test]
fn unix_loopback_protects_each_unique_endpoint_with_the_same_ingress_policy() {
    use std::{fs, os::unix::fs::PermissionsExt, thread};

    let fixture = Fixture::new();
    let temp = tempfile::tempdir().expect("tempdir");
    let endpoint = temp.path().join("runtime").join("live.sock");
    let mut receiver =
        UnixLiveReceiver::bind(endpoint.clone(), fixture.policy(fixture.workspace.clone()))
            .expect("bind receiver");
    assert_eq!(
        fs::metadata(&endpoint)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(endpoint.parent().expect("parent"))
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let ready_marker = endpoint.with_extension("ready");
    let ready = fs::read_to_string(&ready_marker).expect("ready marker");
    assert!(ready.contains("LLAO 1.0 generation=3 membership="));
    assert_eq!(
        UnixLiveReceiver::bind(endpoint.clone(), fixture.policy(fixture.workspace.clone()))
            .err()
            .expect("single receiver owner")
            .kind(),
        std::io::ErrorKind::AddrInUse
    );

    let receiver_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match receiver.receive(deadline) {
                ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                outcome => return outcome,
            }
        }
    });
    let publisher = UnixLivePublisher::new(endpoint);
    assert_eq!(
        publisher.publish(&fixture.event, Instant::now() + Duration::from_secs(2)),
        PublishOutcome::Accepted {
            receiver_generation: 3
        }
    );
    match receiver_thread.join().expect("receiver thread") {
        ReceiveOutcome::Event {
            receiver_generation,
            workspace_hints,
            event,
        } => {
            assert_eq!(receiver_generation, 3);
            assert_eq!(
                workspace_hints.as_ref(),
                std::slice::from_ref(&fixture.workspace)
            );
            assert_eq!(*event, fixture.event);
        }
        other => panic!("expected event, got {other:?}"),
    }
    assert!(!ready_marker.exists());
}

#[cfg(windows)]
#[test]
fn windows_named_pipe_loopback_enforces_single_owner_and_exact_workspace_hint() {
    use std::thread;

    let fixture = Fixture::new();
    let endpoint = BoundedText::try_new(format!(
        r"\\.\pipe\latte-lens-agent-test-{}-{}",
        std::process::id(),
        fixture.event.event_id.digest().to_hex()
    ))
    .expect("bounded endpoint");
    let mut receiver =
        WindowsNamedPipeReceiver::bind(endpoint.clone(), fixture.policy(fixture.workspace.clone()))
            .expect("bind receiver");
    assert!(
        WindowsNamedPipeReceiver::bind(endpoint.clone(), fixture.policy(fixture.workspace.clone()))
            .is_err(),
        "the named pipe endpoint has one receiver owner"
    );
    let receiver_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match receiver.receive(deadline) {
                ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                outcome => return outcome,
            }
        }
    });
    let publisher = WindowsNamedPipePublisher::new(endpoint);
    assert_eq!(
        publisher.publish(&fixture.event, Instant::now() + Duration::from_secs(2)),
        PublishOutcome::Accepted {
            receiver_generation: 3
        }
    );
    match receiver_thread.join().expect("receiver thread") {
        ReceiveOutcome::Event {
            receiver_generation,
            workspace_hints,
            event,
        } => {
            assert_eq!(receiver_generation, 3);
            assert_eq!(
                workspace_hints.as_ref(),
                std::slice::from_ref(&fixture.workspace)
            );
            assert_eq!(*event, fixture.event);
        }
        other => panic!("expected event, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn receiver_registry_fans_one_event_out_to_every_matching_lens_instance() {
    use std::thread;

    let fixture = Fixture::new();
    let temp = tempfile::tempdir().expect("tempdir");
    let install = InstallId::from_digest(digest(70));
    let registry =
        FilesystemLiveReceiverRegistry::new(temp.path().to_path_buf(), install).expect("registry");
    let first = bind_registered_live_receiver(
        &registry,
        fixture.policy(fixture.workspace.clone()),
        std::slice::from_ref(&fixture.workspace),
    )
    .expect("first receiver");
    let second = bind_registered_live_receiver(
        &registry,
        fixture.policy(fixture.workspace.clone()),
        std::slice::from_ref(&fixture.workspace),
    )
    .expect("second receiver");
    assert_eq!(
        registry
            .discover_matching(std::slice::from_ref(&fixture.workspace))
            .expect("discover")
            .len(),
        2
    );

    let receive = |mut receiver: Box<dyn LiveObservationReceiver>| {
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match receiver.receive(deadline) {
                    ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                    outcome => return outcome,
                }
            }
        })
    };
    let first = receive(first);
    let second = receive(second);
    let publisher = RegistryLivePublisher::new(registry, fixture.workspace.clone());
    assert!(matches!(
        publisher.publish(&fixture.event, Instant::now() + Duration::from_secs(2)),
        PublishOutcome::Accepted { .. }
    ));

    for receiver in [first, second] {
        match receiver.join().expect("receiver thread") {
            ReceiveOutcome::Event { event, .. } => assert_eq!(*event, fixture.event),
            other => panic!("expected event, got {other:?}"),
        }
    }
}

#[cfg(unix)]
#[test]
fn unix_loopback_rejects_oversize_length_without_allocating_the_body() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        thread,
    };

    let fixture = Fixture::new();
    let temp = tempfile::tempdir().expect("tempdir");
    let endpoint = temp.path().join("runtime").join("live.sock");
    let mut receiver =
        UnixLiveReceiver::bind(endpoint.clone(), fixture.policy(fixture.workspace.clone()))
            .expect("bind receiver");
    let receiver_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match receiver.receive(deadline) {
                ReceiveOutcome::Idle if Instant::now() < deadline => thread::yield_now(),
                outcome => return outcome,
            }
        }
    });
    let mut stream = UnixStream::connect(endpoint).expect("connect");
    stream
        .write_all(&((MAX_LIVE_FRAME_BYTES + 1) as u32).to_be_bytes())
        .expect("write length");
    let mut ack = [0_u8; 9];
    stream.read_exact(&mut ack).expect("invalid ACK");
    assert_eq!(ack[0], 4);
    assert_eq!(
        receiver_thread.join().expect("receiver thread"),
        ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope)
    );
}
