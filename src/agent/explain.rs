use super::{
    ActivityState, BoundedVec, EvidenceAuthority, EvidenceDomain, EvidenceProvenance, ObserverId,
    SessionLifecycle, Timestamp,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DecisionValue {
    Unknown,
    Lifecycle(SessionLifecycle),
    Activity(ActivityState),
    Present,
    Count(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecisionDisposition {
    Applied,
    Expired,
    Suppressed,
    StaleSequence,
    WrongEpoch,
    AwaitingSnapshot,
    UnsupportedCapability,
    EqualAuthorityConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompetingEvidenceSummary {
    pub observer: ObserverId,
    pub domain: EvidenceDomain,
    pub authority: EvidenceAuthority,
    pub current: bool,
    pub disposition: DecisionDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionTrace {
    pub domain: EvidenceDomain,
    pub effective_value: DecisionValue,
    pub winning_observer: Option<ObserverId>,
    pub authority: EvidenceAuthority,
    pub provenance: Option<EvidenceProvenance>,
    pub observed_at: Option<Timestamp>,
    pub valid_until: Option<Timestamp>,
    pub disposition: DecisionDisposition,
    pub competing: BoundedVec<CompetingEvidenceSummary, 4>,
}

impl DecisionTrace {
    pub fn unknown(domain: EvidenceDomain, disposition: DecisionDisposition) -> Self {
        Self {
            domain,
            effective_value: DecisionValue::Unknown,
            winning_observer: None,
            authority: EvidenceAuthority::None,
            provenance: None,
            observed_at: None,
            valid_until: None,
            disposition,
            competing: BoundedVec::new(),
        }
    }
}
