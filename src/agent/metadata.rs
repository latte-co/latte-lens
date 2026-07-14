use std::{error::Error, fmt, time::Instant};

use super::{BoundedVec, SessionRef, Timestamp, WorkspaceHint, WorkspaceSelector};

pub const MAX_METADATA_WORKSPACES: usize = 256;
pub const MAX_METADATA_SESSIONS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataLoadLimits {
    pub max_workspaces: usize,
    pub max_sessions: usize,
    pub max_total_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMetadata {
    pub workspace: WorkspaceHint,
    pub last_observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadata {
    pub session: SessionRef,
    pub first_observed_at: Timestamp,
    pub last_observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadataDelta {
    pub session: SessionRef,
    pub observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataSnapshot {
    pub workspaces: BoundedVec<WorkspaceMetadata, MAX_METADATA_WORKSPACES>,
    pub sessions: BoundedVec<SessionMetadata, MAX_METADATA_SESSIONS>,
    pub truncated: bool,
    pub corrupt_records_ignored: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataWriteOutcome {
    Updated,
    SkippedFresh,
    Contended,
    CapacityReached,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub max_sessions: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaintenanceBudget {
    pub max_records: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PruneSummary {
    pub inspected: usize,
    pub removed: usize,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataError {
    Unavailable,
    Corrupt,
    PermissionDenied,
    BoundsExceeded,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "session metadata operation failed: {self:?}")
    }
}

impl Error for MetadataError {}

/// Bounded, metadata-only persistence boundary. No filesystem implementation
/// is registered in C0.
pub trait SessionMetadataStore: Send + Sync {
    fn load_workspace(
        &self,
        selector: &WorkspaceSelector,
        limits: MetadataLoadLimits,
    ) -> Result<MetadataSnapshot, MetadataError>;

    fn merge(&self, delta: &SessionMetadataDelta, deadline: Instant) -> MetadataWriteOutcome;

    fn prune(
        &self,
        policy: &RetentionPolicy,
        budget: MaintenanceBudget,
    ) -> Result<PruneSummary, MetadataError>;
}
