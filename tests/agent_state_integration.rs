#![cfg(feature = "agent-observability")]

mod support;

use std::{collections::BTreeMap, sync::Arc};

use latte_lens::agent::*;
use support::agent::{FakeAdapter, digest};

#[derive(Clone)]
struct SourceFixture {
    observer: ObserverId,
    instance: ObserverInstanceId,
    epoch: StreamEpoch,
    contract: InstanceContract,
    template: InstanceContractTemplate,
}

struct StateFixture {
    registry: AdapterRegistry,
    session: SessionRef,
    subject: SubjectNamespace,
    workspace: WorkspaceHint,
    first: SourceFixture,
    second: SourceFixture,
}

impl StateFixture {
    fn new() -> Self {
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
        let first = source("synthetic/hook", 10, &subject);
        let second = source("synthetic/provider", 20, &subject);
        let mut registry = AdapterRegistry::new();
        for source in [&first, &second] {
            registry
                .register(Arc::new(FakeAdapter::new(
                    ObserverDescriptor::new(source.observer.clone(), "Synthetic", "1")
                        .expect("descriptor"),
                    source.template.clone(),
                    DecodeOutcome::Ignore(IgnoreReason::NoObservableFact),
                )))
                .expect("unique observer");
        }
        Self {
            registry,
            session,
            subject,
            workspace,
            first,
            second,
        }
    }

    fn observation(
        &self,
        kind: ObservationKind,
        observed_at: u64,
        authority: EvidenceAuthority,
    ) -> AgentObservation {
        AgentObservation {
            observed_at: Timestamp::from_unix_millis(observed_at),
            valid_until: None,
            presence: None,
            session: Some(self.session.clone()),
            agent: None,
            turn: None,
            workspace: Some(self.workspace.clone()),
            kind,
            evidence: EvidenceClaim {
                support: CapabilitySupport::Confirmed,
                authority,
                provenance: EvidenceProvenance::InstrumentedHook,
            },
        }
    }

    fn event(
        &self,
        source: &SourceFixture,
        event_byte: u8,
        sequence: Option<u64>,
        observations: Vec<AgentObservation>,
    ) -> ValidatedEnvelope {
        let event = EventEnvelope {
            stream: stream(source),
            event_id: EventId::from_digest(digest(event_byte)),
            sequence: sequence.map(StreamSequence::new),
            op: StreamOp::Upsert(
                BoundedVec::try_from_vec(observations).expect("bounded event observations"),
            ),
        };
        self.registry
            .validate_envelope(ObservationEnvelope::Event(event), &source.contract)
            .expect("validated event")
    }

    fn operation(
        &self,
        source: &SourceFixture,
        event_byte: u8,
        sequence: Option<u64>,
        op: StreamOp,
    ) -> ValidatedEnvelope {
        self.registry
            .validate_envelope(
                ObservationEnvelope::Event(EventEnvelope {
                    stream: stream(source),
                    event_id: EventId::from_digest(digest(event_byte)),
                    sequence: sequence.map(StreamSequence::new),
                    op,
                }),
                &source.contract,
            )
            .expect("validated operation")
    }

    fn snapshot(
        &self,
        source: &SourceFixture,
        snapshot_byte: u8,
        completeness: SnapshotCompleteness,
        observations: Vec<AgentObservation>,
        watermark: Option<u64>,
    ) -> ValidatedEnvelope {
        self.registry
            .validate_envelope(
                ObservationEnvelope::Snapshot(SnapshotEnvelope {
                    stream: stream(source),
                    snapshot_id: SnapshotId::from_digest(digest(snapshot_byte)),
                    chunk_index: 0,
                    final_chunk: true,
                    captured_at: Timestamp::from_unix_millis(100),
                    scope: SnapshotScope {
                        workspaces: WorkspaceScope::Explicit(
                            BoundedVec::try_from_vec(vec![self.workspace.clone()])
                                .expect("workspace scope"),
                        ),
                        subjects: BoundedSet::try_from_iter([self.subject.clone()])
                            .expect("subject scope"),
                        entity_kinds: BoundedSet::try_from_iter([ObservedEntityKind::Session])
                            .expect("entity scope"),
                        domains: BoundedSet::try_from_iter([
                            EvidenceDomain::Lifecycle,
                            EvidenceDomain::Activity,
                        ])
                        .expect("domain scope"),
                    },
                    completeness,
                    watermark: watermark.map(StreamSequence::new),
                    observations: BoundedVec::try_from_vec(observations)
                        .expect("snapshot observations"),
                }),
                &source.contract,
            )
            .expect("validated snapshot")
    }
}

