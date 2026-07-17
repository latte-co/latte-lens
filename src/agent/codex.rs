use std::collections::BTreeMap;

use super::hook_json::{
    HookJsonParser, append_identity_part, set_optional_once, set_required_once,
};

use super::{
    AcquisitionMode, ActivityOp, AdapterDelivery, AdapterError, AdapterInput, AgentKind,
    AgentObservation, AgentOp, AgentRef, AuthorityId, BoundedSet, BoundedText, BoundedVec,
    CapabilityClaim, CapabilitySupport, CodeAgentAdapter, DecodeOutcome, EvidenceAuthority,
    EvidenceClaim, EvidenceDomain, EvidenceProvenance, HookDecodeOutcome, IdentityKeyer,
    IgnoreReason, InstanceContractTemplate, InterfaceStability, LifecycleOp, ObservationKind,
    ObserverDescriptor, ObserverId, ObserverInstanceId, PermissionOp, ReportedActivityState,
    ReportedSessionLifecycle, SensitiveId, SessionOp, SessionRef, SnapshotSemantics, StreamEpoch,
    StreamOp, StreamRef, StreamSemantics, SubjectNamespace, Timestamp, ToolOp, TurnOp,
    WorkspaceHint, stable_hash,
};

pub const CODEX_HOOK_OBSERVER_ID: &str = "openai/codex-hook";
pub const CODEX_SUBJECT_NAMESPACE: &str = "openai/codex";
const ADAPTER_VERSION: &str = "1";
const ACTIVITY_LEASE_MILLIS: u64 = 30_000;

/// Official Codex command-hook adapter.
///
/// Only stable identity and event-shape fields are decoded. Prompt, tool
/// input/output, assistant messages, transcript paths, model, and raw cwd are
/// deliberately skipped by the bounded parser and never enter core models.
#[derive(Clone, Copy, Debug, Default)]
pub struct CodexHookAdapter;

impl CodexHookAdapter {
    pub const fn new() -> Self {
        Self
    }

    fn observer() -> ObserverId {
        ObserverId::parse(CODEX_HOOK_OBSERVER_ID).expect("static Codex observer id")
    }

    fn subject() -> SubjectNamespace {
        SubjectNamespace::parse(CODEX_SUBJECT_NAMESPACE).expect("static Codex subject namespace")
    }

    fn instance() -> ObserverInstanceId {
        ObserverInstanceId::from_digest(stable_hash(
            b"codex-hook-instance",
            &[CODEX_HOOK_OBSERVER_ID.as_bytes()],
        ))
    }

    fn epoch() -> StreamEpoch {
        StreamEpoch::from_digest(stable_hash(
            b"codex-hook-epoch",
            &[ADAPTER_VERSION.as_bytes()],
        ))
    }

    fn authority() -> AuthorityId {
        AuthorityId::from_digest(stable_hash(
            b"codex-session-authority",
            &[CODEX_SUBJECT_NAMESPACE.as_bytes(), b"session_id"],
        ))
    }
}

