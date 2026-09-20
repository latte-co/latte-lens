#![cfg(feature = "agent-observability")]
//! Reducer integration for the Herdr read-only snapshot provider.
//!
//! These cases exercise `AgentState` arbitration across a direct Hook
//! observer (`anthropic/claude-code-hook`, Authoritative event evidence) and
//! the Herdr aggregated snapshot observer (`herdr/cli-snapshot`,
//! Observational current-state evidence). Envelopes are validated against
//! the real production adapter contract templates, so a claim that exceeds a
//! template fails here the same way it fails in the runtime.
//!
//! Coverage follows `docs/design/herdr-observation-provider.md` §10.4:
//! identity merge/non-merge, the §6.3 arbitration matrix (including the
//! blind-spot-2 degraded win), Observational disagreement, lease expiry to
//! `Stale`, refresh back to `Current`, complete-snapshot tombstones that
//! never touch another source's lifecycle, and snapshot-epoch convergence.
//! The subprocess/runtime-level journeys live in `agent_herdr_e2e.rs`.

mod support;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use latte_lens::agent::*;
use support::agent::{FakeAdapter, FakeIdentityKeyer, digest};

/// Synthetic second Observational provider. The §6.3 rule-3 disagreement
/// path is reserved for multiple Observational observers (Herdr today plus a
/// future provider); this template mirrors Herdr's screen-inference Activity
/// claim under a distinct observer id.
const SCREEN_OBSERVER_ID: &str = "synthetic/screen-observer";

struct Sources {
    registry: AdapterRegistry,
    keyer: FakeIdentityKeyer,
    workspace: WorkspaceHint,
    hook: Source,
    herdr: Source,
    screen: Source,
}

#[derive(Clone)]
struct Source {
    observer: ObserverId,
    instance: ObserverInstanceId,
    epoch: StreamEpoch,
    contract: InstanceContract,
}

impl Sources {
    fn new() -> Self {
        let keyer = FakeIdentityKeyer::new();
        let workspace = WorkspaceHint::from_digest(digest(3));

        let hook_template = ClaudeHookAdapter::new().contract_template(None);
        let hook = Source {
            observer: ObserverId::parse(CLAUDE_HOOK_OBSERVER_ID).expect("claude observer"),
            instance: ObserverInstanceId::from_digest(digest(10)),
            epoch: StreamEpoch::from_digest(digest(11)),
            contract: hook_template.hook_contract(
                ObserverInstanceId::from_digest(digest(10)),
                ContractRevision::new(1),
                None,
            ),
        };

        let herdr_template = HerdrSnapshotAdapter::new().contract_template(None);
        let herdr = Source {
            observer: ObserverId::parse(HERDR_SNAPSHOT_OBSERVER_ID).expect("herdr observer"),
            instance: ObserverInstanceId::from_digest(digest(20)),
            epoch: StreamEpoch::from_digest(digest(21)),
            contract: herdr_template.hook_contract(
                ObserverInstanceId::from_digest(digest(20)),
                ContractRevision::new(1),
                None,
            ),
        };

        let screen_template = screen_template();
        let screen = Source {
            observer: ObserverId::parse(SCREEN_OBSERVER_ID).expect("screen observer"),
            instance: ObserverInstanceId::from_digest(digest(30)),
            epoch: StreamEpoch::from_digest(digest(31)),
            contract: screen_template.hook_contract(
                ObserverInstanceId::from_digest(digest(30)),
                ContractRevision::new(1),
                None,
            ),
        };

        let mut registry = AdapterRegistry::new();
        registry
            .register(Arc::new(ClaudeHookAdapter::new()))
            .expect("claude registered");
        registry
            .register(Arc::new(HerdrSnapshotAdapter::new()))
            .expect("herdr registered");
        registry
            .register(Arc::new(FakeAdapter::new(
                ObserverDescriptor::new(screen.observer.clone(), "Synthetic Screen", "1")
                    .expect("descriptor"),
                screen_template,
                DecodeOutcome::Ignore(IgnoreReason::NoObservableFact),
            )))
            .expect("screen registered");

        Self {
            registry,
            keyer,
            workspace,
            hook,
            herdr,
            screen,
        }
    }