fn source(name: &str, byte: u8, subject: &SubjectNamespace) -> SourceFixture {
    let observer = ObserverId::parse(name).expect("observer");
    let capability = |domain| {
        (
            domain,
            CapabilityClaim {
                support: CapabilitySupport::Confirmed,
                max_authority: EvidenceAuthority::Authoritative,
                provenance: EvidenceProvenance::InstrumentedHook,
                reason: BoundedText::try_new("synthetic contract").expect("reason"),
                lease_backed: true,
            },
        )
    };
    let capabilities = [
        EvidenceDomain::Presence,
        EvidenceDomain::Session,
        EvidenceDomain::Lifecycle,
        EvidenceDomain::Activity,
        EvidenceDomain::Turn,
        EvidenceDomain::Permission,
        EvidenceDomain::Tool,
        EvidenceDomain::AgentTopology,
        EvidenceDomain::Change,
        EvidenceDomain::Artifact,
        EvidenceDomain::Presentation,
        EvidenceDomain::Diagnostic,
    ]
    .into_iter()
    .map(capability)
    .collect::<BTreeMap<_, _>>();
    let subjects = BoundedSet::try_from_iter([subject.clone()]).expect("subjects");
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
    SourceFixture {
        observer: observer.clone(),
        instance: ObserverInstanceId::from_digest(digest(byte)),
        epoch: StreamEpoch::from_digest(digest(byte.saturating_add(1))),
        contract: InstanceContract {
            observer: observer.clone(),
            instance: ObserverInstanceId::from_digest(digest(byte)),
            revision: ContractRevision::new(1),
            observer_version: None,
            subjects: subjects.clone(),
            acquisition: acquisition.clone(),
            capabilities: capabilities.clone(),
            snapshot_semantics,
            stream_semantics,
            requires_instrumentation: false,
            stability: InterfaceStability::Stable,
        },
        template: InstanceContractTemplate {
            observer,
            subjects,
            acquisition,
            capabilities,
            snapshot_semantics,
            stream_semantics,
            requires_instrumentation: false,
            stability: InterfaceStability::Stable,
        },
    }
}

fn stream(source: &SourceFixture) -> StreamRef {
    StreamRef {
        observer: source.observer.clone(),
        instance: source.instance.clone(),
        epoch: source.epoch.clone(),
    }
}

fn only_session(state: &AgentState) -> AgentViewSession {
    state.view().sessions.first().expect("one session").clone()
}

fn metadata_snapshot(fixture: &StateFixture, lifecycle: SessionLifecycleHint) -> MetadataSnapshot {
    MetadataSnapshot {
        workspaces: BoundedVec::new(),
        sessions: BoundedVec::try_from_vec(vec![SessionMetadata {
            session: fixture.session.clone(),
            observers: BoundedVec::try_from_vec(vec![fixture.first.observer.clone()])
                .expect("observer metadata"),
            observers_truncated: false,
            discovery: SessionDiscovery::DiscoveredMidSession,
            first_observed_at: Timestamp::from_unix_millis(10),
            last_observed_at: Timestamp::from_unix_millis(20),
            lifecycle_hint: lifecycle,
            last_activity_hint: ActivityStateHint::Working,
            last_event_kind: ObservationKindTag::Activity,
            known_agents: BoundedVec::new(),
            agents_truncated: false,
            start_observed: false,
            terminal: match lifecycle {
                SessionLifecycleHint::Ended | SessionLifecycleHint::Failed => {
                    Some(TerminalSummary {
                        lifecycle,
                        observed_at: Timestamp::from_unix_millis(20),
                    })
                }
                SessionLifecycleHint::Unknown | SessionLifecycleHint::Open => None,
            },
            generation: 1,
            partial: true,
            revived: false,
        }])
        .expect("metadata sessions"),
        truncated: false,
        corrupt_records_ignored: 0,
    }
}

