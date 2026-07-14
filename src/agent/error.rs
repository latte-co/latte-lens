use std::{error::Error, fmt};

/// Safe, payload-free validation failures at the adapter/core boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationError {
    UnknownObserver,
    ObserverMismatch,
    InstanceMismatch,
    WrongEpoch,
    StaleContractRevision,
    InvalidObservationShape,
    MissingSession,
    MissingPresence,
    SessionMismatch,
    WorkspaceMismatch,
    InvalidExpiry,
    InvalidEvidenceClaim,
    UnsupportedSubject,
    UnsupportedCapability,
    AuthorityExceeded,
    ProvenanceMismatch,
    DestructiveOperationDenied,
    InvalidSnapshotScope,
    EmptyEvent,
    EmptyDeleteDomains,
}

impl fmt::Display for ObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "observation rejected: {self:?}")
    }
}

impl Error for ObservationError {}