    /// A Claude session key shared by the direct Hook and the Herdr
    /// snapshot: same subject, byte-identical AuthorityId, same native id.
    fn claude_session(&self, native_id: &str) -> SessionRef {
        let subject = SubjectNamespace::parse(CLAUDE_SUBJECT_NAMESPACE).expect("claude subject");
        let key = self
            .keyer
            .session_key(
                &subject,
                &ClaudeHookAdapter::authority(),
                SensitiveId::new(native_id.as_bytes()),
            )
            .expect("session key");
        SessionRef::new(key, self.workspace.clone())
    }

    fn hook_activity(
        &self,
        session: &SessionRef,
        value: ReportedActivityState,
        observed_at: u64,
    ) -> AgentObservation {
        AgentObservation {
            observed_at: Timestamp::from_unix_millis(observed_at),
            valid_until: None,
            presence: None,
            session: Some(session.clone()),
            agent: None,
            turn: None,
            workspace: Some(self.workspace.clone()),
            kind: ObservationKind::Activity(ActivityOp::Set(value)),
            evidence: EvidenceClaim {
                support: CapabilitySupport::Partial,
                authority: EvidenceAuthority::Authoritative,
                provenance: EvidenceProvenance::InstrumentedHook,
            },
        }
    }

    fn hook_lifecycle(
        &self,
        session: &SessionRef,
        value: ReportedSessionLifecycle,
        observed_at: u64,
    ) -> AgentObservation {
        AgentObservation {
            observed_at: Timestamp::from_unix_millis(observed_at),
            valid_until: None,
            presence: None,
            session: Some(session.clone()),
            agent: None,
            turn: None,
            workspace: Some(self.workspace.clone()),
            kind: ObservationKind::Lifecycle(LifecycleOp::Set(value)),
            evidence: EvidenceClaim {
                support: CapabilitySupport::Confirmed,
                authority: EvidenceAuthority::Authoritative,
                provenance: EvidenceProvenance::InstrumentedHook,
            },
        }
    }

    /// A Herdr current-state Activity fact: Observational, screen-inferred,
    /// and lease-backed exactly as the production adapter emits it.
    fn herdr_activity(
        &self,
        session: &SessionRef,
        value: ReportedActivityState,
        observed_at: u64,
        lease_millis: Option<u64>,
    ) -> AgentObservation {
        AgentObservation {
            observed_at: Timestamp::from_unix_millis(observed_at),
            valid_until: lease_millis
                .map(|millis| Timestamp::from_unix_millis(observed_at.saturating_add(millis))),
            presence: None,
            session: Some(session.clone()),
            agent: None,
            turn: None,
            workspace: None,
            kind: ObservationKind::Activity(ActivityOp::Set(value)),
            evidence: EvidenceClaim {
                support: CapabilitySupport::Partial,
                authority: EvidenceAuthority::Observational,
                provenance: EvidenceProvenance::AggregatedScreenInference,
            },
        }
    }

    fn herdr_session_observed(&self, session: &SessionRef, observed_at: u64) -> AgentObservation {
        AgentObservation {
            observed_at: Timestamp::from_unix_millis(observed_at),
            valid_until: None,
            presence: None,
            session: Some(session.clone()),
            agent: None,
            turn: None,
            workspace: Some(self.workspace.clone()),
            kind: ObservationKind::Session(SessionOp::Observed),
            evidence: EvidenceClaim {
                support: CapabilitySupport::Partial,
                authority: EvidenceAuthority::Observational,
                provenance: EvidenceProvenance::AggregatedHookAuthority,
            },
        }
    }

    fn herdr_presence(&self, native_pane: &str, observed_at: u64) -> AgentObservation {
        let presence = self
            .keyer
            .presence_ref(
                &self.herdr.observer,
                &self.herdr.instance,
                SensitiveId::new(native_pane.as_bytes()),
                Some(&SubjectNamespace::parse(CLAUDE_SUBJECT_NAMESPACE).expect("subject")),
                Some(self.workspace.clone()),
            )
            .expect("presence ref");
        AgentObservation {
            observed_at: Timestamp::from_unix_millis(observed_at),
            valid_until: None,
            presence: Some(presence),
            session: None,
            agent: None,
            turn: None,
            workspace: Some(self.workspace.clone()),
            kind: ObservationKind::Presence(PresenceOp::Seen),
            evidence: EvidenceClaim {
                support: CapabilitySupport::Confirmed,
                authority: EvidenceAuthority::Authoritative,
                provenance: EvidenceProvenance::AggregatedHookAuthority,
            },
        }
    }