impl CodeAgentAdapter for CodexHookAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        ObserverDescriptor::new(Self::observer(), "Codex Hooks", ADAPTER_VERSION)
            .expect("static Codex observer descriptor")
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        let subjects = BoundedSet::try_from_iter([Self::subject()]).expect("one Codex subject");
        let acquisition =
            BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).expect("one acquisition mode");
        let capabilities = BTreeMap::from([
            (
                EvidenceDomain::Session,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Observational,
                    "Every supported Codex hook carries session_id",
                ),
            ),
            (
                EvidenceDomain::Lifecycle,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "SessionStart proves Open; there is no SessionEnd or lifecycle snapshot",
                ),
            ),
            (
                EvidenceDomain::Activity,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "Activity is leased event evidence; there is no current-state snapshot",
                ),
            ),
            (
                EvidenceDomain::Turn,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "Observed turn hooks carry turn_id; history and missed turns cannot recover",
                ),
            ),
            (
                EvidenceDomain::Permission,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "PermissionRequest proves Requested; resolution is not exposed",
                ),
            ),
            (
                EvidenceDomain::Tool,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "Pre/Post cover intercepted tools only; outcome and missed tools cannot recover",
                ),
            ),
            (
                EvidenceDomain::AgentTopology,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "Subagent start/stop is event-only; there is no topology snapshot",
                ),
            ),
        ]);
        InstanceContractTemplate {
            observer: Self::observer(),
            subjects,
            acquisition,
            capabilities,
            snapshot_semantics: SnapshotSemantics::unsupported(),
            stream_semantics: StreamSemantics::unsupported(),
            requires_instrumentation: true,
            stability: InterfaceStability::VersionedExperimental,
        }
    }

    fn decode(
        &self,
        input: AdapterInput<'_>,
        _identity: &dyn IdentityKeyer,
    ) -> Result<DecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        Ok(DecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent))
    }

    fn decode_hook(
        &self,
        input: AdapterInput<'_>,
        identity: &dyn IdentityKeyer,
    ) -> Result<HookDecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        if input.delivery != AdapterDelivery::HookEvent {
            return Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent));
        }
        let workspace = input
            .workspace
            .clone()
            .ok_or(AdapterError::MalformedInput)?;
        let payload = CodexPayload::parse(input.payload)?;
        if payload.hook_event_name != input.event_name {
            return Err(AdapterError::MalformedInput);
        }
        decode_event(input, payload, workspace, identity)
    }
}

fn capability(
    support: CapabilitySupport,
    max_authority: EvidenceAuthority,
    reason: &'static str,
) -> CapabilityClaim {
    CapabilityClaim {
        support,
        max_authority,
        provenance: EvidenceProvenance::InstrumentedHook,
        reason: BoundedText::try_new(reason).expect("static Codex capability reason"),
        lease_backed: false,
    }
}

fn claim(support: CapabilitySupport, authority: EvidenceAuthority) -> EvidenceClaim {
    EvidenceClaim {
        support,
        authority,
        provenance: EvidenceProvenance::InstrumentedHook,
    }
}

