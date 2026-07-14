use std::{error::Error, fmt, time::Instant};

use super::{
    BoundedBytes, BoundedText, BoundedVec, InstanceContract, ObserverId, ObserverInstanceId,
    StreamSequence, Timestamp, WorkspaceSelector,
};

pub const MAX_PROVIDER_INSTANCES: usize = 32;
pub const MAX_RAW_SNAPSHOT_ITEMS: usize = 256;
pub const MAX_RAW_SNAPSHOT_BYTES: usize = 256 * 1024;
pub const MAX_RAW_PAYLOAD_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderDiscoveryLimits {
    pub max_instances: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotLimits {
    pub max_items: usize,
    pub max_total_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderEndpointKind {
    LocalSocket,
    LocalHttp,
    EmbeddedControlPlane,
    OtherLocal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderHealth {
    Available,
    Degraded,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInstance {
    pub observer: ObserverId,
    pub instance: ObserverInstanceId,
    pub version: Option<BoundedText<64>>,
    pub endpoint_kind: ProviderEndpointKind,
    pub health: ProviderHealth,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCursor(BoundedBytes<256>);

impl ProviderCursor {
    pub const fn new(bytes: BoundedBytes<256>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawProviderItem {
    pub event_name: BoundedText<128>,
    pub observed_at: Timestamp,
    pub payload: BoundedBytes<MAX_RAW_PAYLOAD_BYTES>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawSnapshot {
    cursor: Option<ProviderCursor>,
    watermark: Option<StreamSequence>,
    complete: bool,
    items: BoundedVec<RawProviderItem, MAX_RAW_SNAPSHOT_ITEMS>,
}

impl RawSnapshot {
    pub fn try_new(
        cursor: Option<ProviderCursor>,
        watermark: Option<StreamSequence>,
        complete: bool,
        items: BoundedVec<RawProviderItem, MAX_RAW_SNAPSHOT_ITEMS>,
    ) -> Result<Self, ProviderError> {
        let total_bytes = items.iter().try_fold(0_usize, |total, item| {
            total.checked_add(item.payload.as_slice().len())
        });
        if total_bytes.is_none_or(|total| total > MAX_RAW_SNAPSHOT_BYTES) {
            return Err(ProviderError::BoundsExceeded);
        }
        Ok(Self {
            cursor,
            watermark,
            complete,
            items,
        })
    }

    pub const fn cursor(&self) -> Option<&ProviderCursor> {
        self.cursor.as_ref()
    }

    pub const fn watermark(&self) -> Option<StreamSequence> {
        self.watermark
    }

    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn items(&self) -> &[RawProviderItem] {
        &self.items
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawEvent {
    pub cursor: Option<ProviderCursor>,
    pub sequence: Option<StreamSequence>,
    pub item: RawProviderItem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderError {
    DeadlineExceeded,
    Unavailable,
    Incompatible,
    PermissionDenied,
    BoundsExceeded,
    InvalidResponse,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "read-only provider failed: {self:?}")
    }
}

impl Error for ProviderError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderEventOutcome {
    Event(RawEvent),
    Idle,
    Reset,
    Gap {
        expected: Option<StreamSequence>,
        received: Option<StreamSequence>,
    },
    Closed,
    Failed(ProviderError),
}

/// Read-only discovery/snapshot/event acquisition boundary.
///
/// There are intentionally no start, send, focus, resume, or configuration
/// mutation methods on this trait.
pub trait ObservationProvider: Send {
    fn observer_id(&self) -> ObserverId;

    fn discover(
        &mut self,
        selector: &WorkspaceSelector,
        limits: ProviderDiscoveryLimits,
    ) -> Result<BoundedVec<ProviderInstance, MAX_PROVIDER_INSTANCES>, ProviderError>;

    fn probe(
        &mut self,
        instance: &ProviderInstance,
        deadline: Instant,
    ) -> Result<InstanceContract, ProviderError>;

    fn snapshot(
        &mut self,
        instance: &ProviderInstance,
        cursor: Option<&ProviderCursor>,
        limits: SnapshotLimits,
        deadline: Instant,
    ) -> Result<RawSnapshot, ProviderError>;

    fn next_event(
        &mut self,
        instance: &ProviderInstance,
        deadline: Instant,
    ) -> ProviderEventOutcome;
}