    fn hook_event(
        &self,
        source: &Source,
        byte: u8,
        sequence: Option<u64>,
        observations: Vec<AgentObservation>,
    ) -> ValidatedEnvelope {
        let event = EventEnvelope {
            stream: self.stream(source),
            event_id: EventId::from_digest(digest(byte)),
            sequence: sequence.map(StreamSequence::new),
            op: StreamOp::Upsert(
                BoundedVec::try_from_vec(observations).expect("event observations bounded"),
            ),
        };
        self.registry
            .validate_envelope(ObservationEnvelope::Event(event), &source.contract)
            .expect("validated hook event")
    }

    fn snapshot(
        &self,
        source: &Source,
        epoch: &StreamEpoch,
        byte: u8,
        completeness: SnapshotCompleteness,
        scope_domains: &[EvidenceDomain],
        observations: Vec<AgentObservation>,
    ) -> ValidatedEnvelope {
        let stream = StreamRef {
            observer: source.observer.clone(),
            instance: source.instance.clone(),
            epoch: epoch.clone(),
        };
        let mut domains = BTreeSet::new();
        domains.extend(scope_domains.iter().copied());
        let mut entity_kinds = BTreeSet::new();
        if domains.contains(&EvidenceDomain::Presence) {
            entity_kinds.insert(ObservedEntityKind::Presence);
        }
        entity_kinds.insert(ObservedEntityKind::Session);
        let scope = SnapshotScope {
            workspaces: WorkspaceScope::Selected,
            subjects: source.contract.subjects.clone(),
            entity_kinds: BoundedSet::try_from_iter(entity_kinds).expect("entity kinds bounded"),
            domains: BoundedSet::try_from_iter(domains).expect("domains bounded"),
        };
        let snapshot = SnapshotEnvelope {
            stream,
            snapshot_id: SnapshotId::from_digest(digest(byte)),
            chunk_index: 0,
            final_chunk: true,
            captured_at: Timestamp::from_unix_millis(500),
            scope,
            completeness,
            watermark: None,
            observations: BoundedVec::try_from_vec(observations)
                .expect("snapshot observations bounded"),
        };
        self.registry
            .validate_envelope(ObservationEnvelope::Snapshot(snapshot), &source.contract)
            .expect("validated snapshot")
    }

    fn herdr_complete(
        &self,
        epoch: &StreamEpoch,
        byte: u8,
        observations: Vec<AgentObservation>,
    ) -> ValidatedEnvelope {
        self.snapshot(
            &self.herdr.clone(),
            epoch,
            byte,
            SnapshotCompleteness::Complete,
            &[
                EvidenceDomain::Presence,
                EvidenceDomain::Session,
                EvidenceDomain::Activity,
            ],
            observations,
        )
    }

    fn stream(&self, source: &Source) -> StreamRef {
        StreamRef {
            observer: source.observer.clone(),
            instance: source.instance.clone(),
            epoch: source.epoch.clone(),
        }
    }
}

fn screen_template() -> InstanceContractTemplate {
    let subjects = BoundedSet::try_from_iter([
        SubjectNamespace::parse(CLAUDE_SUBJECT_NAMESPACE).expect("claude subject")
    ])
    .expect("subjects");
    let capabilities = BTreeMap::from([
        (
            EvidenceDomain::Session,
            screen_capability(
                CapabilitySupport::Partial,
                EvidenceAuthority::Observational,
                EvidenceProvenance::AggregatedHookAuthority,
            ),
        ),
        (
            EvidenceDomain::Activity,
            screen_capability(
                CapabilitySupport::Partial,
                EvidenceAuthority::Observational,
                EvidenceProvenance::AggregatedScreenInference,
            ),
        ),
    ]);
    InstanceContractTemplate {
        observer: ObserverId::parse(SCREEN_OBSERVER_ID).expect("screen observer"),
        subjects,
        acquisition: BoundedSet::try_from_iter([AcquisitionMode::AggregatedSnapshot])
            .expect("acquisition"),
        capabilities,
        snapshot_semantics: SnapshotSemantics {
            supported: true,
            atomic_boundary: true,
            chunked: true,
            provides_watermark: false,
        },
        stream_semantics: StreamSemantics::unsupported(),
        requires_instrumentation: false,
        stability: InterfaceStability::VersionedExperimental,
    }
}

