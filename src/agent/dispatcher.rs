use std::time::Instant;

use super::{
    AdapterRegistry, EventEnvelope, InstanceContract, LiveObservationPublisher,
    MetadataWriteOutcome, ObservationEnvelope, PublishOutcome, SessionMetadataDelta,
    SessionMetadataStore, StreamOp,
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
        let Ok(validated) = self
            .adapters
            .validate_envelope(ObservationEnvelope::Event(event), contract)
        else {
            return DispatchOutcome::RejectedInvalid;
        };
        let ObservationEnvelope::Event(event) = validated.into_inner() else {
            unreachable!("dispatcher only validates event envelopes");
        };

        if let PublishOutcome::Accepted {
            receiver_generation,
        } = self.publisher.publish(&event, deadline)
        {
            return DispatchOutcome::LiveAccepted {
                receiver_generation,
            };
        }

        let StreamOp::Upsert(observations) = &event.op else {
            return DispatchOutcome::IgnoredNoSession;
        };
        let mut selected: Option<super::SessionRef> = None;
        let mut observed_at: Option<super::Timestamp> = None;
        for observation in observations {
            let Some(session) = &observation.session else {
                continue;
            };
            if selected
                .as_ref()
                .is_some_and(|selected| selected != session)
            {
                return DispatchOutcome::RejectedInvalid;
            }
            selected = Some(session.clone());
            observed_at = Some(observed_at.map_or(observation.observed_at, |current| {
                current.max(observation.observed_at)
            }));
        }
        let (Some(session), Some(observed_at)) = (selected, observed_at) else {
            return DispatchOutcome::IgnoredNoSession;
        };
        DispatchOutcome::Metadata(self.metadata.merge(
            &SessionMetadataDelta {
                session,
                observed_at,
            },
            deadline,
        ))
    }
}
