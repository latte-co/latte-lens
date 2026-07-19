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

pub const TRAEX_HOOK_OBSERVER_ID: &str = "bytedance/traex-hook";
pub const TRAEX_SUBJECT_NAMESPACE: &str = "bytedance/traex";
const ADAPTER_VERSION: &str = "1";
const ACTIVITY_LEASE_MILLIS: u64 = 30_000;

/// TraeX command-hook adapter.
///
/// TraeX shares part of its hook implementation lineage with Codex, but its
/// session/turn wire format and lifecycle surface are versioned independently.
/// It therefore owns a distinct observer, subject namespace, and authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct TraexHookAdapter;

impl TraexHookAdapter {
    pub const fn new() -> Self {
        Self
    }

    fn observer() -> ObserverId {
        ObserverId::parse(TRAEX_HOOK_OBSERVER_ID).expect("static TraeX observer id")
    }

    fn subject() -> SubjectNamespace {
        SubjectNamespace::parse(TRAEX_SUBJECT_NAMESPACE).expect("static TraeX subject namespace")
    }

    fn instance() -> ObserverInstanceId {
        ObserverInstanceId::from_digest(stable_hash(
            b"traex-hook-instance",
            &[TRAEX_HOOK_OBSERVER_ID.as_bytes()],
        ))
    }

    fn epoch() -> StreamEpoch {
        StreamEpoch::from_digest(stable_hash(
            b"traex-hook-epoch",
            &[ADAPTER_VERSION.as_bytes()],
        ))
    }

    fn authority() -> AuthorityId {
        AuthorityId::from_digest(stable_hash(
            b"traex-session-authority",
            &[TRAEX_SUBJECT_NAMESPACE.as_bytes(), b"session_id"],
        ))
    }
}