fn screen_capability(
    support: CapabilitySupport,
    max_authority: EvidenceAuthority,
    provenance: EvidenceProvenance,
) -> CapabilityClaim {
    CapabilityClaim {
        support,
        max_authority,
        provenance,
        reason: BoundedText::try_new("synthetic screen observer").expect("reason"),
        lease_backed: false,
    }
}

fn only_session(state: &AgentState) -> AgentViewSession {
    state
        .view()
        .sessions
        .first()
        .expect("exactly one session")
        .clone()
}

fn activity_trace(row: &AgentViewSession) -> &DecisionTrace {
    row.decisions
        .iter()
        .find(|decision| decision.domain == EvidenceDomain::Activity)
        .expect("activity decision trace")
}

#[test]
fn herdr_snapshot_alone_cold_starts_two_sessions_without_any_hook_event() {
    // Blind spots 1 (cold start) and 3 (liveness/topology): a complete Herdr
    // snapshot present at Lens startup must surface sessions immediately.
    let sources = Sources::new();
    let mut state = AgentState::new(1);
    let first = sources.claude_session("claude-session-1");
    let second = sources.claude_session("claude-session-2");
    let observations = vec![
        sources.herdr_session_observed(&first, 100),
        sources.herdr_activity(&first, ReportedActivityState::Working, 100, None),
        sources.herdr_session_observed(&second, 100),
        sources.herdr_activity(&second, ReportedActivityState::Idle, 100, None),
    ];
    assert_eq!(
        state
            .apply_envelope(
                1,
                sources.herdr_complete(&sources.herdr.epoch, 1, observations)
            )
            .disposition,
        ApplyDisposition::Applied
    );

    let view = state.view();
    assert_eq!(view.known_count, 2, "both snapshot sessions are visible");
    assert_eq!(view.live_count, 2);
    for row in &view.sessions {
        assert_eq!(row.mode, ObservationMode::LiveObserved);
        assert_eq!(row.observers.len(), 1);
        assert_eq!(row.observers[0], sources.herdr.observer);
        assert_eq!(row.freshness, ObservationFreshness::Current);
        assert!(!row.reconciling);
        assert_eq!(row.completeness, ViewCompleteness::Complete);
    }
}

#[test]
fn herdr_and_hook_same_native_identity_merge_into_one_two_observer_session() {
    let sources = Sources::new();
    let session = sources.claude_session("shared-session");
    let mut state = AgentState::new(1);

    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            1,
            vec![
                sources.herdr_session_observed(&session, 100),
                sources.herdr_activity(&session, ReportedActivityState::Idle, 100, None),
            ],
        ),
    );
    state.apply_envelope(
        1,
        sources.hook_event(
            &sources.hook,
            2,
            Some(1),
            vec![sources.hook_activity(&session, ReportedActivityState::Working, 110)],
        ),
    );

    let view = state.view();
    assert_eq!(view.sessions.len(), 1, "same identity collapses to one row");
    let row = &view.sessions[0];
    assert_eq!(row.activity, ActivityState::Working);
    let mut observers = row.observers.iter().cloned().collect::<Vec<_>>();
    observers.sort();
    assert_eq!(
        observers,
        vec![
            sources.hook.observer.clone(),
            sources.herdr.observer.clone()
        ]
    );
    assert_eq!(row.coverage.observers.len(), 2);
}

#[test]
fn distinct_native_identities_do_not_merge() {
    let sources = Sources::new();
    let herdr_only = sources.claude_session("herdr-pane-session");
    let hook_only = sources.claude_session("direct-hook-session");
    let mut state = AgentState::new(1);

    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            1,
            vec![
                sources.herdr_session_observed(&herdr_only, 100),
                sources.herdr_activity(&herdr_only, ReportedActivityState::Idle, 100, None),
            ],
        ),
    );
    state.apply_envelope(
        1,
        sources.hook_event(
            &sources.hook,
            2,
            Some(1),
            vec![sources.hook_activity(&hook_only, ReportedActivityState::Working, 110)],
        ),
    );

    assert_eq!(state.view().sessions.len(), 2, "different ids stay apart");
}