#[test]
fn metadata_bootstrap_does_not_invent_live_or_working_state() {
    let fixture = StateFixture::new();
    let mut state = AgentState::new(1);
    assert_eq!(
        state
            .bootstrap_metadata(1, metadata_snapshot(&fixture, SessionLifecycleHint::Open))
            .disposition,
        ApplyDisposition::Applied
    );
    let row = only_session(&state);
    assert_eq!(row.mode, ObservationMode::MetadataOnly);
    assert_eq!(row.lifecycle, SessionLifecycle::Unknown);
    assert_eq!(row.coverage.observers.len(), 1);
    assert_eq!(row.coverage.observers[0].instance, None);
    assert_eq!(row.activity, ActivityState::Unknown);
    assert_eq!(row.freshness, ObservationFreshness::Unknown);
    assert_eq!(state.view().known_count, 1);
    assert_eq!(state.view().live_count, 0);
}

#[test]
fn any_mid_session_fact_establishes_a_live_session_without_faking_start() {
    let fixture = StateFixture::new();
    let cases = [
        ObservationKind::Session(SessionOp::Observed),
        ObservationKind::Tool(ToolOp::Started),
        ObservationKind::Permission(PermissionOp::Requested),
        ObservationKind::Turn(TurnOp::UnattributedEvidence),
        ObservationKind::Change(ChangeObservation {
            kind: ChangeKind::Modified,
        }),
    ];
    for (index, kind) in cases.into_iter().enumerate() {
        let mut state = AgentState::new(1);
        let result = state.apply_envelope(
            1,
            fixture.event(
                &fixture.first,
                30 + index as u8,
                Some(1),
                vec![fixture.observation(kind, 30, EvidenceAuthority::Observational)],
            ),
        );
        assert_eq!(
            result.disposition,
            ApplyDisposition::Applied,
            "case {index}"
        );
        let row = only_session(&state);
        assert_eq!(row.mode, ObservationMode::LiveObserved, "case {index}");
        assert_eq!(
            row.discovery,
            SessionDiscovery::DiscoveredMidSession,
            "case {index}"
        );
        assert_eq!(row.activity, ActivityState::Unknown, "case {index}");
    }

    let mut state = AgentState::new(1);
    state.apply_envelope(
        1,
        fixture.event(
            &fixture.first,
            40,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open)),
                30,
                EvidenceAuthority::Authoritative,
            )],
        ),
    );
    assert_eq!(
        only_session(&state).discovery,
        SessionDiscovery::StartConfirmed
    );
}

#[test]
fn multiple_code_agent_sessions_in_one_workspace_remain_independent() {
    let fixture = StateFixture::new();
    let second_session = SessionRef::new(
        SessionKey::new(
            fixture.subject.clone(),
            InstallId::from_digest(digest(4)),
            AuthorityId::from_digest(digest(5)),
            digest(199),
        ),
        fixture.workspace.clone(),
    );
    let first = fixture.observation(
        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
        30,
        EvidenceAuthority::Authoritative,
    );
    let mut second = fixture.observation(
        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
        31,
        EvidenceAuthority::Authoritative,
    );
    second.session = Some(second_session.clone());

    let mut state = AgentState::new(1);
    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.event(&fixture.first, 49, Some(1), vec![first, second]),
            )
            .disposition,
        ApplyDisposition::Applied
    );
    let view = state.view();
    assert_eq!(view.known_count, 2);
    assert_eq!(view.live_count, 2);
    assert_eq!(view.sessions.len(), 2);
    assert!(
        view.sessions
            .iter()
            .any(|row| row.session == second_session.key().clone())
    );
}