impl CodeAgentAdapter for TraexHookAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        ObserverDescriptor::new(Self::observer(), "TraeX Hooks", ADAPTER_VERSION)
            .expect("static TraeX observer descriptor")
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        let subjects = BoundedSet::try_from_iter([Self::subject()]).expect("one TraeX subject");
        let acquisition =
            BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).expect("one acquisition mode");
        let capabilities = BTreeMap::from([
            (
                EvidenceDomain::Session,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Observational,
                    "Every supported TraeX hook carries session_id",
                ),
            ),
            (
                EvidenceDomain::Lifecycle,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                    "SessionStart and SessionEnd cover the documented lifecycle boundary",
                ),
            ),
            (
                EvidenceDomain::Activity,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "Activity is leased hook evidence; there is no current-state snapshot",
                ),
            ),
            (
                EvidenceDomain::Turn,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "Turn-scoped hooks carry turn_id but missed turns cannot recover",
                ),
            ),
            (
                EvidenceDomain::Permission,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "PermissionRequest proves Requested; user resolution is not exposed",
                ),
            ),
            (
                EvidenceDomain::Tool,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                    "Pre, successful Post, and failed Post expose documented tool outcomes",
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
            stability: InterfaceStability::PrivateExperimental,
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
        let payload = TraexPayload::parse(input.payload)?;
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
        reason: BoundedText::try_new(reason).expect("static TraeX capability reason"),
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
    payload: TraexPayload,
    workspace: WorkspaceHint,
    identity: &dyn IdentityKeyer,
) -> Result<HookDecodeOutcome, AdapterError> {
    let Some(event) = TraexEvent::parse(input.event_name) else {
        return Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent));
    };
    if matches!(
        event,
        TraexEvent::Notification | TraexEvent::PreCompact | TraexEvent::PostCompact
    ) {
        return Ok(HookDecodeOutcome::Ignore(IgnoreReason::NoObservableFact));
    }
    event.validate(&payload)?;

    let subject = TraexHookAdapter::subject();
    let authority = TraexHookAdapter::authority();
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
        TraexEvent::SessionStart => {
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
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
        }
        TraexEvent::UserPromptSubmit => {
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
        TraexEvent::PreToolUse => {
            push(
                ObservationKind::Tool(ToolOp::Started),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        TraexEvent::PermissionRequest => {
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
        TraexEvent::PostToolUse => {
            push(
                ObservationKind::Tool(ToolOp::Completed),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        TraexEvent::PostToolUseFailure => {
            push(
                ObservationKind::Tool(ToolOp::Failed),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        TraexEvent::SubagentStart => push(
            ObservationKind::Agent(AgentOp::Observed),
            claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
            false,
        )?,
        TraexEvent::SubagentStop => push(
            ObservationKind::Agent(AgentOp::Released),
            claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
            false,
        )?,
        TraexEvent::Stop => {
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
        TraexEvent::SessionEnd => {
            push(
                ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended)),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Clear),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
        }
        TraexEvent::Notification | TraexEvent::PreCompact | TraexEvent::PostCompact => {
            unreachable!("ignored above")
        }
    }

    let observer = TraexHookAdapter::observer();
    let instance = TraexHookAdapter::instance();
    let epoch = TraexHookAdapter::epoch();
    let event_identity = payload.event_identity(event, input.observed_at);
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
enum TraexEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    SubagentStart,
    SubagentStop,
    Stop,
    SessionEnd,
    Notification,
    PreCompact,
    PostCompact,
}

impl TraexEvent {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "SessionStart" => Some(Self::SessionStart),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "PreToolUse" => Some(Self::PreToolUse),
            "PermissionRequest" => Some(Self::PermissionRequest),
            "PostToolUse" => Some(Self::PostToolUse),
            "PostToolUseFailure" => Some(Self::PostToolUseFailure),
            "SubagentStart" => Some(Self::SubagentStart),
            "SubagentStop" => Some(Self::SubagentStop),
            "Stop" => Some(Self::Stop),
            "SessionEnd" => Some(Self::SessionEnd),
            "Notification" => Some(Self::Notification),
            "PreCompact" => Some(Self::PreCompact),
            "PostCompact" => Some(Self::PostCompact),
            _ => None,
        }
    }

    fn validate(self, payload: &TraexPayload) -> Result<(), AdapterError> {
        let require = |value: Option<&String>| {
            value
                .filter(|value| !value.is_empty())
                .map(|_| ())
                .ok_or(AdapterError::MalformedInput)
        };
        match self {
            Self::SessionStart => require(payload.source.as_ref())?,
            Self::UserPromptSubmit | Self::Stop => require(payload.turn_id.as_ref())?,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure => {
                require(payload.turn_id.as_ref())?;
                require(payload.tool_name.as_ref())?;
                require(payload.tool_use_id.as_ref())?;
            }
            Self::PermissionRequest => {
                require(payload.turn_id.as_ref())?;
                require(payload.tool_name.as_ref())?;
            }
            Self::SubagentStart | Self::SubagentStop => {
                require(payload.turn_id.as_ref())?;
                require(payload.agent_id.as_ref())?;
                require(payload.agent_type.as_ref())?;
            }
            Self::SessionEnd | Self::Notification | Self::PreCompact | Self::PostCompact => {}
        }
        Ok(())
    }
}

#[derive(Default)]
struct TraexPayload {
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

impl TraexPayload {
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

    fn event_identity(&self, event: TraexEvent, observed_at: Timestamp) -> Vec<u8> {
        let mut output = Vec::with_capacity(256);
        append_identity_part(&mut output, self.hook_event_name.as_bytes());
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
        if event == TraexEvent::PermissionRequest && self.tool_use_id.is_none() {
            append_identity_part(&mut output, &observed_at.as_unix_millis().to_be_bytes());
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ContractRevision, HmacIdentityKeyer, ObservationEnvelope, SensitiveId};

    fn identity() -> HmacIdentityKeyer {
        HmacIdentityKeyer::new(SensitiveId::new(&[0x74; 32])).expect("identity")
    }

    fn workspace() -> WorkspaceHint {
        WorkspaceHint::from_digest(stable_hash(b"test-workspace", &[b"traex"]))
    }

    fn decode(event_name: &str, payload: &[u8]) -> Result<HookDecodeOutcome, AdapterError> {
        TraexHookAdapter::new().decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name,
                observer_version: Some("0.120.46"),
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

    type DocumentedHookCase<'a> = (&'a str, &'a [u8], fn(&ObservationKind) -> bool);

    #[test]
    fn production_contract_is_private_and_keeps_traex_identity_separate() {
        let adapter = TraexHookAdapter::new();
        assert_eq!(adapter.descriptor().id.as_str(), TRAEX_HOOK_OBSERVER_ID);
        let template = adapter.contract_template(Some("0.120.46"));
        assert_eq!(template.stability, InterfaceStability::PrivateExperimental);
        assert!(template.subjects.contains(&TraexHookAdapter::subject()));
        assert_eq!(
            template.capabilities[&EvidenceDomain::Lifecycle].support,
            CapabilitySupport::Confirmed
        );
        assert_eq!(
            template.capabilities[&EvidenceDomain::Tool].support,
            CapabilitySupport::Confirmed
        );
        assert_eq!(
            template.capabilities[&EvidenceDomain::Turn].support,
            CapabilitySupport::Partial
        );
        assert!(template.snapshot_semantics == SnapshotSemantics::unsupported());
    }

    #[test]
    fn documented_hook_shapes_map_to_bounded_core_facts() {
        let cases: &[DocumentedHookCase<'_>] = &[
            (
                "SessionStart",
                br#"{"session_id":"s","cwd":"/repo","hook_event_name":"SessionStart","model":"m","permission_mode":"default","transcript_path":null,"source":"startup"}"#,
                |kind| matches!(kind, ObservationKind::Lifecycle(_)),
            ),
            (
                "UserPromptSubmit",
                br#"{"session_id":"s","cwd":"/repo","hook_event_name":"UserPromptSubmit","model":"m","permission_mode":"default","transcript_path":null,"turn_id":"t","prompt":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Turn(TurnOp::Started)),
            ),
            (
                "PreToolUse",
                br#"{"session_id":"s","hook_event_name":"PreToolUse","turn_id":"t","tool_name":"Bash","tool_use_id":"u","tool_input":{"command":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Started)),
            ),
            (
                "PermissionRequest",
                br#"{"session_id":"s","hook_event_name":"PermissionRequest","turn_id":"t","tool_name":"Bash","tool_input":{"command":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Permission(PermissionOp::Requested)),
            ),
            (
                "PostToolUse",
                br#"{"session_id":"s","hook_event_name":"PostToolUse","turn_id":"t","tool_name":"Bash","tool_use_id":"u","tool_response":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Completed)),
            ),
            (
                "PostToolUseFailure",
                br#"{"session_id":"s","hook_event_name":"PostToolUseFailure","turn_id":"t","tool_name":"Bash","tool_use_id":"u","error":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Failed)),
            ),
            (
                "SubagentStart",
                br#"{"session_id":"s","hook_event_name":"SubagentStart","turn_id":"t","agent_id":"a","agent_type":"review"}"#,
                |kind| matches!(kind, ObservationKind::Agent(AgentOp::Observed)),
            ),
            (
                "SubagentStop",
                br#"{"session_id":"s","hook_event_name":"SubagentStop","turn_id":"t","agent_id":"a","agent_type":"review"}"#,
                |kind| matches!(kind, ObservationKind::Agent(AgentOp::Released)),
            ),
            (
                "Stop",
                br#"{"session_id":"s","hook_event_name":"Stop","turn_id":"t","stop_hook_active":false,"last_assistant_message":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Turn(TurnOp::Completed)),
            ),
            (
                "SessionEnd",
                br#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"clear"}"#,
                |kind| matches!(kind, ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended))),
            ),
        ];
        for (event, payload, expected) in cases {
            let observations = observations(decode(event, payload).expect(event));
            assert!(
                observations
                    .iter()
                    .any(|observation| expected(&observation.kind)),
                "missing expected fact for {event}"
            );
        }
    }

    #[test]
    fn lifecycle_and_tool_terminals_use_traex_authority() {
        let ended = observations(
            decode(
                "SessionEnd",
                br#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"logout"}"#,
            )
            .expect("session end"),
        );
        assert!(ended.iter().any(|observation| matches!(
            observation.kind,
            ObservationKind::Activity(ActivityOp::Clear)
        )));
        let failed = observations(
            decode(
                "PostToolUseFailure",
                br#"{"session_id":"s","hook_event_name":"PostToolUseFailure","turn_id":"t","tool_name":"Bash","tool_use_id":"u","tool_input":"secret","tool_response":"secret"}"#,
            )
            .expect("failed tool"),
        );
        assert!(
            failed.iter().any(|observation| matches!(
                observation.kind,
                ObservationKind::Tool(ToolOp::Failed)
            ))
        );
        assert!(failed.iter().all(|observation| {
            observation
                .session
                .as_ref()
                .is_some_and(|session| session.key().subject().as_str() == TRAEX_SUBJECT_NAMESPACE)
        }));
    }

    #[test]
    fn exact_turn_and_subagent_ids_remain_distinct_after_private_hashing() {
        let started = observations(
            decode(
                "SubagentStart",
                br#"{"session_id":"s","hook_event_name":"SubagentStart","turn_id":"turn-a","agent_id":"agent-a","agent_type":"review"}"#,
            )
            .expect("subagent start"),
        );
        let stopped = observations(
            decode(
                "SubagentStop",
                br#"{"session_id":"s","hook_event_name":"SubagentStop","turn_id":"turn-b","agent_id":"agent-b","agent_type":"review"}"#,
            )
            .expect("subagent stop"),
        );
        assert_ne!(started[0].turn, stopped[0].turn);
        assert_ne!(started[0].agent, stopped[0].agent);
        assert!(
            started[0]
                .agent
                .as_ref()
                .is_some_and(|agent| agent.parent().is_some())
        );
    }

    #[test]
    fn sensitive_unknown_fields_are_skipped_before_envelope_creation() {
        let canary = "traex-private-prompt-tool-transcript-message";
        let payload = format!(
            r#"{{"session_id":"s","hook_event_name":"PostToolUseFailure","turn_id":"t","tool_name":"Bash","tool_use_id":"u","cwd":"/{canary}","transcript_path":"/{canary}","thread_name":"{canary}","model":"{canary}","tool_input":{{"command":"{canary}"}},"tool_response":"{canary}","error":"{canary}","last_assistant_message":"{canary}"}}"#
        );
        let outcome = decode("PostToolUseFailure", payload.as_bytes()).expect("decode");
        assert!(!format!("{outcome:?}").contains(canary));
    }

    #[test]
    fn mismatches_duplicates_missing_fields_and_malformed_values_fail_closed() {
        let invalid: &[(&str, &[u8])] = &[
            (
                "SessionEnd",
                br#"{"session_id":"s","hook_event_name":"Stop","reason":"other"}"#,
            ),
            ("SessionEnd", br#"{"hook_event_name":"SessionEnd"}"#),
            (
                "Stop",
                br#"{"session_id":"s","hook_event_name":"Stop"}"#,
            ),
            (
                "PostToolUse",
                br#"{"session_id":"s","hook_event_name":"PostToolUse","turn_id":"t","tool_name":"Bash"}"#,
            ),
            (
                "SessionEnd",
                br#"{"session_id":"s","session_id":"again","hook_event_name":"SessionEnd"}"#,
            ),
            (
                "SessionEnd",
                br#"{"session_id":null,"hook_event_name":"SessionEnd"}"#,
            ),
            (
                "SessionEnd",
                br#"{"session_id":"\uD800","hook_event_name":"SessionEnd"}"#,
            ),
            ("SessionEnd", br#"[]"#),
        ];
        for (event, payload) in invalid {
            assert!(decode(event, payload).is_err(), "accepted invalid {event}");
        }
    }

    #[test]
    fn notification_compaction_and_future_events_are_ignored() {
        for (event, payload) in [
            (
                "Notification",
                br#"{"session_id":"s","hook_event_name":"Notification","turn_id":"t","title":"secret","message":"secret"}"#.as_slice(),
            ),
            (
                "PreCompact",
                br#"{"session_id":"s","hook_event_name":"PreCompact","turn_id":"t","trigger":"auto"}"#.as_slice(),
            ),
            (
                "PostCompact",
                br#"{"session_id":"s","hook_event_name":"PostCompact","turn_id":"t","trigger":"auto"}"#.as_slice(),
            ),
            (
                "FutureEvent",
                br#"{"session_id":"s","hook_event_name":"FutureEvent","private":"secret"}"#.as_slice(),
            ),
        ] {
            assert!(matches!(
                decode(event, payload).expect("bounded ignore"),
                HookDecodeOutcome::Ignore(_)
            ));
        }
    }

    #[test]
    fn permission_occurrences_without_tool_ids_do_not_collide() {
        let adapter = TraexHookAdapter::new();
        let payload = br#"{"session_id":"s","hook_event_name":"PermissionRequest","turn_id":"t","tool_name":"Bash"}"#;
        let decode_at = |millis| {
            adapter
                .decode_hook(
                    AdapterInput {
                        delivery: AdapterDelivery::HookEvent,
                        event_name: "PermissionRequest",
                        observer_version: Some("0.120.46"),
                        observed_at: Timestamp::from_unix_millis(millis),
                        workspace: Some(workspace()),
                        payload,
                    },
                    &identity(),
                )
                .expect("permission")
        };
        let HookDecodeOutcome::Event(first) = decode_at(100) else {
            panic!("event")
        };
        let HookDecodeOutcome::Event(second) = decode_at(101) else {
            panic!("event")
        };
        assert_ne!(first.event_id, second.event_id);
    }

    #[test]
    fn generic_decode_and_wrong_delivery_never_guess_a_payload() {
        let adapter = TraexHookAdapter::new();
        let input = AdapterInput {
            delivery: AdapterDelivery::HookEvent,
            event_name: "SessionEnd",
            observer_version: None,
            observed_at: Timestamp::from_unix_millis(1),
            workspace: Some(workspace()),
            payload: br#"{"session_id":"s","hook_event_name":"SessionEnd"}"#,
        };
        assert!(matches!(
            adapter.decode(input, &identity()).expect("generic ignore"),
            DecodeOutcome::Ignore(_)
        ));
        let wrong = AdapterInput {
            delivery: AdapterDelivery::ProviderEvent,
            event_name: "SessionEnd",
            observer_version: None,
            observed_at: Timestamp::from_unix_millis(1),
            workspace: Some(workspace()),
            payload: br#"{"session_id":"s","hook_event_name":"SessionEnd"}"#,
        };
        assert!(matches!(
            adapter
                .decode_hook(wrong, &identity())
                .expect("wrong delivery ignore"),
            HookDecodeOutcome::Ignore(_)
        ));

        let HookDecodeOutcome::Event(event) = decode(
            "SessionEnd",
            br#"{"session_id":"s","hook_event_name":"SessionEnd"}"#,
        )
        .expect("event") else {
            panic!("event")
        };
        let contract = adapter.contract_template(Some("0.120.46")).hook_contract(
            event.stream.instance.clone(),
            ContractRevision::new(1),
            None,
        );
        contract
            .validate_envelope(&ObservationEnvelope::Event(*event))
            .expect("contract accepts TraeX event");
    }
}