#[test]
fn unexpired_authoritative_hook_wins_and_leaves_the_observational_value_as_competing() {
    let sources = Sources::new();
    let session = sources.claude_session("contested");
    let mut state = AgentState::new(1);

    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            1,
            vec![
                sources.herdr_session_observed(&session, 100),
                sources.herdr_activity(&session, ReportedActivityState::Idle, 100, None),
            ],
        ),
    );
    state.apply_envelope(
        1,
        sources.hook_event(
            &sources.hook,
            2,
            Some(1),
            vec![sources.hook_activity(&session, ReportedActivityState::Working, 110)],
        ),
    );

    let row = only_session(&state);
    assert_eq!(row.activity, ActivityState::Working);
    let trace = activity_trace(&row);
    assert_eq!(trace.disposition, DecisionDisposition::Applied);
    assert_eq!(trace.authority, EvidenceAuthority::Authoritative);
    assert_eq!(trace.winning_observer, Some(sources.hook.observer.clone()));
    let competing = trace
        .competing
        .iter()
        .find(|item| item.observer == sources.herdr.observer)
        .expect("herdr value retained as competing evidence");
    assert_eq!(competing.authority, EvidenceAuthority::Observational);
    assert_eq!(competing.disposition, DecisionDisposition::Suppressed);
}

#[test]
fn herdr_observation_degrades_to_a_honest_win_without_any_authoritative_evidence() {
    // Blind spot 2 acceptance: no Hook candidate at all. Each screen-inferred
    // state may win the degraded pass, and the trace must say Observational.
    for (state_value, expected) in [
        (ReportedActivityState::Working, ActivityState::Working),
        (ReportedActivityState::Idle, ActivityState::Idle),
        (
            ReportedActivityState::WaitingPermission,
            ActivityState::WaitingPermission,
        ),
    ] {
        let sources = Sources::new();
        let session = sources.claude_session("herdr-only");
        let mut state = AgentState::new(1);
        state.apply_envelope(
            1,
            sources.herdr_complete(
                &sources.herdr.epoch,
                1,
                vec![
                    sources.herdr_session_observed(&session, 100),
                    sources.herdr_activity(&session, state_value, 100, None),
                ],
            ),
        );

        let row = only_session(&state);
        assert_eq!(row.activity, expected);
        let trace = activity_trace(&row);
        assert_eq!(trace.disposition, DecisionDisposition::Degraded);
        assert_eq!(trace.authority, EvidenceAuthority::Observational);
        assert_eq!(
            trace.provenance,
            Some(EvidenceProvenance::AggregatedScreenInference)
        );
        assert_eq!(trace.winning_observer, Some(sources.herdr.observer.clone()));
        assert_eq!(trace.effective_value, DecisionValue::Activity(expected));
        assert!(trace.competing.is_empty());
        // A degraded win is not a conflict: coverage stays complete.
        assert_eq!(row.completeness, ViewCompleteness::Complete);
    }
}

#[test]
fn stale_authoritative_hook_allows_the_herdr_value_to_degrade_in() {
    // The Hook candidate's lease expires first (the row goes Unknown/Stale),
    // then the next Herdr snapshot supplies the current state and wins the
    // degraded pass. The expired-domain marker is cleared only when fresh
    // evidence arrives, so the row returns to Current on that snapshot.
    let sources = Sources::new();
    let session = sources.claude_session("stale-hook");
    let mut state = AgentState::new(1);
    let mut hook = sources.hook_activity(&session, ReportedActivityState::Working, 100);
    hook.valid_until = Some(Timestamp::from_unix_millis(140));
    let hook_result =
        state.apply_envelope(1, sources.hook_event(&sources.hook, 1, Some(1), vec![hook]));
    assert_eq!(only_session(&state).activity, ActivityState::Working);
    let expiry = hook_result
        .expiry_updates
        .first()
        .expect("hook lease scheduled")
        .key
        .clone();

    assert_eq!(
        state.expire_evidence(1, &[expiry]).disposition,
        ApplyDisposition::Applied
    );
    let expired = only_session(&state);
    assert_eq!(expired.activity, ActivityState::Unknown);
    assert_eq!(expired.freshness, ObservationFreshness::Stale);

    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            2,
            vec![
                sources.herdr_session_observed(&session, 150),
                sources.herdr_activity(&session, ReportedActivityState::Idle, 150, None),
            ],
        ),
    );

    let row = only_session(&state);
    assert_eq!(
        row.activity,
        ActivityState::Idle,
        "herdr degrades in after lease"
    );
    assert_eq!(row.freshness, ObservationFreshness::Current);
    let trace = activity_trace(&row);
    assert_eq!(trace.disposition, DecisionDisposition::Degraded);
    assert_eq!(trace.authority, EvidenceAuthority::Observational);
}