#[test]
fn stop_like_turn_evidence_does_not_end_session_and_expiry_only_stales_activity() {
    let fixture = StateFixture::new();
    let mut state = AgentState::new(7);
    let mut working = fixture.observation(
        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
        30,
        EvidenceAuthority::Authoritative,
    );
    working.valid_until = Some(Timestamp::from_unix_millis(40));
    let result = state.apply_envelope(7, fixture.event(&fixture.first, 50, Some(1), vec![working]));
    assert_eq!(only_session(&state).activity, ActivityState::Working);
    let expiry = result
        .expiry_updates
        .first()
        .expect("expiry update")
        .key
        .clone();

    state.apply_envelope(
        7,
        fixture.event(
            &fixture.first,
            51,
            Some(2),
            vec![fixture.observation(
                ObservationKind::Turn(TurnOp::UnattributedEvidence),
                35,
                EvidenceAuthority::Observational,
            )],
        ),
    );
    assert_ne!(only_session(&state).lifecycle, SessionLifecycle::Ended);

    assert_eq!(
        state.expire_evidence(7, &[expiry]).disposition,
        ApplyDisposition::Applied
    );
    let row = only_session(&state);
    assert_eq!(row.activity, ActivityState::Unknown);
    assert_eq!(row.freshness, ObservationFreshness::Stale);
    assert_eq!(row.lifecycle, SessionLifecycle::Unknown);
    assert_eq!(row.coverage.observers.len(), 1);
    assert_eq!(row.coverage.observers[0].stream_gap_count, 0);
}

#[test]
fn duplicate_sequence_gap_reset_and_generation_are_explicit() {
    let fixture = StateFixture::new();
    let mut state = AgentState::new(1);
    let first = fixture.event(
        &fixture.first,
        60,
        Some(1),
        vec![fixture.observation(
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
            30,
            EvidenceAuthority::Authoritative,
        )],
    );
    assert_eq!(
        state.apply_envelope(9, first.clone()).disposition,
        ApplyDisposition::WrongGeneration
    );
    assert_eq!(
        state.apply_envelope(1, first.clone()).disposition,
        ApplyDisposition::Applied
    );
    assert_eq!(
        state.apply_envelope(1, first).disposition,
        ApplyDisposition::Duplicate
    );
    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.event(
                    &fixture.first,
                    61,
                    Some(1),
                    vec![fixture.observation(
                        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                        31,
                        EvidenceAuthority::Authoritative,
                    )],
                ),
            )
            .disposition,
        ApplyDisposition::StaleSequence
    );
    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.event(
                    &fixture.first,
                    62,
                    Some(3),
                    vec![fixture.observation(
                        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                        32,
                        EvidenceAuthority::Authoritative,
                    )],
                ),
            )
            .disposition,
        ApplyDisposition::GapDetected
    );
    assert!(only_session(&state).reconciling);

    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.operation(&fixture.first, 63, Some(4), StreamOp::Reset),
            )
            .disposition,
        ApplyDisposition::GapDetected
    );
    assert!(only_session(&state).reconciling);

    state.apply_envelope(
        1,
        fixture.snapshot(
            &fixture.first,
            64,
            SnapshotCompleteness::Complete,
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                100,
                EvidenceAuthority::Authoritative,
            )],
            Some(4),
        ),
    );
    let row = only_session(&state);
    assert!(!row.reconciling);
    assert_eq!(row.activity, ActivityState::Idle);
    assert_eq!(row.coverage.observers[0].stream_gap_count, 2);
    assert_eq!(
        row.coverage.observers[0].snapshot_completeness,
        Some(SnapshotCompleteness::Complete)
    );
}

