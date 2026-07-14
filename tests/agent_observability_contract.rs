#![cfg(feature = "agent-observability")]

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use latte_lens::agent::*;
use support::agent::*;

struct Fixture {
    observer: ObserverId,
    instance: ObserverInstanceId,
    epoch: StreamEpoch,
    session: SessionRef,
    observation: AgentObservation,
    contract: InstanceContract,
    template: InstanceContractTemplate,
}

fn fixture(max_authority: EvidenceAuthority) -> Fixture {
    let observer = ObserverId::parse("test/observer").expect("observer");
    let instance = ObserverInstanceId::from_digest(digest(1));
    let epoch = StreamEpoch::from_digest(digest(2));
    let subject = SubjectNamespace::parse("test/agent").expect("subject");
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
    let observation = AgentObservation {
        observed_at: Timestamp::from_unix_millis(10),
        valid_until: None,
        presence: None,
        session: Some(session.clone()),
        agent: None,
        turn: None,
        workspace: Some(workspace),
        kind: ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
        evidence: EvidenceClaim {
            support: CapabilitySupport::Confirmed,
            authority: EvidenceAuthority::Authoritative,
            provenance: EvidenceProvenance::InstrumentedHook,
        },
    };
    let capability = CapabilityClaim {
        support: CapabilitySupport::Confirmed,
        max_authority,
        provenance: EvidenceProvenance::InstrumentedHook,
        reason: BoundedText::try_new("synthetic fixture").expect("reason"),
        lease_backed: false,
    };
    let capabilities = BTreeMap::from([(EvidenceDomain::Activity, capability)]);
    let subjects = BoundedSet::try_from_iter([subject.clone()]).expect("subjects");
    let acquisition =
        BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).expect("acquisition modes");
    let contract = InstanceContract {
        observer: observer.clone(),
        instance: instance.clone(),
        revision: ContractRevision::new(1),
        observer_version: None,
        subjects: subjects.clone(),
        acquisition: acquisition.clone(),
        capabilities: capabilities.clone(),
        snapshot_semantics: SnapshotSemantics::unsupported(),
        stream_semantics: StreamSemantics {
            supported: true,
            sequenced: true,
            reports_reset: true,
            reports_gap: true,
        },
        requires_instrumentation: true,
        stability: InterfaceStability::Stable,
    };
    let template = InstanceContractTemplate {
        observer: observer.clone(),
        subjects,
        acquisition,
        capabilities,
        snapshot_semantics: SnapshotSemantics::unsupported(),
        stream_semantics: contract.stream_semantics,
        requires_instrumentation: true,
        stability: InterfaceStability::Stable,
    };
    Fixture {
        observer,
        instance,
        epoch,
        session,
        observation,
        contract,
        template,
    }
}

fn event(fixture: &Fixture) -> EventEnvelope {
    EventEnvelope {
        stream: StreamRef {
            observer: fixture.observer.clone(),
            instance: fixture.instance.clone(),
            epoch: fixture.epoch.clone(),
        },
        event_id: EventId::from_digest(digest(7)),
        sequence: Some(StreamSequence::new(1)),
        op: StreamOp::Upsert(
            BoundedVec::try_from_vec(vec![fixture.observation.clone()]).expect("observations"),
        ),
    }
}

fn adapter(fixture: &Fixture) -> Arc<dyn CodeAgentAdapter> {
    Arc::new(FakeAdapter::new(
        ObserverDescriptor::new(fixture.observer.clone(), "Fake adapter", "1").expect("descriptor"),
        fixture.template.clone(),
        DecodeOutcome::Observations(
            BoundedVec::try_from_vec(vec![fixture.observation.clone()]).expect("output"),
        ),
    ))
}

#[test]
fn production_registry_is_empty_and_registry_has_no_fallback() {
    assert!(production_adapter_registry().is_empty());

    let fixture = fixture(EvidenceAuthority::Authoritative);
    let registry = AdapterRegistry::new();
    assert_eq!(
        registry.validate_envelope(
            ObservationEnvelope::Event(event(&fixture)),
            &fixture.contract,
        ),
        Err(ObservationError::UnknownObserver)
    );
}