#[test]
fn expired_candidates_on_both_sides_render_unknown_and_stale() {
    let sources = Sources::new();
    let session = sources.claude_session("both-stale");
    let mut state = AgentState::new(1);

    let mut hook = sources.hook_activity(&session, ReportedActivityState::Working, 100);
    hook.valid_until = Some(Timestamp::from_unix_millis(140));
    let hook_result =
        state.apply_envelope(1, sources.hook_event(&sources.hook, 1, Some(1), vec![hook]));
    let herdr_result = state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            2,
            vec![
                sources.herdr_session_observed(&session, 100),
                sources.herdr_activity(&session, ReportedActivityState::Idle, 100, Some(40)),
            ],
        ),
    );
    let keys = hook_result
        .expiry_updates
        .iter()
        .chain(herdr_result.expiry_updates.iter())
        .map(|update| update.key.clone())
        .collect::<Vec<_>>();
    assert_eq!(keys.len(), 2);

    assert_eq!(
        state.expire_evidence(1, &keys).disposition,
        ApplyDisposition::Applied
    );
    let row = only_session(&state);
    assert_eq!(row.activity, ActivityState::Unknown);
    assert_eq!(row.freshness, ObservationFreshness::Stale);
    assert_eq!(
        activity_trace(&row).disposition,
        DecisionDisposition::Suppressed
    );
}

#[test]
fn disagreeing_observational_providers_render_unknown_without_escalating_conflict() {
    // §6.3 rule 3: Observational disagreement is Unknown with an honest
    // Observational conflict trace, but the session conflict flag stays
    // Authoritative-only, so coverage is not downgraded to Partial.
    let sources = Sources::new();
    let session = sources.claude_session("screen-split");
    let mut state = AgentState::new(1);

    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            1,
            vec![
                sources.herdr_session_observed(&session, 100),
                sources.herdr_activity(&session, ReportedActivityState::Idle, 100, None),
            ],
        ),
    );
    let screen_observation = AgentObservation {
        evidence: EvidenceClaim {
            support: CapabilitySupport::Partial,
            authority: EvidenceAuthority::Observational,
            provenance: EvidenceProvenance::AggregatedScreenInference,
        },
        ..sources.herdr_activity(&session, ReportedActivityState::Working, 110, None)
    };
    state.apply_envelope(
        1,
        sources.snapshot(
            &sources.screen,
            &sources.screen.epoch,
            2,
            SnapshotCompleteness::Complete,
            &[EvidenceDomain::Session, EvidenceDomain::Activity],
            vec![screen_observation],
        ),
    );

    let row = only_session(&state);
    assert_eq!(row.activity, ActivityState::Unknown);
    let trace = activity_trace(&row);
    assert_eq!(
        trace.disposition,
        DecisionDisposition::EqualAuthorityConflict
    );
    assert_eq!(trace.authority, EvidenceAuthority::Observational);
    assert_eq!(trace.winning_observer, None);
    assert_eq!(trace.competing.len(), 2);
    assert_eq!(
        row.completeness,
        ViewCompleteness::Complete,
        "no conflict escalation"
    );
}

