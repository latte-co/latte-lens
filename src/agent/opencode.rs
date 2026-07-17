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
    StreamOp, StreamRef, StreamSemantics, SubjectNamespace, Timestamp, ToolOp, TurnKey, TurnOp,
    WorkspaceHint, stable_hash,
};

pub const OPENCODE_PLUGIN_OBSERVER_ID: &str = "opencode/plugin";
pub const OPENCODE_SUBJECT_NAMESPACE: &str = "opencode/opencode";
const ADAPTER_VERSION: &str = "1";
const DERIVED_ACTIVITY_LEASE_MILLIS: u64 = 30_000;

/// Official OpenCode plugin-event adapter.
///
/// The companion plugin flattens native events to stable identity and state
/// fields before invoking `latte-lens hook`. Prompt/message text, tool
/// arguments/output, permission patterns/metadata, errors, diffs, titles,
/// model details, and raw paths never cross this adapter boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenCodePluginAdapter;

impl OpenCodePluginAdapter {
    pub const fn new() -> Self {
        Self
    }

    fn observer() -> ObserverId {
        ObserverId::parse(OPENCODE_PLUGIN_OBSERVER_ID).expect("static OpenCode observer id")
    }

    fn subject() -> SubjectNamespace {
        SubjectNamespace::parse(OPENCODE_SUBJECT_NAMESPACE)
            .expect("static OpenCode subject namespace")
    }

    fn instance() -> ObserverInstanceId {
        ObserverInstanceId::from_digest(stable_hash(
            b"opencode-plugin-instance",
            &[OPENCODE_PLUGIN_OBSERVER_ID.as_bytes()],
        ))
    }

    fn epoch() -> StreamEpoch {
        StreamEpoch::from_digest(stable_hash(
            b"opencode-plugin-epoch",
            &[ADAPTER_VERSION.as_bytes()],
        ))
    }

    fn authority() -> AuthorityId {
        AuthorityId::from_digest(stable_hash(
            b"opencode-session-authority",
            &[OPENCODE_SUBJECT_NAMESPACE.as_bytes(), b"sessionID"],
        ))
    }
}