#[test]
fn adapter_registry_rejects_duplicate_observer_ids() {
    let fixture = fixture(EvidenceAuthority::Authoritative);
    let mut registry = AdapterRegistry::new();
    registry.register(adapter(&fixture)).expect("first adapter");
    assert!(registry.register(adapter(&fixture)).is_err());
    assert_eq!(registry.len(), 1);
}

#[test]
fn instance_contract_rejects_authority_escalation() {
    let fixture = fixture(EvidenceAuthority::Observational);
    let mut registry = AdapterRegistry::new();
    registry.register(adapter(&fixture)).expect("adapter");
    assert_eq!(
        registry.validate_envelope(
            ObservationEnvelope::Event(event(&fixture)),
            &fixture.contract,
        ),
        Err(ObservationError::AuthorityExceeded)
    );
}

#[test]
fn adapter_template_prevents_probe_from_expanding_authority() {
    let mut fixture = fixture(EvidenceAuthority::Authoritative);
    fixture
        .template
        .capabilities
        .get_mut(&EvidenceDomain::Activity)
        .expect("activity template")
        .max_authority = EvidenceAuthority::Observational;
    let mut registry = AdapterRegistry::new();
    registry.register(adapter(&fixture)).expect("adapter");
    assert_eq!(
        registry.validate_envelope(
            ObservationEnvelope::Event(event(&fixture)),
            &fixture.contract,
        ),
        Err(ObservationError::UnsupportedCapability)
    );
}

#[test]
fn snapshot_items_must_be_inside_the_declared_scope() {
    let mut fixture = fixture(EvidenceAuthority::Authoritative);
    fixture
        .contract
        .acquisition
        .try_insert(AcquisitionMode::NativeSnapshot)
        .expect("snapshot mode");
    fixture.contract.snapshot_semantics = SnapshotSemantics {
        supported: true,
        atomic_boundary: true,
        chunked: true,
        provides_watermark: true,
    };
    fixture.template.acquisition = fixture.contract.acquisition.clone();
    fixture.template.snapshot_semantics = fixture.contract.snapshot_semantics;

    let mut registry = AdapterRegistry::new();
    registry.register(adapter(&fixture)).expect("adapter");
    let snapshot = SnapshotEnvelope {
        stream: event(&fixture).stream,
        snapshot_id: SnapshotId::from_digest(digest(88)),
        chunk_index: 0,
        final_chunk: true,
        captured_at: Timestamp::from_unix_millis(10),
        scope: SnapshotScope {
            workspaces: WorkspaceScope::Explicit(
                BoundedVec::try_from_vec(vec![WorkspaceHint::from_digest(digest(99))])
                    .expect("workspaces"),
            ),
            subjects: fixture.contract.subjects.clone(),
            entity_kinds: BoundedSet::try_from_iter([ObservedEntityKind::Session])
                .expect("entity kinds"),
            domains: BoundedSet::try_from_iter([EvidenceDomain::Activity]).expect("domains"),
        },
        completeness: SnapshotCompleteness::Complete,
        watermark: Some(StreamSequence::new(1)),
        observations: BoundedVec::try_from_vec(vec![fixture.observation.clone()])
            .expect("observations"),
    };
    assert_eq!(
        registry.validate_envelope(ObservationEnvelope::Snapshot(snapshot), &fixture.contract,),
        Err(ObservationError::InvalidSnapshotScope)
    );
}

#[test]
fn instance_registry_rejects_wrong_epoch_and_stale_revision() {
    let fixture = fixture(EvidenceAuthority::Authoritative);
    let mut instances = InstanceRegistry::new();
    assert_eq!(
        instances
            .upsert(fixture.contract.clone(), fixture.epoch.clone())
            .expect("insert contract"),
        ContractUpdate::Inserted
    );
    let wrong_epoch = StreamRef {
        epoch: StreamEpoch::from_digest(digest(99)),
        ..event(&fixture).stream
    };
    assert_eq!(
        instances.contract_for(&wrong_epoch),
        Err(ObservationError::WrongEpoch)
    );

    let mut stale = fixture.contract.clone();
    stale.revision = ContractRevision::new(0);
    assert_eq!(
        instances.upsert(stale, fixture.epoch),
        Err(ObservationError::StaleContractRevision)
    );
}