#[test]
fn epoch_rotation_unsequenced_events_and_deletes_during_reconcile_are_explicit() {
    let fixture = StateFixture::new();

    let mut state = AgentState::new(1);
    state.apply_envelope(
        1,
        fixture.event(
            &fixture.first,
            140,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                10,
                EvidenceAuthority::Authoritative,
            )],
        ),
    );
    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.event(
                    &fixture.first,
                    141,
                    None,
                    vec![fixture.observation(
                        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                        11,
                        EvidenceAuthority::Authoritative,
                    )],
                ),
            )
            .disposition,
        ApplyDisposition::UnsequencedAfterSequenced
    );

    let mut rotated = fixture.first.clone();
    rotated.epoch = StreamEpoch::from_digest(digest(142));
    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.event(
                    &rotated,
                    143,
                    Some(1),
                    vec![fixture.observation(
                        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                        12,
                        EvidenceAuthority::Authoritative,
                    )],
                ),
            )
            .disposition,
        ApplyDisposition::WrongEpoch
    );
    assert!(only_session(&state).reconciling);

    let mut state = AgentState::new(1);
    state.apply_envelope(
        1,
        fixture.event(
            &fixture.first,
            144,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                10,
                EvidenceAuthority::Authoritative,
            )],
        ),
    );
    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.event(
                    &fixture.first,
                    145,
                    Some(3),
                    vec![fixture.observation(
                        ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                        12,
                        EvidenceAuthority::Authoritative,
                    )],
                ),
            )
            .disposition,
        ApplyDisposition::GapDetected
    );
    assert_eq!(
        state
            .apply_envelope(
                1,
                fixture.operation(
                    &fixture.first,
                    146,
                    Some(4),
                    StreamOp::Delete {
                        entity: ObservedEntityKey::Session(fixture.session.key().clone()),
                        domains: BoundedSet::try_from_iter([EvidenceDomain::Activity])
                            .expect("delete domains"),
                    },
                ),
            )
            .disposition,
        ApplyDisposition::AwaitingSnapshot
    );
    assert_eq!(only_session(&state).activity, ActivityState::Idle);
}

#[test]
fn chunked_snapshot_shape_mismatch_is_a_gap_and_a_fresh_snapshot_recovers() {
    let fixture = StateFixture::new();
    let mut state = AgentState::new(1);
    let scope = SnapshotScope {
        workspaces: WorkspaceScope::Explicit(
            BoundedVec::try_from_vec(vec![fixture.workspace.clone()]).expect("workspaces"),
        ),
        subjects: BoundedSet::try_from_iter([fixture.subject.clone()]).expect("subjects"),
        entity_kinds: BoundedSet::try_from_iter([ObservedEntityKind::Session]).expect("entities"),
        domains: BoundedSet::try_from_iter([EvidenceDomain::Activity]).expect("domains"),
    };
    let chunk = |snapshot_byte, chunk_index, final_chunk| {
        fixture
            .registry
            .validate_envelope(
                ObservationEnvelope::Snapshot(SnapshotEnvelope {
                    stream: stream(&fixture.first),
                    snapshot_id: SnapshotId::from_digest(digest(snapshot_byte)),
                    chunk_index,
                    final_chunk,
                    captured_at: Timestamp::from_unix_millis(100),
                    scope: scope.clone(),
                    completeness: SnapshotCompleteness::Complete,
                    watermark: Some(StreamSequence::new(1)),
                    observations: BoundedVec::new(),
                }),
                &fixture.first.contract,
            )
            .expect("validated snapshot chunk")
    };

    assert_eq!(
        state.apply_envelope(1, chunk(150, 0, false)).disposition,
        ApplyDisposition::AwaitingSnapshot
    );
    assert_eq!(
        state.apply_envelope(1, chunk(151, 1, true)).disposition,
        ApplyDisposition::GapDetected
    );
    assert_eq!(
        state.apply_envelope(1, chunk(152, 0, true)).disposition,
        ApplyDisposition::Applied
    );
}

