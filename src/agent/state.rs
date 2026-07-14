use super::{BoundedVec, SessionKey, SessionMetadataDelta};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewCompleteness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentViewSession {
    pub session: SessionKey,
    pub completeness: ViewCompleteness,
}

/// I/O-free projection accepted by future UI code.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentViewState {
    pub sessions: BoundedVec<AgentViewSession, 256>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyDisposition {
    Applied,
    Duplicate,
    StaleSequence,
    UnsequencedAfterSequenced,
    Expired,
    WrongGeneration,
    WrongEpoch,
    GapDetected,
    AwaitingSnapshot,
    UnsupportedCapability,
    EqualAuthorityConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceExpiryUpdate {
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyResult {
    pub disposition: ApplyDisposition,
    pub changed: bool,
    pub metadata_deltas: BoundedVec<SessionMetadataDelta, 64>,
    pub expiry_updates: BoundedVec<EvidenceExpiryUpdate, 64>,
}

/// C0 reducer shell. Envelope reconciliation is intentionally deferred to C1.
#[derive(Debug, Default)]
pub struct AgentState {
    generation: u64,
    view: AgentViewState,
}

impl AgentState {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            view: AgentViewState {
                sessions: BoundedVec::new(),
            },
        }
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn view(&self) -> AgentViewState {
        self.view.clone()
    }
}