fn decode_event(
    input: AdapterInput<'_>,
    payload: CodexPayload,
    workspace: WorkspaceHint,
    identity: &dyn IdentityKeyer,
) -> Result<HookDecodeOutcome, AdapterError> {
    let Some(event) = CodexEvent::parse(input.event_name) else {
        return Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent));
    };
    if matches!(event, CodexEvent::PreCompact | CodexEvent::PostCompact) {
        return Ok(HookDecodeOutcome::Ignore(IgnoreReason::NoObservableFact));
    }
    event.validate(&payload)?;

    let subject = CodexHookAdapter::subject();
    let authority = CodexHookAdapter::authority();
    let session_key = identity
        .session_key(
            &subject,
            &authority,
            SensitiveId::new(payload.session_id.as_bytes()),
        )
        .map_err(|_| AdapterError::IdentityRejected)?;
    let session = SessionRef::new(session_key, workspace.clone());
    let turn = payload
        .turn_id
        .as_deref()
        .map(|turn_id| {
            identity.turn_key(
                session.key(),
                &authority,
                SensitiveId::new(turn_id.as_bytes()),
            )
        })
        .transpose()
        .map_err(|_| AdapterError::IdentityRejected)?;
    let agent = payload
        .agent_id
        .as_deref()
        .map(|agent_id| {
            let primary = identity.agent_key(session.key(), SensitiveId::new(b"primary"))?;
            let key = identity.agent_key(session.key(), SensitiveId::new(agent_id.as_bytes()))?;
            Ok(AgentRef::new(key, Some(primary), Some(AgentKind::Subagent)))
        })
        .transpose()
        .map_err(|_: super::IdentityError| AdapterError::IdentityRejected)?;

    let mut observations = BoundedVec::new();
    let mut push = |kind, evidence, leased_activity: bool| {
        observations
            .try_push(AgentObservation {
                observed_at: input.observed_at,
                valid_until: leased_activity.then(|| {
                    Timestamp::from_unix_millis(
                        input
                            .observed_at
                            .as_unix_millis()
                            .saturating_add(ACTIVITY_LEASE_MILLIS),
                    )
                }),
                presence: None,
                session: Some(session.clone()),
                agent: agent.clone(),
                turn: turn.clone(),
                workspace: Some(workspace.clone()),
                kind,
                evidence,
            })
            .map_err(|_| AdapterError::OutputRejected)
    };

    match event {
        CodexEvent::SessionStart => {
            push(
                ObservationKind::Session(SessionOp::Observed),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Observational,
                ),
                false,
            )?;
            push(
                ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
        }
        CodexEvent::UserPromptSubmit => {
            push(
                ObservationKind::Turn(TurnOp::Started),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        CodexEvent::PreToolUse => {
            push(
                ObservationKind::Tool(ToolOp::Started),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        CodexEvent::PermissionRequest => {
            push(
                ObservationKind::Permission(PermissionOp::Requested),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(
                    ReportedActivityState::WaitingPermission,
                )),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        CodexEvent::PostToolUse => {
            push(
                ObservationKind::Tool(ToolOp::Completed),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                ObservationKind::Turn(TurnOp::Updated),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
        }
        CodexEvent::SubagentStart => push(
            ObservationKind::Agent(AgentOp::Observed),
            claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
            false,
        )?,
        CodexEvent::SubagentStop => push(
            ObservationKind::Agent(AgentOp::Released),
            claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
            false,
        )?,
        CodexEvent::Stop => {
            push(
                ObservationKind::Turn(TurnOp::Completed),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        CodexEvent::PreCompact | CodexEvent::PostCompact => unreachable!("ignored above"),
    }

    let observer = CodexHookAdapter::observer();
    let instance = CodexHookAdapter::instance();
    let epoch = CodexHookAdapter::epoch();
    let event_identity = payload.event_identity(input.event_name);
    let event_id = identity
        .event_id(
            &observer,
            &instance,
            &epoch,
            SensitiveId::new(&event_identity),
        )
        .map_err(|_| AdapterError::IdentityRejected)?;
    Ok(HookDecodeOutcome::Event(Box::new(super::EventEnvelope {
        stream: StreamRef {
            observer,
            instance,
            epoch,
        },
        event_id,
        sequence: None,
        op: StreamOp::Upsert(observations),
    })))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexEvent {
    SessionStart,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    UserPromptSubmit,
    SubagentStart,
    SubagentStop,
    Stop,
}

impl CodexEvent {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "SessionStart" => Some(Self::SessionStart),
            "PreToolUse" => Some(Self::PreToolUse),
            "PermissionRequest" => Some(Self::PermissionRequest),
            "PostToolUse" => Some(Self::PostToolUse),
            "PreCompact" => Some(Self::PreCompact),
            "PostCompact" => Some(Self::PostCompact),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "SubagentStart" => Some(Self::SubagentStart),
            "SubagentStop" => Some(Self::SubagentStop),
            "Stop" => Some(Self::Stop),
            _ => None,
        }
    }

    fn validate(self, payload: &CodexPayload) -> Result<(), AdapterError> {
        let require = |value: Option<&String>| {
            value
                .filter(|value| !value.is_empty())
                .map(|_| ())
                .ok_or(AdapterError::MalformedInput)
        };
        match self {
            Self::SessionStart => {
                require(payload.source.as_ref())?;
            }
            Self::PreToolUse | Self::PostToolUse => {
                require(payload.turn_id.as_ref())?;
                require(payload.tool_name.as_ref())?;
                require(payload.tool_use_id.as_ref())?;
            }
            Self::PermissionRequest => {
                require(payload.turn_id.as_ref())?;
                require(payload.tool_name.as_ref())?;
            }
            Self::PreCompact | Self::PostCompact => {
                require(payload.turn_id.as_ref())?;
                require(payload.trigger.as_ref())?;
            }
            Self::UserPromptSubmit | Self::Stop => {
                require(payload.turn_id.as_ref())?;
            }
            Self::SubagentStart | Self::SubagentStop => {
                require(payload.turn_id.as_ref())?;
                require(payload.agent_id.as_ref())?;
                require(payload.agent_type.as_ref())?;
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct CodexPayload {
    session_id: String,
    hook_event_name: String,
    turn_id: Option<String>,
    tool_use_id: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    source: Option<String>,
    trigger: Option<String>,
    tool_name: Option<String>,
}

impl CodexPayload {
    fn parse(bytes: &[u8]) -> Result<Self, AdapterError> {
        std::str::from_utf8(bytes).map_err(|_| AdapterError::MalformedInput)?;
        let mut parser = HookJsonParser::new(bytes);
        let mut payload = Self::default();
        let mut session_id_seen = false;
        let mut hook_event_name_seen = false;
        parser.parse_object(|parser, key| match key {
            "session_id" => set_required_once(
                &mut payload.session_id,
                &mut session_id_seen,
                parser.parse_bounded_string(),
            ),
            "hook_event_name" => set_required_once(
                &mut payload.hook_event_name,
                &mut hook_event_name_seen,
                parser.parse_bounded_string(),
            ),
            "turn_id" => set_optional_once(&mut payload.turn_id, parser.parse_bounded_string()),
            "tool_use_id" => {
                set_optional_once(&mut payload.tool_use_id, parser.parse_bounded_string())
            }
            "agent_id" => set_optional_once(&mut payload.agent_id, parser.parse_bounded_string()),
            "agent_type" => {
                set_optional_once(&mut payload.agent_type, parser.parse_bounded_string())
            }
            "source" => set_optional_once(&mut payload.source, parser.parse_bounded_string()),
            "trigger" => set_optional_once(&mut payload.trigger, parser.parse_bounded_string()),
            "tool_name" => set_optional_once(&mut payload.tool_name, parser.parse_bounded_string()),
            _ => parser.skip_value(1),
        })?;
        parser.finish()?;
        if payload.session_id.is_empty() || payload.hook_event_name.is_empty() {
            return Err(AdapterError::MalformedInput);
        }
        Ok(payload)
    }

    fn event_identity(&self, event_name: &str) -> Vec<u8> {
        let mut output = Vec::with_capacity(256);
        append_identity_part(&mut output, event_name.as_bytes());
        append_identity_part(&mut output, self.session_id.as_bytes());
        for value in [
            self.turn_id.as_deref(),
            self.tool_use_id.as_deref(),
            self.agent_id.as_deref(),
            self.source.as_deref(),
            self.trigger.as_deref(),
            self.tool_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            append_identity_part(&mut output, value.as_bytes());
        }
        output
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::agent::{HmacIdentityKeyer, ObservationEnvelope, SensitiveId};

    fn identity() -> HmacIdentityKeyer {
        HmacIdentityKeyer::new(SensitiveId::new(&[0x5a; 32])).expect("identity")
    }

    fn workspace() -> WorkspaceHint {
        WorkspaceHint::from_digest(stable_hash(b"test-workspace", &[b"codex"]))
    }

    fn decode(event_name: &str, payload: &[u8]) -> Result<HookDecodeOutcome, AdapterError> {
        CodexHookAdapter::new().decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name,
                observer_version: Some("0.144.3"),
                observed_at: Timestamp::from_unix_millis(100),
                workspace: Some(workspace()),
                payload,
            },
            &identity(),
        )
    }

    fn observations(outcome: HookDecodeOutcome) -> Vec<AgentObservation> {
        let HookDecodeOutcome::Event(event) = outcome else {
            panic!("expected event")
        };
        let StreamOp::Upsert(observations) = event.op else {
            panic!("expected upsert")
        };
        observations.into_vec()
    }

    #[test]
    fn production_contract_is_explicitly_partial_and_hook_only() {
        let adapter = CodexHookAdapter::new();
        assert_eq!(adapter.descriptor().id.as_str(), CODEX_HOOK_OBSERVER_ID);
        let template = adapter.contract_template(Some("0.144.3"));
        assert_eq!(template.subjects.iter().count(), 1);
        assert!(template.acquisition.contains(&AcquisitionMode::HookEvent));
        assert_eq!(
            template.capabilities[&EvidenceDomain::Session].support,
            CapabilitySupport::Confirmed
        );
        for domain in [
            EvidenceDomain::Lifecycle,
            EvidenceDomain::Activity,
            EvidenceDomain::Turn,
            EvidenceDomain::Permission,
            EvidenceDomain::Tool,
            EvidenceDomain::AgentTopology,
        ] {
            assert_eq!(
                template.capabilities[&domain].support,
                CapabilitySupport::Partial
            );
            assert!(
                !template.capabilities[&domain].reason.as_str().is_empty(),
                "{domain:?} must explain its partial boundary"
            );
        }
        assert!(!template.capabilities.contains_key(&EvidenceDomain::Change));
        assert!(
            !template
                .capabilities
                .contains_key(&EvidenceDomain::Artifact)
        );
        assert!(!template.snapshot_semantics.supported);
        assert!(!template.stream_semantics.supported);
        assert!(template.requires_instrumentation);
    }

    #[test]
    fn official_hook_shapes_map_to_bounded_core_facts() {
        type HookCase<'a> = (&'a str, &'a [u8], fn(&ObservationKind) -> bool);
        let cases: [HookCase<'_>; 8] = [
            (
                "SessionStart",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"SessionStart","model":"gpt","source":"startup"}"#,
                |kind| matches!(kind, ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open))),
            ),
            (
                "UserPromptSubmit",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"UserPromptSubmit","model":"gpt","turn_id":"t-1","prompt":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Turn(TurnOp::Started)),
            ),
            (
                "PreToolUse",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"PreToolUse","model":"gpt","turn_id":"t-1","tool_name":"Bash","tool_use_id":"tool-1","tool_input":{"command":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Started)),
            ),
            (
                "PermissionRequest",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"PermissionRequest","model":"gpt","turn_id":"t-1","tool_name":"Bash","tool_input":{"description":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Permission(PermissionOp::Requested)),
            ),
            (
                "PostToolUse",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"PostToolUse","model":"gpt","turn_id":"t-1","tool_name":"Bash","tool_use_id":"tool-1","tool_input":{},"tool_response":{"output":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Completed)),
            ),
            (
                "SubagentStart",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"SubagentStart","model":"gpt","turn_id":"t-1","agent_id":"a-1","agent_type":"worker"}"#,
                |kind| matches!(kind, ObservationKind::Agent(AgentOp::Observed)),
            ),
            (
                "SubagentStop",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"SubagentStop","model":"gpt","turn_id":"t-1","agent_id":"a-1","agent_type":"worker","last_assistant_message":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Agent(AgentOp::Released)),
            ),
            (
                "Stop",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"Stop","model":"gpt","turn_id":"t-1","last_assistant_message":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Turn(TurnOp::Completed)),
            ),
        ];

        for (event_name, payload, expected) in cases {
            let outcome = decode(event_name, payload).expect(event_name);
            let facts = observations(outcome);
            assert!(
                facts.iter().any(|fact| expected(&fact.kind)),
                "{event_name}"
            );
            assert!(facts.iter().all(|fact| {
                fact.session.is_some()
                    && fact.workspace.as_ref() == Some(&workspace())
                    && fact.validate_shape().is_ok()
            }));
        }
    }

    #[test]
    fn activity_is_leased_and_stop_never_ends_the_session() {
        let facts = observations(
            decode(
                "Stop",
                br#"{"session_id":"s","hook_event_name":"Stop","turn_id":"t"}"#,
            )
            .expect("stop"),
        );
        assert!(facts.iter().any(|fact| matches!(
            fact.kind,
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle))
        ) && fact.valid_until
            == Some(Timestamp::from_unix_millis(30_100))));
        assert!(!facts.iter().any(|fact| matches!(
            fact.kind,
            ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended))
        )));
    }

    #[test]
    fn sensitive_unknown_fields_are_skipped_and_never_reach_the_envelope() {
        let canary = "privacy-canary-prompt-tool-transcript";
        let payload = format!(
            r#"{{"session_id":"s","hook_event_name":"PreToolUse","turn_id":"t","tool_name":"mcp__fs__read","tool_use_id":"u","transcript_path":"/{canary}","tool_input":{{"nested":[true,false,null,-1.25e2,"{canary}"]}},"last_assistant_message":"{canary}"}}"#
        );
        let HookDecodeOutcome::Event(event) =
            decode("PreToolUse", payload.as_bytes()).expect("decode")
        else {
            panic!("expected event")
        };
        let rendered = format!("{event:?}");
        assert!(!rendered.contains(canary));
        let adapter = CodexHookAdapter::new();
        let contract = adapter.contract_template(None).hook_contract(
            event.stream.instance.clone(),
            crate::agent::ContractRevision::new(1),
            None,
        );
        assert!(
            contract
                .validate_envelope(&ObservationEnvelope::Event(*event))
                .is_ok()
        );
    }

    #[test]
    fn mismatch_duplicate_missing_and_malformed_fields_fail_closed() {
        let cases: [(&str, &[u8]); 8] = [
            (
                "Stop",
                br#"{"session_id":"s","hook_event_name":"UserPromptSubmit","turn_id":"t"}"#,
            ),
            ("Stop", br#"{"hook_event_name":"Stop","turn_id":"t"}"#),
            ("Stop", br#"{"session_id":"s","hook_event_name":"Stop"}"#),
            (
                "Stop",
                br#"{"session_id":"s","session_id":"again","hook_event_name":"Stop","turn_id":"t"}"#,
            ),
            (
                "Stop",
                br#"{"session_id":"","session_id":"again","hook_event_name":"Stop","turn_id":"t"}"#,
            ),
            (
                "Stop",
                br#"{"session_id":null,"hook_event_name":"Stop","turn_id":"t"}"#,
            ),
            ("Stop", br#"{"session_id":"s","hook_event_name":"Stop","turn_id":"\uD800"}"#),
            ("Stop", br#"[]"#),
        ];
        for (event_name, payload) in cases {
            assert_eq!(
                decode(event_name, payload),
                Err(AdapterError::MalformedInput)
            );
        }
        assert_eq!(
            decode(
                "Stop",
                b"{\"session_id\":\"s\",\"hook_event_name\":\"Stop\",\"turn_id\":\"\xff\"}"
            ),
            Err(AdapterError::MalformedInput)
        );
    }

    #[test]
    fn escaped_ids_decode_but_compaction_and_future_events_are_ignored() {
        let facts = observations(
            decode(
                "Stop",
                br#"{"session_id":"s\u002d\ud83d\ude80","hook_event_name":"Stop","turn_id":"t\/1"}"#,
            )
            .expect("escaped ids"),
        );
        assert_eq!(facts.len(), 2);
        for event_name in ["PreCompact", "PostCompact"] {
            let payload = format!(
                r#"{{"session_id":"s","hook_event_name":"{event_name}","turn_id":"t","trigger":"auto"}}"#
            );
            assert_eq!(
                decode(event_name, payload.as_bytes()),
                Ok(HookDecodeOutcome::Ignore(IgnoreReason::NoObservableFact))
            );
        }
        assert_eq!(
            decode(
                "FutureHook",
                br#"{"session_id":"s","hook_event_name":"FutureHook"}"#
            ),
            Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent))
        );
    }

    #[test]
    fn generic_decode_and_wrong_delivery_never_guess_a_payload() {
        let adapter = CodexHookAdapter::new();
        let input = AdapterInput {
            delivery: AdapterDelivery::ProviderEvent,
            event_name: "Stop",
            observer_version: None,
            observed_at: Timestamp::from_unix_millis(1),
            workspace: Some(workspace()),
            payload: br#"{"session_id":"s","hook_event_name":"Stop","turn_id":"t"}"#,
        };
        assert_eq!(
            adapter.decode(input, &identity()),
            Ok(DecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent))
        );
        let input = AdapterInput {
            delivery: AdapterDelivery::ProviderEvent,
            event_name: "Stop",
            observer_version: None,
            observed_at: Timestamp::from_unix_millis(1),
            workspace: Some(workspace()),
            payload: br#"{"session_id":"s","hook_event_name":"Stop","turn_id":"t"}"#,
        };
        assert_eq!(
            adapter.decode_hook(input, &identity()),
            Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent))
        );
    }
}
