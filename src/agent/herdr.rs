//! Herdr `agent list` snapshot adapter (read-only, stage 1).
//!
//! Decodes one `agents` array entry per [`AdapterInput`] payload into at most
//! three normalized facts (presence, session identity, activity). Identity
//! merging byte-reuses the mapped Hook adapters' authority digests so a
//! Herdr-observed session and its direct Hook evidence collapse onto one
//! session key (`docs/design/herdr-observation-provider.md` §5.2/§5.3).

use std::collections::BTreeMap;

use super::hook_json::{
    HookJsonParser, append_identity_part, set_optional_once, set_required_once,
};
use super::{
    AcquisitionMode, ActivityOp, AdapterDelivery, AdapterError, AdapterInput, AgentObservation,
    AuthorityId, BoundedSet, BoundedText, BoundedVec, CLAUDE_SUBJECT_NAMESPACE,
    CODEX_SUBJECT_NAMESPACE, CapabilityClaim, CapabilitySupport, ClaudeHookAdapter,
    CodeAgentAdapter, CodexHookAdapter, DecodeOutcome, EvidenceAuthority, EvidenceClaim,
    EvidenceDomain, EvidenceProvenance, IdentityKeyer, IgnoreReason, InstanceContractTemplate,
    InterfaceStability, OPENCODE_SUBJECT_NAMESPACE, ObservationKind, ObserverDescriptor,
    ObserverId, ObserverInstanceId, OpenCodePluginAdapter, PresenceOp, ReportedActivityState,
    SensitiveId, SensitiveWorkspaceLocator, SessionOp, SessionRef, SnapshotSemantics,
    StreamSemantics, SubjectNamespace, Timestamp, stable_hash,
};

pub const HERDR_SNAPSHOT_OBSERVER_ID: &str = "herdr/cli-snapshot";
pub const HERDR_ENTRY_EVENT: &str = "agent.list.entry";
const ADAPTER_VERSION: &str = "1";

/// Activity lease served with every Herdr `Activity::Set` fact.
///
/// The reducer only receives provider snapshots when `AgentRuntime` re-pulls
/// them on its 30s reprobe cadence, so the lease must span one pull-to-pull
/// interval plus one tolerance round. Herdr's internal poll cadence governs
/// provider cache freshness, not delivery; a shorter lease would flap healthy
/// candidates to `Stale` between runtime pulls.
pub const HERDR_ACTIVITY_LEASE_MILLIS: u64 = 60_000;

/// Stable instance identity for one local Herdr server socket.
pub(crate) fn herdr_instance_id(socket_path: &str) -> ObserverInstanceId {
    ObserverInstanceId::from_digest(stable_hash(
        b"herdr-cli-instance",
        &[socket_path.as_bytes()],
    ))
}

fn observer() -> ObserverId {
    ObserverId::parse(HERDR_SNAPSHOT_OBSERVER_ID).expect("static Herdr observer id")
}

/// Instance identity derived from the process environment, or `None` when the
/// Herdr gating env is absent. Reading the socket path here is the only
/// channel the provider-path adapter has to the server identity; the raw path
/// never leaves this module.
fn current_instance() -> Option<ObserverInstanceId> {
    let path = std::env::var("HERDR_SOCKET_PATH").ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| herdr_instance_id(path))
}

/// One conservative `agent` -> SubjectNamespace mapping.
struct MappedSubject {
    namespace: SubjectNamespace,
    authority: AuthorityId,
}

/// Declarative mapping table (design §5.2). Adding an entry requires a
/// verification record that Herdr's native id and the direct Hook's native id
/// belong to the same product identity system.
fn mapped_subject(agent: &str) -> Option<MappedSubject> {
    let (namespace, authority) = match agent {
        "claude" => (CLAUDE_SUBJECT_NAMESPACE, ClaudeHookAdapter::authority()),
        "codex" => (CODEX_SUBJECT_NAMESPACE, CodexHookAdapter::authority()),
        "opencode" => (
            OPENCODE_SUBJECT_NAMESPACE,
            OpenCodePluginAdapter::authority(),
        ),
        _ => return None,
    };
    Some(MappedSubject {
        namespace: SubjectNamespace::parse(namespace).expect("static mapped namespace"),
        authority,
    })
}

