use super::{
    AgentRef, ArtifactKey, BoundedVec, ObservationError, PresenceRef, SessionRef, Timestamp,
    TurnKey, WorkspaceHint,
};

pub const MAX_SELECTED_WORKSPACES: usize = 32;

/// A bounded set of workspace identities selected in the application.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceSelector {
    workspaces: BoundedVec<WorkspaceHint, MAX_SELECTED_WORKSPACES>,
}

impl WorkspaceSelector {
    pub const fn new(workspaces: BoundedVec<WorkspaceHint, MAX_SELECTED_WORKSPACES>) -> Self {
        Self { workspaces }
    }

    pub fn workspaces(&self) -> &[WorkspaceHint] {
        &self.workspaces
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EvidenceDomain {
    Presence,
    Session,
    Lifecycle,
    Activity,
    Turn,
    Permission,
    Tool,
    AgentTopology,
    Change,
    Artifact,
    Presentation,
    Diagnostic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilitySupport {
    Confirmed,
    Partial,
    Unsupported,
    Unknown,
}

impl CapabilitySupport {
    pub const fn permits(self, actual: Self) -> bool {
        matches!(
            (self, actual),
            (Self::Confirmed, Self::Confirmed | Self::Partial) | (Self::Partial, Self::Partial)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EvidenceAuthority {
    None,
    Observational,
    Authoritative,
}

impl EvidenceAuthority {
    pub const fn permits(self, actual: Self) -> bool {
        actual as u8 <= self as u8
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceProvenance {
    NativeControlPlane,
    InstrumentedHook,
    AggregatedHookAuthority,
    AggregatedScreenInference,
    ProcessPresence,
    VcsInference,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvidenceClaim {
    pub support: CapabilitySupport,
    pub authority: EvidenceAuthority,
    pub provenance: EvidenceProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOp {
    Observed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleOp {
    Set(ReportedSessionLifecycle),
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportedSessionLifecycle {
    Open,
    Ended,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityOp {
    Set(ReportedActivityState),
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportedActivityState {
    Working,
    WaitingPermission,
    Idle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresenceOp {
    Seen,
    Released,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnOp {
    Started,
    Updated,
    Completed,
    Failed,
    UnattributedEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionOp {
    Requested,
    Granted,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOp {
    Started,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentOp {
    Observed,
    Released,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeObservation {
    pub kind: ChangeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKind {
    Document,
    Link,
    Media,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactObservation {
    pub key: ArtifactKey,
    pub kind: ArtifactKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationOp {
    Set,
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticCode {
    UnsupportedEvent,
    MissingStableIdentity,
    PartialCoverage,
    ProviderUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SanitizedDiagnostic {
    pub code: DiagnosticCode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationKind {
    Presence(PresenceOp),
    Session(SessionOp),
    Lifecycle(LifecycleOp),
    Activity(ActivityOp),
    Turn(TurnOp),
    Permission(PermissionOp),
    Tool(ToolOp),
    Agent(AgentOp),
    Change(ChangeObservation),
    Artifact(ArtifactObservation),
    Presentation(PresentationOp),
    Diagnostic(SanitizedDiagnostic),
}

impl ObservationKind {
    pub const fn domain(&self) -> EvidenceDomain {
        match self {
            Self::Presence(_) => EvidenceDomain::Presence,
            Self::Session(_) => EvidenceDomain::Session,
            Self::Lifecycle(_) => EvidenceDomain::Lifecycle,
            Self::Activity(_) => EvidenceDomain::Activity,
            Self::Turn(_) => EvidenceDomain::Turn,
            Self::Permission(_) => EvidenceDomain::Permission,
            Self::Tool(_) => EvidenceDomain::Tool,
            Self::Agent(_) => EvidenceDomain::AgentTopology,
            Self::Change(_) => EvidenceDomain::Change,
            Self::Artifact(_) => EvidenceDomain::Artifact,
            Self::Presentation(_) => EvidenceDomain::Presentation,
            Self::Diagnostic(_) => EvidenceDomain::Diagnostic,
        }
    }

    pub(crate) const fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::Presence(PresenceOp::Released)
                | Self::Lifecycle(LifecycleOp::Clear)
                | Self::Activity(ActivityOp::Clear)
                | Self::Agent(AgentOp::Released)
                | Self::Presentation(PresentationOp::Clear)
        )
    }

    const fn allows_expiry(&self) -> bool {
        matches!(
            self,
            Self::Presence(PresenceOp::Seen)
                | Self::Activity(_)
                | Self::Presentation(PresentationOp::Set)
                | Self::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open))
        )
    }
}

/// The only normalized fact accepted by the core.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentObservation {
    pub observed_at: Timestamp,
    pub valid_until: Option<Timestamp>,
    pub presence: Option<PresenceRef>,
    pub session: Option<SessionRef>,
    pub agent: Option<AgentRef>,
    pub turn: Option<TurnKey>,
    pub workspace: Option<WorkspaceHint>,
    pub kind: ObservationKind,
    pub evidence: EvidenceClaim,
}

impl AgentObservation {
    pub const fn domain(&self) -> EvidenceDomain {
        self.kind.domain()
    }

    pub fn validate_shape(&self) -> Result<(), ObservationError> {
        if matches!(
            self.evidence.support,
            CapabilitySupport::Unsupported | CapabilitySupport::Unknown
        ) || (self.evidence.authority == EvidenceAuthority::None
            && self.domain() != EvidenceDomain::Diagnostic)
        {
            return Err(ObservationError::InvalidEvidenceClaim);
        }

        if let Some(valid_until) = self.valid_until
            && (valid_until <= self.observed_at || !self.kind.allows_expiry())
        {
            return Err(ObservationError::InvalidExpiry);
        }

        if matches!(self.kind, ObservationKind::Presence(_)) && self.presence.is_none() {
            return Err(ObservationError::MissingPresence);
        }
        if matches!(self.kind, ObservationKind::Agent(_)) && self.agent.is_none() {
            return Err(ObservationError::InvalidObservationShape);
        }
        if matches!(
            self.kind,
            ObservationKind::Turn(
                TurnOp::Started | TurnOp::Updated | TurnOp::Completed | TurnOp::Failed
            )
        ) && self.turn.is_none()
        {
            return Err(ObservationError::InvalidObservationShape);
        }

        if self.session.is_none()
            && !matches!(
                self.kind,
                ObservationKind::Presence(_) | ObservationKind::Diagnostic(_)
            )
        {
            return Err(ObservationError::MissingSession);
        }

        if self.session.is_none() && (self.agent.is_some() || self.turn.is_some()) {
            return Err(ObservationError::MissingSession);
        }

        if let Some(session) = &self.session {
            if self
                .agent
                .as_ref()
                .is_some_and(|agent| agent.key().session() != session.key())
                || self
                    .agent
                    .as_ref()
                    .and_then(AgentRef::parent)
                    .is_some_and(|parent| parent.session() != session.key())
                || self
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.session() != session.key())
            {
                return Err(ObservationError::SessionMismatch);
            }

            if self
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace != session.workspace())
            {
                return Err(ObservationError::WorkspaceMismatch);
            }
        }

        if let (Some(presence), Some(workspace)) = (&self.presence, &self.workspace)
            && presence
                .workspace()
                .is_some_and(|presence_workspace| presence_workspace != workspace)
        {
            return Err(ObservationError::WorkspaceMismatch);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AuthorityId, InstallId, SessionKey, StableDigest, SubjectNamespace};

    fn digest(byte: u8) -> StableDigest {
        StableDigest::from_bytes([byte; 32])
    }

    fn session() -> SessionRef {
        SessionRef::new(
            SessionKey::new(
                SubjectNamespace::parse("test/agent").expect("subject"),
                InstallId::from_digest(digest(1)),
                AuthorityId::from_digest(digest(2)),
                digest(3),
            ),
            WorkspaceHint::from_digest(digest(4)),
        )
    }

    fn claim() -> EvidenceClaim {
        EvidenceClaim {
            support: CapabilitySupport::Confirmed,
            authority: EvidenceAuthority::Authoritative,
            provenance: EvidenceProvenance::InstrumentedHook,
        }
    }

    #[test]
    fn session_fact_requires_stable_session_identity() {
        let observation = AgentObservation {
            observed_at: Timestamp::from_unix_millis(1),
            valid_until: None,
            presence: None,
            session: None,
            agent: None,
            turn: None,
            workspace: None,
            kind: ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
            evidence: claim(),
        };
        assert_eq!(
            observation.validate_shape(),
            Err(ObservationError::MissingSession)
        );
    }

    #[test]
    fn workspace_and_session_must_agree() {
        let session = session();
        let observation = AgentObservation {
            observed_at: Timestamp::from_unix_millis(1),
            valid_until: None,
            presence: None,
            session: Some(session),
            agent: None,
            turn: None,
            workspace: Some(WorkspaceHint::from_digest(digest(9))),
            kind: ObservationKind::Session(SessionOp::Observed),
            evidence: claim(),
        };
        assert_eq!(
            observation.validate_shape(),
            Err(ObservationError::WorkspaceMismatch)
        );
    }

    #[test]
    fn expiry_is_restricted_to_lease_like_evidence() {
        let session = session();
        let observation = AgentObservation {
            observed_at: Timestamp::from_unix_millis(10),
            valid_until: Some(Timestamp::from_unix_millis(20)),
            presence: None,
            workspace: Some(session.workspace().clone()),
            session: Some(session),
            agent: None,
            turn: None,
            kind: ObservationKind::Session(SessionOp::Observed),
            evidence: claim(),
        };
        assert_eq!(
            observation.validate_shape(),
            Err(ObservationError::InvalidExpiry)
        );
    }
}
