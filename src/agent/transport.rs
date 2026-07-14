use std::time::Instant;

use super::EventEnvelope;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Accepted { receiver_generation: u64 },
    Unavailable,
    NotMember,
    Busy,
    Incompatible,
    Rejected,
}

pub trait LiveObservationPublisher: Send + Sync {
    fn publish(&self, event: &EventEnvelope, deadline: Instant) -> PublishOutcome;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportRejectReason {
    InvalidPeer,
    InvalidEnvelope,
    IncompatibleProtocol,
    NotMember,
    Busy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiveOutcome {
    Event {
        receiver_generation: u64,
        event: Box<EventEnvelope>,
    },
    Idle,
    Closed,
    Rejected(TransportRejectReason),
}

pub trait LiveObservationReceiver: Send {
    fn receive(&mut self, deadline: Instant) -> ReceiveOutcome;
    fn begin_draining(&mut self);
}