#[test]
fn equal_authority_conflict_is_order_independent_and_explained() {
    let fixture = StateFixture::new();
    for reverse in [false, true] {
        let mut state = AgentState::new(1);
        let working = fixture.event(
            &fixture.first,
            70,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                30,
                EvidenceAuthority::Authoritative,
            )],
        );
        let idle = fixture.event(
            &fixture.second,
            71,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                31,
                EvidenceAuthority::Authoritative,
            )],
        );
        let envelopes = if reverse {
            vec![idle, working]
        } else {
            vec![working, idle]
        };
        for envelope in envelopes {
            state.apply_envelope(1, envelope);
        }
        let row = only_session(&state);
        assert_eq!(row.activity, ActivityState::Unknown);
        assert_eq!(row.completeness, ViewCompleteness::Partial);
        assert!(row.decisions.iter().any(|decision| {
            decision.domain == EvidenceDomain::Activity
                && decision.disposition == DecisionDisposition::EqualAuthorityConflict
                && decision.competing.len() == 2
        }));
    }
}

#[test]
fn complete_snapshot_tombstones_only_its_source_and_partial_does_not() {
    let fixture = StateFixture::new();
    for completeness in [
        SnapshotCompleteness::Partial,
        SnapshotCompleteness::Truncated,
        SnapshotCompleteness::Complete,
    ] {
        let mut state = AgentState::new(1);
        state.apply_envelope(
            1,
            fixture.event(
                &fixture.first,
                80,
                Some(1),
                vec![fixture.observation(
                    ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                    30,
                    EvidenceAuthority::Authoritative,
                )],
            ),
        );
        state.apply_envelope(
            1,
            fixture.snapshot(&fixture.first, 81, completeness, vec![], Some(1)),
        );
        let expected = if completeness == SnapshotCompleteness::Complete {
            ActivityState::Unknown
        } else {
            ActivityState::Working
        };
        assert_eq!(only_session(&state).activity, expected, "{completeness:?}");
    }

    let mut state = AgentState::new(1);
    state.apply_envelope(
        1,
        fixture.event(
            &fixture.second,
            82,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                30,
                EvidenceAuthority::Authoritative,
            )],
        ),
    );
    state.apply_envelope(
        1,
        fixture.snapshot(
            &fixture.first,
            83,
            SnapshotCompleteness::Complete,
            vec![],
            Some(1),
        ),
    );
    assert_eq!(only_session(&state).activity, ActivityState::Idle);
}

#[test]
fn terminal_metadata_is_preserved_but_open_metadata_is_advisory() {
    let fixture = StateFixture::new();
    for (hint, expected) in [
        (SessionLifecycleHint::Open, SessionLifecycle::Unknown),
        (SessionLifecycleHint::Ended, SessionLifecycle::Ended),
        (SessionLifecycleHint::Failed, SessionLifecycle::Failed),
    ] {
        let mut state = AgentState::new(1);
        state.bootstrap_metadata(1, metadata_snapshot(&fixture, hint));
        assert_eq!(only_session(&state).lifecycle, expected);
    }
}

#[test]
fn agent_topology_is_bounded_and_existing_parent_information_can_be_corrected() {
    let fixture = StateFixture::new();
    let mut state = AgentState::new(1);
    let parent = AgentKey::new(fixture.session.key().clone(), digest(150));
    for batch in 0..5_u8 {
        let observations = (0..8_u8)
            .map(|offset| {
                let index = batch * 8 + offset;
                let key = AgentKey::new(
                    fixture.session.key().clone(),
                    digest(index.saturating_add(100)),
                );
                let mut observation = fixture.observation(
                    ObservationKind::Agent(AgentOp::Observed),
                    u64::from(index) + 10,
                    EvidenceAuthority::Authoritative,
                );
                observation.agent = Some(AgentRef::new(key, None, Some(AgentKind::Subagent)));
                observation
            })
            .collect();
        state.apply_envelope(
            1,
            fixture.event(
                &fixture.first,
                100 + batch,
                Some(u64::from(batch) + 1),
                observations,
            ),
        );
    }
    let row = only_session(&state);
    assert_eq!(row.known_agents, MAX_METADATA_AGENTS);
    assert_eq!(row.live_agents, MAX_METADATA_AGENTS);
    assert_eq!(row.agents.len(), MAX_METADATA_AGENTS);
    assert!(row.agents_truncated);
    assert_eq!(row.dropped_live_events, 8);
    assert_eq!(row.completeness, ViewCompleteness::Partial);

    let corrected_key = AgentKey::new(fixture.session.key().clone(), digest(100));
    let mut correction = fixture.observation(
        ObservationKind::Agent(AgentOp::Observed),
        100,
        EvidenceAuthority::Authoritative,
    );
    correction.agent = Some(AgentRef::new(
        corrected_key.clone(),
        Some(parent.clone()),
        Some(AgentKind::Subagent),
    ));
    state.apply_envelope(
        1,
        fixture.event(&fixture.first, 110, Some(6), vec![correction]),
    );
    let corrected = only_session(&state)
        .agents
        .iter()
        .find(|agent| agent.key == corrected_key)
        .expect("corrected agent")
        .clone();
    assert_eq!(corrected.parent, Some(parent));
    assert!(corrected.live);
}

