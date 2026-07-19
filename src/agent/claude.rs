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

pub const CLAUDE_HOOK_OBSERVER_ID: &str = "anthropic/claude-code-hook";
pub const CLAUDE_SUBJECT_NAMESPACE: &str = "anthropic/claude-code";
const ADAPTER_VERSION: &str = "1";
const ACTIVITY_LEASE_MILLIS: u64 = 30_000;

/// Official Claude Code command-hook adapter.
///
/// The adapter reads only native identity and event-shape fields. Prompt text,
/// tool input/output, error details, assistant messages, transcript paths,
/// model details, permission mode, and raw cwd are skipped by the bounded JSON
/// reader and never enter core, IPC, metadata, logs, or UI state.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeHookAdapter;

impl ClaudeHookAdapter {
    pub const fn new() -> Self {
        Self
    }

    fn observer() -> ObserverId {
        ObserverId::parse(CLAUDE_HOOK_OBSERVER_ID).expect("static Claude observer id")
    }

    fn subject() -> SubjectNamespace {
        SubjectNamespace::parse(CLAUDE_SUBJECT_NAMESPACE).expect("static Claude subject namespace")
    }

    fn instance() -> ObserverInstanceId {
        ObserverInstanceId::from_digest(stable_hash(
            b"claude-hook-instance",
            &[CLAUDE_HOOK_OBSERVER_ID.as_bytes()],
        ))
    }

    fn epoch() -> StreamEpoch {
        StreamEpoch::from_digest(stable_hash(
            b"claude-hook-epoch",
            &[ADAPTER_VERSION.as_bytes()],
        ))
    }

    fn authority() -> AuthorityId {
        AuthorityId::from_digest(stable_hash(
            b"claude-session-authority",
            &[CLAUDE_SUBJECT_NAMESPACE.as_bytes(), b"session_id"],
        ))
    }
}

