use super::{
    AgentKey, AgentObservation, ArtifactKey, BoundedSet, BoundedVec, EventId, EvidenceDomain,
    ObservationError, ObserverId, ObserverInstanceId, PresenceRef, SessionKey, SnapshotId,
    StreamEpoch, StreamSequence, SubjectNamespace, Timestamp, TurnKey, WorkspaceHint,
};

pub const MAX_EVENT_OBSERVATIONS: usize = 8;
pub const MAX_SNAPSHOT_OBSERVATIONS: usize = 64;
pub const MAX_SNAPSHOT_SUBJECTS: usize = 32;
pub const MAX_SNAPSHOT_WORKSPACES: usize = 32;
pub const MAX_DELETE_DOMAINS: usize = 8;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamRef {
    pub observer: ObserverId,
    pub instance: ObserverInstanceId,
    pub epoch: StreamEpoch,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ObservedEntityKind {
    Presence,
    Session,
    Agent,
    Turn,
    Artifact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceScope {
    Selected,
    Explicit(BoundedVec<WorkspaceHint, MAX_SNAPSHOT_WORKSPACES>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotScope {
    pub workspaces: WorkspaceScope,
    pub subjects: BoundedSet<SubjectNamespace, MAX_SNAPSHOT_SUBJECTS>,
    pub entity_kinds: BoundedSet<ObservedEntityKind, 5>,
    pub domains: BoundedSet<EvidenceDomain, 12>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotCompleteness {
    Complete,
    Partial,
    Truncated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotEnvelope {
    pub stream: StreamRef,
    pub snapshot_id: SnapshotId,
    pub chunk_index: u16,
    pub final_chunk: bool,
    pub captured_at: Timestamp,
    pub scope: SnapshotScope,
    pub completeness: SnapshotCompleteness,
    pub watermark: Option<StreamSequence>,
    pub observations: BoundedVec<AgentObservation, MAX_SNAPSHOT_OBSERVATIONS>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservedEntityKey {
    Presence(PresenceRef),
    Session(SessionKey),
    Agent(AgentKey),
    Turn(TurnKey),
    Artifact(ArtifactKey),
}

impl ObservedEntityKey {
    pub const fn subject(&self) -> Option<&SubjectNamespace> {
        match self {
            Self::Presence(presence) => presence.subject_hint(),
            Self::Session(session) => Some(session.subject()),
            Self::Agent(agent) => Some(agent.session().subject()),
            Self::Turn(turn) => Some(turn.session().subject()),
            Self::Artifact(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamOp {
    Upsert(BoundedVec<AgentObservation, MAX_EVENT_OBSERVATIONS>),
    Delete {
        entity: ObservedEntityKey,
        domains: BoundedSet<EvidenceDomain, MAX_DELETE_DOMAINS>,
    },
    Reset,
    Gap {
        expected: Option<StreamSequence>,
        received: Option<StreamSequence>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventEnvelope {
    pub stream: StreamRef,
    pub event_id: EventId,
    pub sequence: Option<StreamSequence>,
    pub op: StreamOp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationEnvelope {
    Snapshot(SnapshotEnvelope),
    Event(EventEnvelope),
}

impl ObservationEnvelope {
    pub const fn stream(&self) -> &StreamRef {
        match self {
            Self::Snapshot(snapshot) => &snapshot.stream,
            Self::Event(event) => &event.stream,
        }
    }

    pub fn observations(&self) -> &[AgentObservation] {
        match self {
            Self::Snapshot(snapshot) => &snapshot.observations,
            Self::Event(event) => match &event.op {
                StreamOp::Upsert(observations) => observations,
                StreamOp::Delete { .. } | StreamOp::Reset | StreamOp::Gap { .. } => &[],
            },
        }
    }

    pub fn validate_shape(&self) -> Result<(), ObservationError> {
        match self {
            Self::Snapshot(snapshot) => {
                for observation in &snapshot.observations {
                    observation.validate_shape()?;
                }
            }
            Self::Event(event) => match &event.op {
                StreamOp::Upsert(observations) => {
                    if observations.is_empty() {
                        return Err(ObservationError::EmptyEvent);
                    }
                    for observation in observations {
                        observation.validate_shape()?;
                    }
                }
                StreamOp::Delete { domains, .. } if domains.is_empty() => {
                    return Err(ObservationError::EmptyDeleteDomains);
                }
                StreamOp::Delete { .. } | StreamOp::Reset | StreamOp::Gap { .. } => {}
            },
        }
        Ok(())
    }
}