fn capability(
    support: CapabilitySupport,
    max_authority: EvidenceAuthority,
    provenance: EvidenceProvenance,
    reason: &'static str,
) -> CapabilityClaim {
    CapabilityClaim {
        support,
        max_authority,
        provenance,
        reason: BoundedText::try_new(reason).expect("static Herdr capability reason"),
        lease_backed: false,
    }
}

/// One parsed `agents` array entry. Only identity-bearing and scope-bearing
/// fields are read; `terminal_title`, `focused`, `foreground_cwd`, and every
/// other payload field are skipped without allocation.
pub(crate) struct HerdrEntry {
    agent: Option<String>,
    session_kind: Option<String>,
    session_value: Option<String>,
    agent_status: Option<String>,
    cwd: Option<String>,
    pane_id: String,
    /// Parsed only to reject malformed entries; the adapter itself never
    /// derives a fact from it.
    state_change_seq: u64,
}

impl HerdrEntry {
    /// Native session id when the entry carries `agent_session.kind == "id"`.
    /// Non-`id` kinds (path-like identifiers) are not identity evidence.
    fn native_session_id(&self) -> Option<&str> {
        (self.session_kind.as_deref() == Some("id"))
            .then_some(self.session_value.as_deref())
            .flatten()
            .filter(|value| !value.is_empty())
    }

    fn activity(&self) -> Option<ReportedActivityState> {
        match self.agent_status.as_deref() {
            Some("working") => Some(ReportedActivityState::Working),
            Some("idle") => Some(ReportedActivityState::Idle),
            Some("blocked") => Some(ReportedActivityState::WaitingPermission),
            // `done`/`unknown` and any unrecognized status produce no
            // activity op; absence is not a clear (design §5.4).
            _ => None,
        }
    }

    /// Parse one entry, or `Ok(None)` when it carries no stable pane
    /// identity and therefore cannot anchor any fact.
    fn parse(bytes: &[u8]) -> Result<Option<Self>, AdapterError> {
        let mut entry = HerdrEntry {
            agent: None,
            session_kind: None,
            session_value: None,
            agent_status: None,
            cwd: None,
            pane_id: String::new(),
            state_change_seq: 0,
        };
        let mut pane_seen = false;
        let mut seq_seen = false;
        let mut parser = HookJsonParser::new(bytes);
        parser.parse_object(|parser, key| match key {
            "agent" => set_optional_once(&mut entry.agent, parser.parse_bounded_string()),
            "agent_session" => parser.parse_object(|parser, key| match key {
                "kind" => set_optional_once(&mut entry.session_kind, parser.parse_bounded_string()),
                "value" => {
                    set_optional_once(&mut entry.session_value, parser.parse_bounded_string())
                }
                _ => parser.skip_value(1),
            }),
            "agent_status" => {
                set_optional_once(&mut entry.agent_status, parser.parse_bounded_string())
            }
            "cwd" => set_optional_once(&mut entry.cwd, parser.parse_bounded_string()),
            "pane_id" => set_required_once_marker(&mut entry.pane_id, &mut pane_seen, parser),
            "state_change_seq" => {
                if seq_seen {
                    return Err(AdapterError::MalformedInput);
                }
                seq_seen = true;
                entry.state_change_seq = parser.parse_bounded_u64()?;
                Ok(())
            }
            _ => parser.skip_value(1),
        })?;
        parser.finish()?;
        if !pane_seen || !seq_seen || entry.pane_id.is_empty() {
            return Ok(None);
        }
        Ok(Some(entry))
    }
}