#[test]
fn contract_downgrade_rearbitrates_only_the_updated_instance() {
    let fixture = StateFixture::new();
    let mut state = AgentState::new(1);
    state.apply_envelope(
        1,
        fixture.event(
            &fixture.first,
            120,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                30,
                EvidenceAuthority::Authoritative,
            )],
        ),
    );
    state.apply_envelope(
        1,
        fixture.event(
            &fixture.second,
            121,
            Some(1),
            vec![fixture.observation(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                31,
                EvidenceAuthority::Authoritative,
            )],
        ),
    );
    assert_eq!(only_session(&state).activity, ActivityState::Unknown);

    let mut downgraded = fixture.first.contract.clone();
    downgraded.revision = ContractRevision::new(2);
    downgraded
        .capabilities
        .get_mut(&EvidenceDomain::Activity)
        .expect("activity capability")
        .support = CapabilitySupport::Unsupported;
    let result = state.apply_contract_update(1, &downgraded);
    assert_eq!(result.disposition, ApplyDisposition::Applied);
    assert!(result.changed);
    let row = only_session(&state);
    assert_eq!(row.activity, ActivityState::Idle);
    assert!(row.reconciling);
}

#[test]
fn source_aware_agent_topology_survives_one_provider_downgrade() {
    let fixture = StateFixture::new();
    let agent_key = AgentKey::new(fixture.session.key().clone(), digest(180));
    let mut agent = fixture.observation(
        ObservationKind::Agent(AgentOp::Observed),
        30,
        EvidenceAuthority::Authoritative,
    );
    agent.agent = Some(AgentRef::new(
        agent_key.clone(),
        None,
        Some(AgentKind::Subagent),
    ));
    let mut state = AgentState::new(1);
    state.apply_envelope(
        1,
        fixture.event(&fixture.first, 122, Some(1), vec![agent.clone()]),
    );
    state.apply_envelope(1, fixture.event(&fixture.second, 123, Some(1), vec![agent]));

    let mut first_downgrade = fixture.first.contract.clone();
    first_downgrade.revision = ContractRevision::new(2);
    first_downgrade
        .capabilities
        .get_mut(&EvidenceDomain::AgentTopology)
        .expect("agent capability")
        .support = CapabilitySupport::Unsupported;
    state.apply_contract_update(1, &first_downgrade);
    assert!(
        only_session(&state)
            .agents
            .iter()
            .find(|agent| agent.key == agent_key)
            .expect("agent remains")
            .live
    );

    let mut second_downgrade = fixture.second.contract.clone();
    second_downgrade.revision = ContractRevision::new(2);
    second_downgrade
        .capabilities
        .get_mut(&EvidenceDomain::AgentTopology)
        .expect("agent capability")
        .support = CapabilitySupport::Unsupported;
    state.apply_contract_update(1, &second_downgrade);
    assert!(
        !only_session(&state)
            .agents
            .iter()
            .find(|agent| agent.key == agent_key)
            .expect("metadata identity remains")
            .live
    );
}