#[test]
fn identity_merge_requires_same_subject_and_authority() {
    let keyer = FakeIdentityKeyer::new();
    let subject = SubjectNamespace::parse("test/agent").expect("subject");
    let other_subject = SubjectNamespace::parse("test/other-agent").expect("other subject");
    let authority = AuthorityId::from_digest(digest(40));
    let other_authority = AuthorityId::from_digest(digest(41));

    let first = keyer
        .session_key(&subject, &authority, SensitiveId::new(b"native-session"))
        .expect("first key");
    let same = keyer
        .session_key(&subject, &authority, SensitiveId::new(b"native-session"))
        .expect("same key");
    let different_subject = keyer
        .session_key(
            &other_subject,
            &authority,
            SensitiveId::new(b"native-session"),
        )
        .expect("different subject key");
    let different_authority = keyer
        .session_key(
            &subject,
            &other_authority,
            SensitiveId::new(b"native-session"),
        )
        .expect("different authority key");

    assert_eq!(first, same);
    assert_ne!(first, different_subject);
    assert_ne!(first, different_authority);
}

#[test]
fn dispatcher_prefers_live_delivery_and_falls_back_to_metadata() {
    let fixture = fixture(EvidenceAuthority::Authoritative);
    let mut registry = AdapterRegistry::new();
    registry.register(adapter(&fixture)).expect("adapter");

    let live_store = InMemoryMetadataStore::default();
    let live_publisher = FakePublisher::new(PublishOutcome::Accepted {
        receiver_generation: 8,
    });
    let live = ObservationDispatcher::new(&registry, &live_publisher, &live_store).dispatch(
        event(&fixture),
        &fixture.contract,
        Instant::now(),
    );
    assert_eq!(
        live,
        DispatchOutcome::LiveAccepted {
            receiver_generation: 8
        }
    );
    assert!(live_store.writes().is_empty());
    assert_eq!(live_publisher.published().len(), 1);

    let fallback_store = InMemoryMetadataStore::default();
    let unavailable = FakePublisher::new(PublishOutcome::Unavailable);
    let fallback = ObservationDispatcher::new(&registry, &unavailable, &fallback_store).dispatch(
        event(&fixture),
        &fixture.contract,
        Instant::now(),
    );
    assert_eq!(
        fallback,
        DispatchOutcome::Metadata(MetadataWriteOutcome::Updated)
    );
    assert_eq!(fallback_store.writes().len(), 1);
    assert_eq!(fallback_store.writes()[0].session, fixture.session);
}

#[test]
fn fake_provider_exposes_only_bounded_read_results() {
    let fixture = fixture(EvidenceAuthority::Authoritative);
    let instance = ProviderInstance {
        observer: fixture.observer.clone(),
        instance: fixture.instance.clone(),
        version: None,
        endpoint_kind: ProviderEndpointKind::LocalSocket,
        health: ProviderHealth::Available,
    };
    let raw_item = RawProviderItem {
        event_name: BoundedText::try_new("activity").expect("event name"),
        observed_at: Timestamp::from_unix_millis(10),
        payload: BoundedBytes::try_new(b"{}".to_vec()).expect("payload"),
    };
    let snapshot = RawSnapshot::try_new(
        None,
        None,
        true,
        BoundedVec::try_from_vec(vec![raw_item]).expect("items"),
    )
    .expect("snapshot");
    let mut provider = FakeProvider::new(
        fixture.observer,
        BoundedVec::try_from_vec(vec![instance.clone()]).expect("instances"),
        fixture.contract,
        snapshot,
        [ProviderEventOutcome::Idle],
    );
    let discovered = provider
        .discover(
            &WorkspaceSelector::default(),
            ProviderDiscoveryLimits { max_instances: 1 },
        )
        .expect("discover");
    assert_eq!(discovered.len(), 1);
    assert_eq!(
        provider.next_event(&instance, Instant::now()),
        ProviderEventOutcome::Idle
    );
}

#[test]
fn raw_snapshot_rejects_an_aggregate_payload_over_the_hard_cap() {
    let item = RawProviderItem {
        event_name: BoundedText::try_new("activity").expect("event name"),
        observed_at: Timestamp::from_unix_millis(10),
        payload: BoundedBytes::try_new(vec![0; MAX_RAW_PAYLOAD_BYTES]).expect("payload"),
    };
    let items = BoundedVec::try_from_vec(vec![item; 5]).expect("item count");
    assert_eq!(
        RawSnapshot::try_new(None, None, false, items),
        Err(ProviderError::BoundsExceeded)
    );
}
