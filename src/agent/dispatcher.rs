use std::time::{Duration, Instant};

use super::{
    AdapterRegistry, EventEnvelope, InstanceContract, LiveObservationPublisher,
    MetadataWriteOutcome, ObservationEnvelope, PublishOutcome, SessionMetadataStore,
    project_metadata,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    LiveAccepted { receiver_generation: u64 },
    Metadata(MetadataWriteOutcome),
    IgnoredNoSession,
    RejectedInvalid,
}

/// Live-first Hook routing with a bounded metadata fallback.
///
/// This concrete orchestrator accepts only normalized [`EventEnvelope`] input.
/// Source decoding stays in [`super::CodeAgentAdapter`].
pub struct ObservationDispatcher<'a> {
    adapters: &'a AdapterRegistry,
    publisher: &'a dyn LiveObservationPublisher,
    metadata: &'a dyn SessionMetadataStore,
}

impl<'a> ObservationDispatcher<'a> {
    pub const fn new(
        adapters: &'a AdapterRegistry,
        publisher: &'a dyn LiveObservationPublisher,
        metadata: &'a dyn SessionMetadataStore,
    ) -> Self {
        Self {
            adapters,
            publisher,
            metadata,
        }
    }

    pub fn dispatch(
        &self,
        event: EventEnvelope,
        contract: &InstanceContract,
        deadline: Instant,
    ) -> DispatchOutcome {
        self.dispatch_with_budget(
            event,
            contract,
            deadline,
            deadline.saturating_duration_since(Instant::now()),
        )
    }

    /// Dispatch one event with independent live and metadata fallback budgets.
    pub fn dispatch_with_budget(
        &self,
        event: EventEnvelope,
        contract: &InstanceContract,
        live_deadline: Instant,
        metadata_budget: Duration,
    ) -> DispatchOutcome {
        let Ok(validated) = self
            .adapters
            .validate_envelope(ObservationEnvelope::Event(event), contract)
        else {
            return DispatchOutcome::RejectedInvalid;
        };
        let metadata_deltas = project_metadata(&validated);
        let ObservationEnvelope::Event(event) = validated.into_inner() else {
            unreachable!("dispatcher only validates event envelopes");
        };

        let publish = self.publisher.publish(&event, live_deadline);
        if let PublishOutcome::Accepted {
            receiver_generation,
        } = publish
        {
            return DispatchOutcome::LiveAccepted {
                receiver_generation,
            };
        }

        let [delta] = metadata_deltas.as_ref() else {
            if metadata_deltas.is_empty() {
                return DispatchOutcome::IgnoredNoSession;
            }
            // Hook frames must describe one session so fallback remains a
            // single bounded update under the emitter deadline.
            return DispatchOutcome::RejectedInvalid;
        };
        if !matches!(event.op, super::StreamOp::Upsert(_)) {
            return DispatchOutcome::IgnoredNoSession;
        }
        DispatchOutcome::Metadata(self.metadata.merge(delta, Instant::now() + metadata_budget))
    }
}