impl CodeAgentAdapter for ClaudeHookAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        ObserverDescriptor::new(Self::observer(), "Claude Code Hooks", ADAPTER_VERSION)
            .expect("static Claude observer descriptor")
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        let subjects = BoundedSet::try_from_iter([Self::subject()]).expect("one Claude subject");
        let acquisition =
            BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).expect("one acquisition mode");
        let capabilities = BTreeMap::from([
            (
                EvidenceDomain::Session,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Observational,
                    "Every supported Claude Hook carries session_id",
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
                    "Activity is leased event evidence; there is no current-state snapshot",
                ),
            ),
            (
                EvidenceDomain::Turn,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "prompt_id is version-gated and missed turns cannot be recovered",
                ),
            ),
            (
                EvidenceDomain::Permission,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "Request and auto-mode denial are visible; user resolution is not",
                ),
            ),
            (
                EvidenceDomain::Tool,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                    "Pre, successful Post, and failed Post identify documented tool outcomes",
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
            stability: InterfaceStability::Stable,
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
        let payload = ClaudePayload::parse(input.payload)?;
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
        reason: BoundedText::try_new(reason).expect("static Claude capability reason"),
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
    payload: ClaudePayload,
    workspace: WorkspaceHint,
    identity: &dyn IdentityKeyer,
) -> Result<HookDecodeOutcome, AdapterError> {
    let Some(event) = ClaudeEvent::parse(input.event_name) else {
        return Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent));
    };
    if matches!(event, ClaudeEvent::PreCompact | ClaudeEvent::PostCompact) {
        return Ok(HookDecodeOutcome::Ignore(IgnoreReason::NoObservableFact));
    }
    event.validate(&payload)?;

    let subject = ClaudeHookAdapter::subject();
    let authority = ClaudeHookAdapter::authority();
    let session_key = identity
        .session_key(
            &subject,
            &authority,
            SensitiveId::new(payload.session_id.as_bytes()),
        )
        .map_err(|_| AdapterError::IdentityRejected)?;
    let session = SessionRef::new(session_key, workspace.clone());
    let turn = payload
        .prompt_id
        .as_deref()
        .map(|prompt_id| {
            identity.turn_key(
                session.key(),
                &authority,
                SensitiveId::new(prompt_id.as_bytes()),
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
        ClaudeEvent::SessionStart => {
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
        ClaudeEvent::UserPromptSubmit => {
            push(
                ObservationKind::Turn(
                    turn.as_ref()
                        .map_or(TurnOp::UnattributedEvidence, |_| TurnOp::Started),
                ),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        ClaudeEvent::PreToolUse => {
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
        ClaudeEvent::PermissionRequest => {
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
        ClaudeEvent::PermissionDenied => {
            push(
                ObservationKind::Permission(PermissionOp::Denied),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        ClaudeEvent::PostToolUse => {
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
        ClaudeEvent::PostToolUseFailure => {
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
        ClaudeEvent::SubagentStart => push(
            ObservationKind::Agent(AgentOp::Observed),
            claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
            false,
        )?,
        ClaudeEvent::SubagentStop => push(
            ObservationKind::Agent(AgentOp::Released),
            claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
            false,
        )?,
        ClaudeEvent::Stop => {
            if turn.is_some() {
                push(
                    ObservationKind::Turn(TurnOp::Completed),
                    claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                    false,
                )?;
            }
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        ClaudeEvent::StopFailure => {
            if turn.is_some() {
                push(
                    ObservationKind::Turn(TurnOp::Failed),
                    claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                    false,
                )?;
            }
            push(
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        ClaudeEvent::SessionEnd => {
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
        ClaudeEvent::PreCompact | ClaudeEvent::PostCompact => unreachable!("ignored above"),
    }

    let observer = ClaudeHookAdapter::observer();
    let instance = ClaudeHookAdapter::instance();
    let epoch = ClaudeHookAdapter::epoch();
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
enum ClaudeEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PermissionDenied,
    PostToolUse,
    PostToolUseFailure,
    SubagentStart,
    SubagentStop,
    Stop,
    StopFailure,
    SessionEnd,
    PreCompact,
    PostCompact,
}

impl ClaudeEvent {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "SessionStart" => Some(Self::SessionStart),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "PreToolUse" => Some(Self::PreToolUse),
            "PermissionRequest" => Some(Self::PermissionRequest),
            "PermissionDenied" => Some(Self::PermissionDenied),
            "PostToolUse" => Some(Self::PostToolUse),
            "PostToolUseFailure" => Some(Self::PostToolUseFailure),
            "SubagentStart" => Some(Self::SubagentStart),
            "SubagentStop" => Some(Self::SubagentStop),
            "Stop" => Some(Self::Stop),
            "StopFailure" => Some(Self::StopFailure),
            "SessionEnd" => Some(Self::SessionEnd),
            "PreCompact" => Some(Self::PreCompact),
            "PostCompact" => Some(Self::PostCompact),
            _ => None,
        }
    }

    fn validate(self, payload: &ClaudePayload) -> Result<(), AdapterError> {
        let require = |value: Option<&String>| {
            value
                .filter(|value| !value.is_empty())
                .map(|_| ())
                .ok_or(AdapterError::MalformedInput)
        };
        match self {
            Self::SessionStart => require(payload.source.as_ref())?,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure => {
                require(payload.tool_name.as_ref())?;
                require(payload.tool_use_id.as_ref())?;
            }
            Self::PermissionRequest => require(payload.tool_name.as_ref())?,
            Self::PermissionDenied => {
                require(payload.tool_name.as_ref())?;
                require(payload.tool_use_id.as_ref())?;
            }
            Self::SubagentStart | Self::SubagentStop => {
                require(payload.agent_id.as_ref())?;
                require(payload.agent_type.as_ref())?;
            }
            Self::StopFailure | Self::SessionEnd => {}
            Self::PreCompact | Self::PostCompact => require(payload.trigger.as_ref())?,
            Self::UserPromptSubmit | Self::Stop => {}
        }
        Ok(())
    }
}

#[derive(Default)]
struct ClaudePayload {
    session_id: String,
    hook_event_name: String,
    prompt_id: Option<String>,
    tool_use_id: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    source: Option<String>,
    trigger: Option<String>,
    tool_name: Option<String>,
}

impl ClaudePayload {
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
            "prompt_id" => set_optional_once(&mut payload.prompt_id, parser.parse_bounded_string()),
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

    fn event_identity(&self, event: ClaudeEvent, observed_at: Timestamp) -> Vec<u8> {
        let mut output = Vec::with_capacity(256);
        append_identity_part(&mut output, self.hook_event_name.as_bytes());
        append_identity_part(&mut output, self.session_id.as_bytes());
        for value in [
            self.prompt_id.as_deref(),
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
        if event == ClaudeEvent::PermissionRequest && self.tool_use_id.is_none() {
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
        HmacIdentityKeyer::new(SensitiveId::new(&[0x6b; 32])).expect("identity")
    }

    fn workspace() -> WorkspaceHint {
        WorkspaceHint::from_digest(stable_hash(b"test-workspace", &[b"claude"]))
    }

    fn decode(event_name: &str, payload: &[u8]) -> Result<HookDecodeOutcome, AdapterError> {
        ClaudeHookAdapter::new().decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name,
                observer_version: Some("2.1.200"),
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
    fn production_contract_matches_the_documented_claude_hook_boundary() {
        let adapter = ClaudeHookAdapter::new();
        assert_eq!(adapter.descriptor().id.as_str(), CLAUDE_HOOK_OBSERVER_ID);
        let template = adapter.contract_template(Some("2.1.200"));
        assert_eq!(template.subjects.iter().count(), 1);
        assert!(template.acquisition.contains(&AcquisitionMode::HookEvent));
        for domain in [
            EvidenceDomain::Session,
            EvidenceDomain::Lifecycle,
            EvidenceDomain::Tool,
        ] {
            assert_eq!(
                template.capabilities[&domain].support,
                CapabilitySupport::Confirmed
            );
        }
        for domain in [
            EvidenceDomain::Activity,
            EvidenceDomain::Turn,
            EvidenceDomain::Permission,
            EvidenceDomain::AgentTopology,
        ] {
            assert_eq!(
                template.capabilities[&domain].support,
                CapabilitySupport::Partial
            );
            assert!(!template.capabilities[&domain].reason.as_str().is_empty());
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
        assert_eq!(template.stability, InterfaceStability::Stable);
    }

    #[test]
    fn official_hook_shapes_map_to_bounded_core_facts() {
        type HookCase<'a> = (&'a str, &'a [u8], fn(&ObservationKind) -> bool);
        let cases: [HookCase<'_>; 12] = [
            (
                "SessionStart",
                br#"{"session_id":"s-1","transcript_path":"/private","cwd":"/repo","hook_event_name":"SessionStart","source":"startup","model":"claude"}"#,
                |kind| matches!(kind, ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open))),
            ),
            (
                "UserPromptSubmit",
                br#"{"session_id":"s-1","prompt_id":"p-1","transcript_path":"/private","cwd":"/repo","hook_event_name":"UserPromptSubmit","prompt":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Turn(TurnOp::Started)),
            ),
            (
                "PreToolUse",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"PreToolUse","tool_name":"Bash","tool_use_id":"tool-1","tool_input":{"command":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Started)),
            ),
            (
                "PermissionRequest",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Permission(PermissionOp::Requested)),
            ),
            (
                "PermissionDenied",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"PermissionDenied","tool_name":"Bash","tool_use_id":"tool-1","reason":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Permission(PermissionOp::Denied)),
            ),
            (
                "PostToolUse",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"tool-1","tool_input":{},"tool_response":{"output":"secret"}}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Completed)),
            ),
            (
                "PostToolUseFailure",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_use_id":"tool-1","error":"secret","error_details":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Tool(ToolOp::Failed)),
            ),
            (
                "SubagentStart",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"SubagentStart","agent_id":"a-1","agent_type":"Explore"}"#,
                |kind| matches!(kind, ObservationKind::Agent(AgentOp::Observed)),
            ),
            (
                "SubagentStop",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"SubagentStop","agent_id":"a-1","agent_type":"Explore","last_assistant_message":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Agent(AgentOp::Released)),
            ),
            (
                "Stop",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"Stop","last_assistant_message":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Turn(TurnOp::Completed)),
            ),
            (
                "StopFailure",
                br#"{"session_id":"s-1","prompt_id":"p-1","cwd":"/repo","hook_event_name":"StopFailure","error":"rate_limit","error_details":"secret"}"#,
                |kind| matches!(kind, ObservationKind::Turn(TurnOp::Failed)),
            ),
            (
                "SessionEnd",
                br#"{"session_id":"s-1","cwd":"/repo","hook_event_name":"SessionEnd","reason":"other"}"#,
                |kind| matches!(kind, ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended))),
            ),
        ];

        for (event_name, payload, expected) in cases {
            let facts = observations(decode(event_name, payload).expect(event_name));
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
    fn lifecycle_and_tool_outcomes_use_claude_specific_authority() {
        let ended = observations(
            decode(
                "SessionEnd",
                br#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"clear"}"#,
            )
            .expect("session end"),
        );
        assert!(ended.iter().any(|fact| matches!(
            fact.kind,
            ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended))
        ) && fact.evidence.support
            == CapabilitySupport::Confirmed));
        assert!(
            ended
                .iter()
                .any(|fact| matches!(fact.kind, ObservationKind::Activity(ActivityOp::Clear)))
        );

        let failed = observations(
            decode(
                "PostToolUseFailure",
                br#"{"session_id":"s","hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_use_id":"u","error":"failed"}"#,
            )
            .expect("tool failure"),
        );
        assert!(failed.iter().any(|fact| matches!(
            fact.kind,
            ObservationKind::Tool(ToolOp::Failed)
        ) && fact.evidence.support
            == CapabilitySupport::Confirmed));
    }

    #[test]
    fn prompt_id_is_optional_without_inventing_a_turn_identity() {
        let prompt = observations(
            decode(
                "UserPromptSubmit",
                br#"{"session_id":"s","hook_event_name":"UserPromptSubmit","prompt":"secret"}"#,
            )
            .expect("legacy prompt"),
        );
        assert!(prompt.iter().any(|fact| matches!(
            fact.kind,
            ObservationKind::Turn(TurnOp::UnattributedEvidence)
        ) && fact.turn.is_none()));

        let stopped = observations(
            decode(
                "Stop",
                br#"{"session_id":"s","hook_event_name":"Stop","last_assistant_message":"secret"}"#,
            )
            .expect("legacy stop"),
        );
        assert!(!stopped.iter().any(|fact| matches!(
            fact.kind,
            ObservationKind::Turn(TurnOp::Completed | TurnOp::Failed)
        )));
        assert!(stopped.iter().any(|fact| matches!(
            fact.kind,
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle))
        ) && fact.valid_until
            == Some(Timestamp::from_unix_millis(30_100))));
    }

    #[test]
    fn sensitive_unknown_fields_are_skipped_and_never_reach_the_envelope() {
        let canary = "privacy-canary-claude-prompt-tool-transcript";
        let payload = format!(
            r#"{{"session_id":"s","prompt_id":"p","hook_event_name":"PostToolUseFailure","tool_name":"mcp__fs__read","tool_use_id":"u","cwd":"/{canary}","transcript_path":"/{canary}","tool_input":{{"nested":[true,false,null,-1.25e2,"{canary}"]}},"error":"failure","error_details":"{canary}","last_assistant_message":"{canary}"}}"#
        );
        let HookDecodeOutcome::Event(event) =
            decode("PostToolUseFailure", payload.as_bytes()).expect("decode")
        else {
            panic!("expected event")
        };
        assert!(!format!("{event:?}").contains(canary));
        let adapter = ClaudeHookAdapter::new();
        let contract = adapter.contract_template(None).hook_contract(
            event.stream.instance.clone(),
            ContractRevision::new(1),
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
                "SessionEnd",
                br#"{"session_id":"s","hook_event_name":"Stop","reason":"other"}"#,
            ),
            ("SessionEnd", br#"{"hook_event_name":"SessionEnd","reason":"other"}"#),
            (
                "PostToolUseFailure",
                br#"{"session_id":"s","hook_event_name":"PostToolUseFailure","tool_name":"Bash"}"#,
            ),
            (
                "SessionEnd",
                br#"{"session_id":"s","session_id":"again","hook_event_name":"SessionEnd","reason":"other"}"#,
            ),
            (
                "SessionEnd",
                br#"{"session_id":null,"hook_event_name":"SessionEnd","reason":"other"}"#,
            ),
            (
                "SessionEnd",
                br#"{"session_id":"\uD800","hook_event_name":"SessionEnd","reason":"other"}"#,
            ),
            (
                "SessionEnd",
                br#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"\uD800"}"#,
            ),
            ("SessionEnd", br#"[]"#),
        ];
        for (event_name, payload) in cases {
            assert_eq!(
                decode(event_name, payload),
                Err(AdapterError::MalformedInput)
            );
        }
    }

    #[test]
    fn compaction_and_future_events_are_ignored() {
        for event_name in ["PreCompact", "PostCompact"] {
            let payload = format!(
                r#"{{"session_id":"s","hook_event_name":"{event_name}","trigger":"auto","compact_summary":"secret"}}"#
            );
            assert_eq!(
                decode(event_name, payload.as_bytes()),
                Ok(HookDecodeOutcome::Ignore(IgnoreReason::NoObservableFact))
            );
        }
        assert_eq!(
            decode(
                "FutureHook",
                br#"{"session_id":"s","hook_event_name":"FutureHook"}"#,
            ),
            Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent))
        );
    }

    #[test]
    fn generic_decode_and_wrong_delivery_never_guess_a_payload() {
        let adapter = ClaudeHookAdapter::new();
        let input = AdapterInput {
            delivery: AdapterDelivery::ProviderEvent,
            event_name: "SessionEnd",
            observer_version: None,
            observed_at: Timestamp::from_unix_millis(1),
            workspace: Some(workspace()),
            payload: br#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"other"}"#,
        };
        assert_eq!(
            adapter.decode(input, &identity()),
            Ok(DecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent))
        );
        let input = AdapterInput {
            delivery: AdapterDelivery::ProviderEvent,
            event_name: "SessionEnd",
            observer_version: None,
            observed_at: Timestamp::from_unix_millis(1),
            workspace: Some(workspace()),
            payload: br#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"other"}"#,
        };
        assert_eq!(
            adapter.decode_hook(input, &identity()),
            Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent))
        );
    }
}