impl CodeAgentAdapter for OpenCodePluginAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        ObserverDescriptor::new(Self::observer(), "OpenCode Plugin", ADAPTER_VERSION)
            .expect("static OpenCode observer descriptor")
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        let subjects = BoundedSet::try_from_iter([Self::subject()]).expect("one OpenCode subject");
        let acquisition =
            BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).expect("one acquisition mode");
        let capabilities = BTreeMap::from([
            (
                EvidenceDomain::Session,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Observational,
                    "Supported OpenCode plugin events carry a native sessionID",
                ),
            ),
            (
                EvidenceDomain::Lifecycle,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                    "session.created and session.deleted are native lifecycle boundaries",
                ),
            ),
            (
                EvidenceDomain::Activity,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                    "session.status explicitly reports busy, retry, and idle",
                ),
            ),
            (
                EvidenceDomain::Turn,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "User message IDs are correlated in plugin memory; there is no turn snapshot",
                ),
            ),
            (
                EvidenceDomain::Permission,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "asked/replied covers interactive requests but not rule-resolved decisions",
                ),
            ),
            (
                EvidenceDomain::Tool,
                capability(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                    "before/after and tool error parts expose start and terminal outcomes",
                ),
            ),
            (
                EvidenceDomain::AgentTopology,
                capability(
                    CapabilitySupport::Partial,
                    EvidenceAuthority::Authoritative,
                    "parentID exposes child session lifecycle without a topology snapshot",
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
        let payload = OpenCodePayload::parse(input.payload)?;
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
        reason: BoundedText::try_new(reason).expect("static OpenCode capability reason"),
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

fn session_ref(
    identity: &dyn IdentityKeyer,
    native_id: &str,
    workspace: WorkspaceHint,
) -> Result<SessionRef, AdapterError> {
    let key = identity
        .session_key(
            &OpenCodePluginAdapter::subject(),
            &OpenCodePluginAdapter::authority(),
            SensitiveId::new(native_id.as_bytes()),
        )
        .map_err(|_| AdapterError::IdentityRejected)?;
    Ok(SessionRef::new(key, workspace))
}

fn decode_event(
    input: AdapterInput<'_>,
    payload: OpenCodePayload,
    workspace: WorkspaceHint,
    identity: &dyn IdentityKeyer,
) -> Result<HookDecodeOutcome, AdapterError> {
    let Some(event) = OpenCodeEvent::parse(input.event_name) else {
        return Ok(HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent));
    };
    event.validate(&payload)?;

    let session = session_ref(identity, &payload.session_id, workspace.clone())?;
    let turn = payload
        .turn_id
        .as_deref()
        .map(|turn_id| {
            identity.turn_key(
                session.key(),
                &OpenCodePluginAdapter::authority(),
                SensitiveId::new(turn_id.as_bytes()),
            )
        })
        .transpose()
        .map_err(|_| AdapterError::IdentityRejected)?;

    let mut observations = BoundedVec::new();
    let mut push = |target_session: SessionRef,
                    agent: Option<AgentRef>,
                    target_turn: Option<TurnKey>,
                    kind,
                    evidence,
                    leased: bool| {
        observations
            .try_push(AgentObservation {
                observed_at: input.observed_at,
                valid_until: leased.then(|| {
                    Timestamp::from_unix_millis(
                        input
                            .observed_at
                            .as_unix_millis()
                            .saturating_add(DERIVED_ACTIVITY_LEASE_MILLIS),
                    )
                }),
                presence: None,
                session: Some(target_session),
                agent,
                turn: target_turn,
                workspace: Some(workspace.clone()),
                kind,
                evidence,
            })
            .map_err(|_| AdapterError::OutputRejected)
    };

    match event {
        OpenCodeEvent::SessionCreated => {
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Session(SessionOp::Observed),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Observational,
                ),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open)),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            if let Some(parent_id) = payload.parent_session_id.as_deref() {
                let parent = session_ref(identity, parent_id, workspace.clone())?;
                let primary = identity
                    .agent_key(parent.key(), SensitiveId::new(b"primary"))
                    .map_err(|_| AdapterError::IdentityRejected)?;
                let child = identity
                    .agent_key(
                        parent.key(),
                        SensitiveId::new(payload.session_id.as_bytes()),
                    )
                    .map_err(|_| AdapterError::IdentityRejected)?;
                push(
                    parent,
                    Some(AgentRef::new(
                        child,
                        Some(primary),
                        Some(AgentKind::Subagent),
                    )),
                    None,
                    ObservationKind::Agent(AgentOp::Observed),
                    claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                    false,
                )?;
            }
        }
        OpenCodeEvent::SessionUpdated => push(
            session.clone(),
            None,
            None,
            ObservationKind::Session(SessionOp::Observed),
            claim(
                CapabilitySupport::Confirmed,
                EvidenceAuthority::Observational,
            ),
            false,
        )?,
        OpenCodeEvent::SessionDeleted => {
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended)),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Clear),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            if let Some(parent_id) = payload.parent_session_id.as_deref() {
                let parent = session_ref(identity, parent_id, workspace.clone())?;
                let primary = identity
                    .agent_key(parent.key(), SensitiveId::new(b"primary"))
                    .map_err(|_| AdapterError::IdentityRejected)?;
                let child = identity
                    .agent_key(
                        parent.key(),
                        SensitiveId::new(payload.session_id.as_bytes()),
                    )
                    .map_err(|_| AdapterError::IdentityRejected)?;
                push(
                    parent,
                    Some(AgentRef::new(
                        child,
                        Some(primary),
                        Some(AgentKind::Subagent),
                    )),
                    None,
                    ObservationKind::Agent(AgentOp::Released),
                    claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                    false,
                )?;
            }
        }
        OpenCodeEvent::SessionStatus => {
            let activity = match payload.status.as_deref() {
                Some("busy" | "retry") => ReportedActivityState::Working,
                Some("idle") => ReportedActivityState::Idle,
                _ => return Err(AdapterError::MalformedInput),
            };
            if activity == ReportedActivityState::Idle
                && let Some(turn) = turn.clone()
            {
                push(
                    session.clone(),
                    None,
                    Some(turn),
                    ObservationKind::Turn(TurnOp::Completed),
                    claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                    false,
                )?;
            }
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Set(activity)),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
        }
        OpenCodeEvent::MessageUpdated => {
            push(
                session.clone(),
                None,
                turn.clone(),
                ObservationKind::Turn(TurnOp::Started),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        OpenCodeEvent::SessionError => {
            push(
                session.clone(),
                None,
                turn.clone(),
                ObservationKind::Turn(TurnOp::Failed),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        OpenCodeEvent::PermissionAsked => {
            push(
                session.clone(),
                None,
                turn.clone(),
                ObservationKind::Permission(PermissionOp::Requested),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Set(
                    ReportedActivityState::WaitingPermission,
                )),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        OpenCodeEvent::PermissionReplied => {
            let operation = if payload.reply.as_deref() == Some("reject") {
                PermissionOp::Denied
            } else {
                PermissionOp::Granted
            };
            push(
                session.clone(),
                None,
                turn.clone(),
                ObservationKind::Permission(operation),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        OpenCodeEvent::ToolBefore => {
            push(
                session.clone(),
                None,
                turn.clone(),
                ObservationKind::Tool(ToolOp::Started),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
        OpenCodeEvent::ToolAfter | OpenCodeEvent::ToolError => {
            let operation = if event == OpenCodeEvent::ToolAfter {
                ToolOp::Completed
            } else {
                ToolOp::Failed
            };
            push(
                session.clone(),
                None,
                turn.clone(),
                ObservationKind::Tool(operation),
                claim(
                    CapabilitySupport::Confirmed,
                    EvidenceAuthority::Authoritative,
                ),
                false,
            )?;
            push(
                session.clone(),
                None,
                None,
                ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
                claim(CapabilitySupport::Partial, EvidenceAuthority::Authoritative),
                true,
            )?;
        }
    }

    let observer = OpenCodePluginAdapter::observer();
    let instance = OpenCodePluginAdapter::instance();
    let epoch = OpenCodePluginAdapter::epoch();
    let mut event_identity = Vec::with_capacity(256);
    append_identity_part(&mut event_identity, payload.hook_event_name.as_bytes());
    append_identity_part(&mut event_identity, payload.session_id.as_bytes());
    append_identity_part(&mut event_identity, payload.event_id.as_bytes());
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
enum OpenCodeEvent {
    SessionCreated,
    SessionUpdated,
    SessionDeleted,
    SessionStatus,
    MessageUpdated,
    SessionError,
    PermissionAsked,
    PermissionReplied,
    ToolBefore,
    ToolAfter,
    ToolError,
}

impl OpenCodeEvent {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "session.created" => Some(Self::SessionCreated),
            "session.updated" => Some(Self::SessionUpdated),
            "session.deleted" => Some(Self::SessionDeleted),
            "session.status" => Some(Self::SessionStatus),
            "message.updated" => Some(Self::MessageUpdated),
            "session.error" => Some(Self::SessionError),
            "permission.asked" => Some(Self::PermissionAsked),
            "permission.replied" => Some(Self::PermissionReplied),
            "tool.execute.before" => Some(Self::ToolBefore),
            "tool.execute.after" => Some(Self::ToolAfter),
            "message.part.updated" => Some(Self::ToolError),
            _ => None,
        }
    }

    fn validate(self, payload: &OpenCodePayload) -> Result<(), AdapterError> {
        let require = |value: Option<&String>| {
            value
                .filter(|value| !value.is_empty())
                .map(|_| ())
                .ok_or(AdapterError::MalformedInput)
        };
        if payload
            .parent_session_id
            .as_ref()
            .is_some_and(|parent| parent == &payload.session_id)
        {
            return Err(AdapterError::MalformedInput);
        }
        match self {
            Self::SessionCreated | Self::SessionUpdated | Self::SessionDeleted => {}
            Self::SessionStatus => match payload.status.as_deref() {
                Some("busy" | "retry" | "idle") => {}
                _ => return Err(AdapterError::MalformedInput),
            },
            Self::MessageUpdated | Self::SessionError => require(payload.turn_id.as_ref())?,
            Self::PermissionAsked => require(payload.permission_id.as_ref())?,
            Self::PermissionReplied => {
                require(payload.permission_id.as_ref())?;
                match payload.reply.as_deref() {
                    Some("once" | "always" | "reject") => {}
                    _ => return Err(AdapterError::MalformedInput),
                }
            }
            Self::ToolBefore | Self::ToolAfter => {
                require(payload.tool_call_id.as_ref())?;
                require(payload.tool_name.as_ref())?;
            }
            Self::ToolError => {
                require(payload.tool_call_id.as_ref())?;
                require(payload.tool_name.as_ref())?;
                if payload.status.as_deref() != Some("error") {
                    return Err(AdapterError::MalformedInput);
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct OpenCodePayload {
    session_id: String,
    hook_event_name: String,
    event_id: String,
    parent_session_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    permission_id: Option<String>,
    status: Option<String>,
    reply: Option<String>,
}

impl OpenCodePayload {
    fn parse(bytes: &[u8]) -> Result<Self, AdapterError> {
        std::str::from_utf8(bytes).map_err(|_| AdapterError::MalformedInput)?;
        let mut parser = HookJsonParser::new(bytes);
        let mut payload = Self::default();
        let mut session_id_seen = false;
        let mut hook_event_name_seen = false;
        let mut event_id_seen = false;
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
            "event_id" => set_required_once(
                &mut payload.event_id,
                &mut event_id_seen,
                parser.parse_bounded_string(),
            ),
            "parent_session_id" => set_optional_once(
                &mut payload.parent_session_id,
                parser.parse_bounded_string(),
            ),
            "turn_id" => set_optional_once(&mut payload.turn_id, parser.parse_bounded_string()),
            "tool_call_id" => {
                set_optional_once(&mut payload.tool_call_id, parser.parse_bounded_string())
            }
            "tool_name" => set_optional_once(&mut payload.tool_name, parser.parse_bounded_string()),
            "permission_id" => {
                set_optional_once(&mut payload.permission_id, parser.parse_bounded_string())
            }
            "status" => set_optional_once(&mut payload.status, parser.parse_bounded_string()),
            "reply" => set_optional_once(&mut payload.reply, parser.parse_bounded_string()),
            _ => parser.skip_value(1),
        })?;
        parser.finish()?;
        if payload.session_id.is_empty()
            || payload.hook_event_name.is_empty()
            || payload.event_id.is_empty()
        {
            return Err(AdapterError::MalformedInput);
        }
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ContractRevision, HmacIdentityKeyer, ObservationEnvelope};

    fn identity() -> HmacIdentityKeyer {
        HmacIdentityKeyer::new(SensitiveId::new(&[0x5a; 32])).expect("identity")
    }

    fn workspace() -> WorkspaceHint {
        WorkspaceHint::from_digest(stable_hash(b"test-workspace", &[b"opencode"]))
    }

    fn decode(event_name: &str, payload: &[u8]) -> Result<HookDecodeOutcome, AdapterError> {
        OpenCodePluginAdapter::new().decode_hook(
            AdapterInput {
                delivery: AdapterDelivery::HookEvent,
                event_name,
                observer_version: Some("1.15.11"),
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
        observations.into_iter().collect()
    }

    #[test]
    fn contract_declares_native_status_and_terminal_tool_evidence() {
        let template = OpenCodePluginAdapter::new().contract_template(Some("1.15.11"));
        assert_eq!(template.observer.as_str(), OPENCODE_PLUGIN_OBSERVER_ID);
        assert_eq!(
            template.capabilities[&EvidenceDomain::Activity].support,
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
        assert_eq!(template.stability, InterfaceStability::Stable);
    }

    #[test]
    fn child_session_creation_emits_lifecycle_and_topology_without_titles() {
        let payload = br#"{"session_id":"ses_child","parent_session_id":"ses_parent","hook_event_name":"session.created","event_id":"bridge:1","title":"private title","directory":"/private/path","summary":{"diffs":[{"file":"secret"}]}}"#;
        let observations = observations(decode("session.created", payload).expect("decode"));
        assert_eq!(observations.len(), 3);
        assert!(matches!(
            observations[0].kind,
            ObservationKind::Session(SessionOp::Observed)
        ));
        assert!(matches!(
            observations[1].kind,
            ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open))
        ));
        assert!(matches!(
            observations[2].kind,
            ObservationKind::Agent(AgentOp::Observed)
        ));
        assert_ne!(
            observations[0].session.as_ref().expect("child").key(),
            observations[2].session.as_ref().expect("parent").key()
        );
        observations
            .iter()
            .for_each(|observation| observation.validate_shape().expect("shape"));
    }

    #[test]
    fn status_is_authoritative_and_idle_completes_a_correlated_turn() {
        let busy = observations(
            decode(
                "session.status",
                br#"{"session_id":"ses_1","hook_event_name":"session.status","event_id":"bridge:2","status":"retry"}"#,
            )
            .expect("busy"),
        );
        assert_eq!(busy.len(), 1);
        assert!(matches!(
            busy[0].kind,
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working))
        ));
        assert_eq!(busy[0].evidence.support, CapabilitySupport::Confirmed);

        let idle = observations(
            decode(
                "session.status",
                br#"{"session_id":"ses_1","hook_event_name":"session.status","event_id":"bridge:3","status":"idle","turn_id":"msg_1"}"#,
            )
            .expect("idle"),
        );
        assert_eq!(idle.len(), 2);
        assert!(matches!(
            idle[0].kind,
            ObservationKind::Turn(TurnOp::Completed)
        ));
        assert!(matches!(
            idle[1].kind,
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle))
        ));
    }

    #[test]
    fn permission_reply_preserves_granted_and_denied_terminals() {
        for (reply, expected) in [
            ("once", PermissionOp::Granted),
            ("always", PermissionOp::Granted),
            ("reject", PermissionOp::Denied),
        ] {
            let payload = format!(
                r#"{{"session_id":"ses_1","hook_event_name":"permission.replied","event_id":"bridge:{reply}","permission_id":"per_1","reply":"{reply}","metadata":{{"secret":"private"}}}}"#
            );
            let observations =
                observations(decode("permission.replied", payload.as_bytes()).expect("permission"));
            assert!(matches!(
                observations[0].kind,
                ObservationKind::Permission(operation) if operation == expected
            ));
        }
    }

    #[test]
    fn tool_hooks_and_error_parts_keep_all_terminal_outcomes() {
        for (event, status, expected) in [
            ("tool.execute.before", None, ToolOp::Started),
            ("tool.execute.after", None, ToolOp::Completed),
            ("message.part.updated", Some("error"), ToolOp::Failed),
        ] {
            let status = status.map_or(String::new(), |value| format!(r#", "status":"{value}""#));
            let payload = format!(
                r#"{{"session_id":"ses_1","hook_event_name":"{event}","event_id":"bridge:{event}","tool_call_id":"call_1","tool_name":"bash"{status},"input":{{"command":"private"}},"output":"private","error":"private"}}"#
            );
            let observations = observations(decode(event, payload.as_bytes()).expect("tool"));
            assert!(matches!(
                observations[0].kind,
                ObservationKind::Tool(operation) if operation == expected
            ));
        }
    }

    #[test]
    fn session_error_fails_only_a_correlated_turn_not_the_session_lifecycle() {
        let observations = observations(
            decode(
                "session.error",
                br#"{"session_id":"ses_1","hook_event_name":"session.error","event_id":"bridge:error","turn_id":"msg_1","error":{"message":"private"}}"#,
            )
            .expect("error"),
        );
        assert_eq!(observations.len(), 2);
        assert!(matches!(
            observations[0].kind,
            ObservationKind::Turn(TurnOp::Failed)
        ));
        assert!(
            !observations
                .iter()
                .any(|item| matches!(item.kind, ObservationKind::Lifecycle(_)))
        );
    }

    #[test]
    fn unsupported_diff_and_duplicate_identity_fields_are_rejected_safely() {
        let ignored = decode(
            "session.diff",
            br#"{"session_id":"ses_1","hook_event_name":"session.diff","event_id":"bridge:diff","diff":[{"file":"private"}]}"#,
        )
        .expect("unsupported event");
        assert_eq!(
            ignored,
            HookDecodeOutcome::Ignore(IgnoreReason::UnsupportedEvent)
        );
        assert_eq!(
            decode(
                "session.updated",
                br#"{"session_id":"ses_1","session_id":"ses_2","hook_event_name":"session.updated","event_id":"bridge:4"}"#,
            ),
            Err(AdapterError::MalformedInput)
        );
    }

    #[test]
    fn distinct_bridge_occurrences_produce_distinct_valid_event_ids() {
        let first = decode(
            "session.updated",
            br#"{"session_id":"ses_1","hook_event_name":"session.updated","event_id":"bridge:5"}"#,
        )
        .expect("first");
        let second = decode(
            "session.updated",
            br#"{"session_id":"ses_1","hook_event_name":"session.updated","event_id":"bridge:6"}"#,
        )
        .expect("second");
        let (HookDecodeOutcome::Event(first), HookDecodeOutcome::Event(second)) = (first, second)
        else {
            panic!("events")
        };
        assert_ne!(first.event_id, second.event_id);
        let contract = OpenCodePluginAdapter::new()
            .contract_template(Some("1.15.11"))
            .hook_contract(
                first.stream.instance.clone(),
                ContractRevision::new(1),
                Some(BoundedText::try_new("1.15.11").expect("version")),
            );
        contract
            .validate_envelope(&ObservationEnvelope::Event(*first))
            .expect("valid envelope");
    }
}