fn set_required_once_marker(
    target: &mut String,
    seen: &mut bool,
    parser: &mut HookJsonParser<'_>,
) -> Result<(), AdapterError> {
    set_required_once(target, seen, parser.parse_bounded_string())
}

/// Read-only snapshot adapter for the local Herdr CLI.
pub struct HerdrSnapshotAdapter;

impl HerdrSnapshotAdapter {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for HerdrSnapshotAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeAgentAdapter for HerdrSnapshotAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        ObserverDescriptor::new(observer(), "Herdr", ADAPTER_VERSION)
            .expect("static Herdr observer descriptor")
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        let subjects = BoundedSet::try_from_iter([
            SubjectNamespace::parse(CLAUDE_SUBJECT_NAMESPACE).expect("claude namespace"),
            SubjectNamespace::parse(CODEX_SUBJECT_NAMESPACE).expect("codex namespace"),
            SubjectNamespace::parse(OPENCODE_SUBJECT_NAMESPACE).expect("opencode namespace"),
        ])
        .expect("three Herdr subjects");
        let acquisition =
            BoundedSet::try_from_iter([AcquisitionMode::AggregatedSnapshot]).expect("one mode");
        let capabilities = BTreeMap::from([
            (
                EvidenceDomain::Presence,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                    EvidenceProvenance::AggregatedHookAuthority,
                    "Pane binding authoritative for this Herdr server's agent panes only",
                ),
            ),
            (
                EvidenceDomain::Session,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Observational,
                    EvidenceProvenance::AggregatedHookAuthority,
                    "Native ids reported by Herdr's own integrations; merges with direct hook identity",
                ),
            ),
            (
                EvidenceDomain::Activity,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Observational,
                    EvidenceProvenance::AggregatedScreenInference,
                    "Status is screen-inferred current state without per-domain proof",
                ),
            ),
        ]);
        InstanceContractTemplate {
            observer: observer(),
            subjects,
            acquisition,
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

    fn decode(
        &self,
        input: AdapterInput<'_>,
        identity: &dyn IdentityKeyer,
    ) -> Result<DecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        if input.delivery != AdapterDelivery::ProviderSnapshotItem
            || input.event_name != HERDR_ENTRY_EVENT
        {
            return Ok(DecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent));
        }
        let entry = match HerdrEntry::parse(input.payload) {
            Ok(Some(entry)) => entry,
            // An entry without a stable pane identity is dropped, not fatal
            // to the whole snapshot (design §8: 单条目丢弃).
            Ok(None) => return Ok(DecodeOutcome::Ignore(IgnoreReason::MissingStableIdentity)),
            Err(error) => return Err(error),
        };
        let Some(instance) = current_instance() else {
            return Ok(DecodeOutcome::Ignore(IgnoreReason::MissingStableIdentity));
        };
        let mapped = entry.agent.as_deref().and_then(mapped_subject);
        let workspace = entry
            .cwd
            .as_deref()
            .map(|cwd| {
                identity
                    .workspace_hint(SensitiveWorkspaceLocator::new(cwd.as_bytes()))
                    .map_err(|_| AdapterError::IdentityRejected)
            })
            .transpose()?;

        let mut presence_native =
            Vec::with_capacity(instance.digest().as_bytes().len() + entry.pane_id.len() + 16);
        append_identity_part(&mut presence_native, instance.digest().as_bytes());
        append_identity_part(&mut presence_native, entry.pane_id.as_bytes());
        let presence = identity
            .presence_ref(
                &observer(),
                &instance,
                SensitiveId::new(&presence_native),
                mapped.as_ref().map(|subject| &subject.namespace),
                workspace.clone(),
            )
            .map_err(|_| AdapterError::IdentityRejected)?;

        let mut facts = BoundedVec::new();
        let lease = Timestamp::from_unix_millis(
            input
                .observed_at
                .as_unix_millis()
                .saturating_add(HERDR_ACTIVITY_LEASE_MILLIS),
        );
        facts
            .try_push(AgentObservation {
                observed_at: input.observed_at,
                valid_until: None,
                presence: Some(presence),
                session: None,
                agent: None,
                turn: None,
                workspace: workspace.clone(),
                kind: ObservationKind::Presence(PresenceOp::Seen),
                evidence: EvidenceClaim {
                    support: CapabilitySupport::Confirmed,
                    authority: EvidenceAuthority::Authoritative,
                    provenance: EvidenceProvenance::AggregatedHookAuthority,
                },
            })
            .expect("presence fact fits");

        // Session and activity facts additionally require a mapped subject,
        // an `id`-kind native session id, and a workspace hint derived from
        // the entry cwd (a session ref is workspace-scoped).
        if let (Some(mapped), Some(workspace), Some(native_session)) =
            (&mapped, workspace, entry.native_session_id())
        {
            let key = identity
                .session_key(
                    &mapped.namespace,
                    &mapped.authority,
                    SensitiveId::new(native_session.as_bytes()),
                )
                .map_err(|_| AdapterError::IdentityRejected)?;
            let session = SessionRef::new(key, workspace.clone());
            facts
                .try_push(AgentObservation {
                    observed_at: input.observed_at,
                    valid_until: None,
                    presence: None,
                    session: Some(session.clone()),
                    agent: None,
                    turn: None,
                    workspace: Some(workspace),
                    kind: ObservationKind::Session(SessionOp::Observed),
                    evidence: EvidenceClaim {
                        support: CapabilitySupport::Partial,
                        authority: EvidenceAuthority::Observational,
                        provenance: EvidenceProvenance::AggregatedHookAuthority,
                    },
                })
                .expect("session fact fits");
            if let Some(status) = entry.activity() {
                facts
                    .try_push(AgentObservation {
                        observed_at: input.observed_at,
                        valid_until: Some(lease),
                        presence: None,
                        session: Some(session),
                        agent: None,
                        turn: None,
                        workspace: None,
                        kind: ObservationKind::Activity(ActivityOp::Set(status)),
                        evidence: EvidenceClaim {
                            support: CapabilitySupport::Partial,
                            authority: EvidenceAuthority::Observational,
                            provenance: EvidenceProvenance::AggregatedScreenInference,
                        },
                    })
                    .expect("activity fact fits");
            }
        }
        if facts.is_empty() {
            return Ok(DecodeOutcome::Ignore(IgnoreReason::NoObservableFact));
        }
        Ok(DecodeOutcome::Observations(facts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, MutexGuard};

    use crate::agent::{
        AdapterRegistry, CLAUDE_SUBJECT_NAMESPACE, CODEX_SUBJECT_NAMESPACE, ContractRevision,
        HmacIdentityKeyer, OPENCODE_SUBJECT_NAMESPACE, ObservationEnvelope, ObservedEntityKind,
        SnapshotCompleteness, SnapshotEnvelope, SnapshotId, SnapshotScope, StableDigest,
        StreamEpoch, StreamRef, WorkspaceScope,
    };

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Holds `ENV_LOCK` for the guard's lifetime. While alive, call only
    /// `*_locked` helpers — re-entering `EnvGuard` on the same thread
    /// deadlocks the test binary (`std::sync::Mutex` is not reentrant).
    struct EnvGuard<'a> {
        _guard: MutexGuard<'a, ()>,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvGuard<'_> {
        fn set(socket_path: &str) -> Self {
            Self::swap(Some(socket_path))
        }

        fn clear() -> Self {
            Self::swap(None)
        }

        fn swap(value: Option<&str>) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
            let saved = std::env::var_os("HERDR_SOCKET_PATH");
            // SAFETY: tests touching process-global env are serialized on ENV_LOCK.
            unsafe {
                match value {
                    Some(path) => std::env::set_var("HERDR_SOCKET_PATH", path),
                    None => std::env::remove_var("HERDR_SOCKET_PATH"),
                }
            }
            Self {
                _guard: guard,
                saved,
            }
        }
    }

    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            // SAFETY: serialized on ENV_LOCK.
            unsafe {
                match &self.saved {
                    Some(value) => std::env::set_var("HERDR_SOCKET_PATH", value),
                    None => std::env::remove_var("HERDR_SOCKET_PATH"),
                }
            }
        }
    }

    const SOCKET: &str = "/tmp/herdr-adapter-test.sock";
    const RAW_CWD: &str = "/raw/cwd/canary-9f1c";
    const RAW_SESSION: &str = "3aa69e02-a2ab-4f63-9398-e70f05fe5182";
    const RAW_TITLE: &str = "title-canary-7d31";

    fn entry(agent: &str, status: &str, kind: &str, value: &str) -> String {
        format!(
            "{{\"agent\":\"{agent}\",\
             \"agent_session\":{{\"agent\":\"{agent}\",\"kind\":\"{kind}\",\
             \"source\":\"herdr:{agent}\",\"value\":\"{value}\"}},\
             \"agent_status\":\"{status}\",\"cwd\":\"{RAW_CWD}\",\"focused\":false,\
             \"pane_id\":\"w2A:p1\",\"revision\":12,\"state_change_seq\":125,\
             \"tab_id\":\"w2A:t1\",\"terminal_id\":\"term_canary\",\
             \"terminal_title\":\"{RAW_TITLE}\",\"workspace_id\":\"w2A\"}}"
        )
    }

    fn input(payload: &str) -> AdapterInput<'_> {
        AdapterInput {
            delivery: AdapterDelivery::ProviderSnapshotItem,
            event_name: HERDR_ENTRY_EVENT,
            observer_version: Some("0.9.1"),
            observed_at: Timestamp::from_unix_millis(1_000),
            workspace: None,
            payload: payload.as_bytes(),
        }
    }

    fn keyer() -> HmacIdentityKeyer {
        HmacIdentityKeyer::new(SensitiveId::new(&[0x48; 32])).expect("keyer")
    }

    /// Decode assuming the caller already holds `ENV_LOCK` (via `EnvGuard`)
    /// and has `HERDR_SOCKET_PATH` set. Never locks `ENV_LOCK` itself.
    fn decode_locked(payload: &str) -> DecodeOutcome {
        HerdrSnapshotAdapter::new()
            .decode(input(payload), &keyer())
            .expect("decode")
    }

    fn decode(payload: &str) -> DecodeOutcome {
        let _env = EnvGuard::set(SOCKET);
        decode_locked(payload)
    }

    fn facts_locked(payload: &str) -> Vec<AgentObservation> {
        match decode_locked(payload) {
            DecodeOutcome::Observations(facts) => facts.into_vec(),
            other => panic!("expected observations, got {other:?}"),
        }
    }

    fn facts(payload: &str) -> Vec<AgentObservation> {
        match decode(payload) {
            DecodeOutcome::Observations(facts) => facts.into_vec(),
            other => panic!("expected observations, got {other:?}"),
        }
    }

    #[test]
    fn mapped_working_entry_produces_presence_session_and_activity() {
        let facts = facts(&entry("claude", "working", "id", RAW_SESSION));
        assert_eq!(facts.len(), 3);
        assert!(matches!(
            facts[0].kind,
            ObservationKind::Presence(PresenceOp::Seen)
        ));
        assert!(facts[0].presence.is_some());
        assert_eq!(
            facts[0].evidence,
            EvidenceClaim {
                support: CapabilitySupport::Confirmed,
                authority: EvidenceAuthority::Authoritative,
                provenance: EvidenceProvenance::AggregatedHookAuthority,
            }
        );
        assert!(matches!(
            facts[1].kind,
            ObservationKind::Session(SessionOp::Observed)
        ));
        assert!(facts[1].workspace.is_some());
        assert!(matches!(
            facts[2].kind,
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working))
        ));
        assert_eq!(
            facts[2].valid_until,
            Some(Timestamp::from_unix_millis(
                1_000 + HERDR_ACTIVITY_LEASE_MILLIS
            ))
        );
        for fact in &facts {
            fact.validate_shape().expect("valid shape");
        }
    }

    #[test]
    fn activity_status_mapping_covers_working_idle_blocked_only() {
        for (status, expected) in [
            ("working", ReportedActivityState::Working),
            ("idle", ReportedActivityState::Idle),
            ("blocked", ReportedActivityState::WaitingPermission),
        ] {
            let facts = facts(&entry("codex", status, "id", "codex-session-1"));
            assert!(
                facts
                    .iter()
                    .any(|fact| fact.kind == ObservationKind::Activity(ActivityOp::Set(expected))),
                "status {status} must map to {expected:?}"
            );
        }
        for status in ["done", "unknown"] {
            let facts = facts(&entry("codex", status, "id", "codex-session-1"));
            assert!(
                facts
                    .iter()
                    .all(|fact| fact.domain() != EvidenceDomain::Activity),
                "status {status} must not produce an activity op"
            );
        }
    }

    #[test]
    fn unmapped_agent_is_presence_only_without_subject_hint() {
        let facts = facts(&entry("cursor", "working", "id", RAW_SESSION));
        assert_eq!(facts.len(), 1);
        let presence = facts[0].presence.as_ref().expect("presence ref");
        assert!(presence.subject_hint().is_none());
    }

    #[test]
    fn non_id_session_kind_is_not_identity_evidence() {
        let facts = facts(&entry("claude", "working", "path", "/some/path"));
        assert_eq!(facts.len(), 1);
        assert!(facts[0].session.is_none());
        let presence = facts[0].presence.as_ref().expect("presence ref");
        assert_eq!(
            presence.subject_hint().map(SubjectNamespace::as_str),
            Some("anthropic/claude-code")
        );
    }

    #[test]
    fn missing_pane_id_is_ignored() {
        let payload = format!(
            "{{\"agent\":\"claude\",\"agent_status\":\"working\",\
             \"cwd\":\"{RAW_CWD}\",\"state_change_seq\":1}}"
        );
        assert_eq!(
            decode(&payload),
            DecodeOutcome::Ignore(IgnoreReason::MissingStableIdentity)
        );
    }

    #[test]
    fn missing_socket_env_is_ignored() {
        let _env = EnvGuard::clear();
        let outcome = HerdrSnapshotAdapter::new()
            .decode(
                input(&entry("claude", "working", "id", RAW_SESSION)),
                &keyer(),
            )
            .expect("decode");
        assert_eq!(
            outcome,
            DecodeOutcome::Ignore(IgnoreReason::MissingStableIdentity)
        );
    }

    #[test]
    fn raw_paths_and_native_ids_never_leave_the_adapter() {
        let facts = facts(&entry("claude", "working", "id", RAW_SESSION));
        for fact in &facts {
            let rendered = format!("{fact:?}");
            assert!(!rendered.contains(RAW_CWD), "raw cwd leaked: {rendered}");
            assert!(
                !rendered.contains(RAW_SESSION),
                "native id leaked: {rendered}"
            );
            assert!(!rendered.contains(RAW_TITLE), "title leaked: {rendered}");
            assert!(!rendered.contains("w2A:p1"), "pane id leaked: {rendered}");
        }
    }

    #[test]
    fn herdr_authority_bytes_match_the_mapped_hook_adapters() {
        let _env = EnvGuard::set(SOCKET);
        let keyer = keyer();
        for (agent, namespace, expected) in [
            (
                "claude",
                CLAUDE_SUBJECT_NAMESPACE,
                ClaudeHookAdapter::authority(),
            ),
            (
                "codex",
                CODEX_SUBJECT_NAMESPACE,
                CodexHookAdapter::authority(),
            ),
            (
                "opencode",
                OPENCODE_SUBJECT_NAMESPACE,
                OpenCodePluginAdapter::authority(),
            ),
        ] {
            let facts = facts_locked(&entry(agent, "idle", "id", RAW_SESSION));
            let session = facts
                .iter()
                .find_map(|fact| fact.session.clone())
                .expect("session fact");
            assert_eq!(
                session.key().authority_id().digest().as_bytes(),
                expected.digest().as_bytes(),
                "{agent} authority must byte-reuse the Hook adapter digest"
            );
            let direct = keyer
                .session_key(
                    &SubjectNamespace::parse(namespace).expect("namespace"),
                    &expected,
                    SensitiveId::new(RAW_SESSION.as_bytes()),
                )
                .expect("direct session key");
            assert_eq!(session.key().stable_id(), direct.stable_id());
        }
    }

    #[test]
    fn template_declares_readonly_snapshot_capabilities_only() {
        let template = HerdrSnapshotAdapter::new().contract_template(Some("0.9.1"));
        assert_eq!(
            template.acquisition.iter().copied().collect::<Vec<_>>(),
            vec![AcquisitionMode::AggregatedSnapshot]
        );
        let activity = template
            .capabilities
            .get(&EvidenceDomain::Activity)
            .expect("activity capability");
        assert_eq!(activity.max_authority, EvidenceAuthority::Observational);
        assert_eq!(
            activity.provenance,
            EvidenceProvenance::AggregatedScreenInference
        );
        assert!(
            !template
                .capabilities
                .contains_key(&EvidenceDomain::Lifecycle),
            "Herdr must not claim lifecycle evidence"
        );
        assert!(template.snapshot_semantics.supported);
        assert!(!template.stream_semantics.supported);
        assert!(!template.requires_instrumentation);
        assert_eq!(
            template.stability,
            InterfaceStability::VersionedExperimental
        );
    }

    #[test]
    fn decoded_observations_pass_contract_validation() {
        let _env = EnvGuard::set(SOCKET);
        let keyer = keyer();
        let adapter = HerdrSnapshotAdapter::new();
        let facts = match adapter
            .decode(
                input(&entry("claude", "working", "id", RAW_SESSION)),
                &keyer,
            )
            .expect("decode")
        {
            DecodeOutcome::Observations(facts) => facts.into_vec(),
            other => panic!("expected observations, got {other:?}"),
        };
        let mut registry = AdapterRegistry::new();
        registry
            .register(Arc::new(HerdrSnapshotAdapter::new()))
            .expect("register");
        let template = adapter.contract_template(None);
        let contract =
            template.hook_contract(herdr_instance_id(SOCKET), ContractRevision::new(1), None);
        let scope = SnapshotScope {
            workspaces: WorkspaceScope::Selected,
            subjects: template.subjects.clone(),
            entity_kinds: BoundedSet::try_from_iter([
                ObservedEntityKind::Presence,
                ObservedEntityKind::Session,
            ])
            .expect("entity kinds"),
            domains: BoundedSet::try_from_iter([
                EvidenceDomain::Presence,
                EvidenceDomain::Session,
                EvidenceDomain::Activity,
            ])
            .expect("domains"),
        };
        let envelope = SnapshotEnvelope {
            stream: StreamRef {
                observer: observer(),
                instance: herdr_instance_id(SOCKET),
                epoch: StreamEpoch::from_digest(StableDigest::from_bytes([7; 32])),
            },
            snapshot_id: SnapshotId::from_digest(StableDigest::from_bytes([8; 32])),
            chunk_index: 0,
            final_chunk: true,
            captured_at: Timestamp::from_unix_millis(1_000),
            scope,
            completeness: SnapshotCompleteness::Complete,
            watermark: None,
            observations: BoundedVec::try_from_vec(facts).expect("snapshot observations"),
        };
        registry
            .validate_envelope(ObservationEnvelope::Snapshot(envelope), &contract)
            .expect("herdr snapshot envelope validates against its template");
    }
}