#[test]
fn a_refresh_snapshot_returns_stale_activity_to_current() {
    // ObservationFreshness has only Unknown/Current/Stale: after the Herdr
    // lease expires the row goes Stale, then the next snapshot's fresher
    // candidate clears the expired-domain marker and returns it to Current.
    let sources = Sources::new();
    let session = sources.claude_session("refreshed");
    let mut state = AgentState::new(1);

    let first = state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            1,
            vec![
                sources.herdr_session_observed(&session, 100),
                sources.herdr_activity(&session, ReportedActivityState::Idle, 100, Some(40)),
            ],
        ),
    );
    assert_eq!(
        only_session(&state).freshness,
        ObservationFreshness::Current
    );
    let expiry = first
        .expiry_updates
        .first()
        .expect("herdr lease scheduled")
        .key
        .clone();

    assert_eq!(
        state.expire_evidence(1, &[expiry]).disposition,
        ApplyDisposition::Applied
    );
    let stale = only_session(&state);
    assert_eq!(stale.activity, ActivityState::Unknown);
    assert_eq!(stale.freshness, ObservationFreshness::Stale);

    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            2,
            vec![
                sources.herdr_session_observed(&session, 200),
                sources.herdr_activity(&session, ReportedActivityState::Working, 200, Some(40)),
            ],
        ),
    );
    let current = only_session(&state);
    assert_eq!(current.activity, ActivityState::Working);
    assert_eq!(current.freshness, ObservationFreshness::Current);
    assert_eq!(
        activity_trace(&current).disposition,
        DecisionDisposition::Degraded
    );
}

#[test]
fn complete_snapshot_missing_a_session_tombstones_only_its_own_activity_and_presence() {
    // A later Complete snapshot that no longer lists the pane tombstones
    // Herdr's own Activity/Presence, but must never erase the direct Hook's
    // Authoritative lifecycle evidence (Herdr does not even claim Lifecycle).
    let sources = Sources::new();
    let session = sources.claude_session("vanished-pane");
    let mut state = AgentState::new(1);

    state.apply_envelope(
        1,
        sources.hook_event(
            &sources.hook,
            1,
            Some(1),
            vec![sources.hook_lifecycle(&session, ReportedSessionLifecycle::Open, 90)],
        ),
    );
    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            2,
            vec![
                sources.herdr_presence("w1:p1", 100),
                sources.herdr_session_observed(&session, 100),
                sources.herdr_activity(&session, ReportedActivityState::Working, 100, None),
            ],
        ),
    );
    let before = only_session(&state);
    assert_eq!(before.lifecycle, SessionLifecycle::Open);
    assert_eq!(before.activity, ActivityState::Working);
    assert!(!state.view().unattributed_presences.is_empty());

    // Next complete snapshot no longer contains the pane.
    state.apply_envelope(1, sources.herdr_complete(&sources.herdr.epoch, 3, vec![]));
    let after = only_session(&state);
    assert_eq!(
        after.lifecycle,
        SessionLifecycle::Open,
        "hook lifecycle survives another source's snapshot"
    );
    assert_eq!(
        after.activity,
        ActivityState::Unknown,
        "herdr activity tombstoned"
    );
    assert!(
        state.view().unattributed_presences.is_empty(),
        "herdr presence tombstoned"
    );
}

#[test]
fn a_snapshot_on_a_new_epoch_reconciles_and_converges() {
    // Reducer side of the §4.3 two-phase reset: a snapshot carrying a new
    // StreamEpoch rotates the stream state through reconciling and the
    // complete snapshot converges it immediately (snapshots are not rejected
    // like stale-sequenced events).
    let sources = Sources::new();
    let session = sources.claude_session("reset-session");
    let mut state = AgentState::new(1);

    state.apply_envelope(
        1,
        sources.herdr_complete(
            &sources.herdr.epoch,
            1,
            vec![
                sources.herdr_session_observed(&session, 100),
                sources.herdr_activity(&session, ReportedActivityState::Idle, 100, None),
            ],
        ),
    );
    assert_eq!(only_session(&state).activity, ActivityState::Idle);

    let new_epoch = StreamEpoch::from_digest(digest(99));
    state.apply_envelope(
        1,
        sources.herdr_complete(
            &new_epoch,
            2,
            vec![
                sources.herdr_session_observed(&session, 200),
                sources.herdr_activity(&session, ReportedActivityState::Working, 200, None),
            ],
        ),
    );

    let row = only_session(&state);
    assert_eq!(row.activity, ActivityState::Working);
    assert!(
        !row.reconciling,
        "the complete snapshot converges reconciliation"
    );
    let herdr_coverage = row
        .coverage
        .observers
        .iter()
        .find(|coverage| coverage.observer == sources.herdr.observer)
        .expect("herdr coverage");
    assert_eq!(
        herdr_coverage.snapshot_completeness,
        Some(SnapshotCompleteness::Complete)
    );
}
