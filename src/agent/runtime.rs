use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};

use super::{
    BoundedVec, MetadataSnapshot, SessionMetadataDelta, ValidatedEnvelope, WorkspaceSelector,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceExpiry {
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceExpiryKey {
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentRuntimeRequest {
    SelectWorkspace {
        generation: u64,
        selector: WorkspaceSelector,
    },
    RefreshProviders {
        generation: u64,
    },
    PersistMetadata {
        generation: u64,
        delta: SessionMetadataDelta,
    },
    ScheduleExpiry {
        generation: u64,
        expiry: EvidenceExpiry,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRuntimeStatus {
    Discovering,
    Current,
    Reconciling,
    GapDetected,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentRuntimeStatus {
    Starting,
    Running,
    Draining,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentRuntimeCompletion {
    MetadataLoaded {
        generation: u64,
        snapshot: MetadataSnapshot,
    },
    EnvelopeReceived {
        generation: u64,
        envelope: Box<ValidatedEnvelope>,
    },
    EvidenceExpired {
        generation: u64,
        keys: BoundedVec<EvidenceExpiryKey, 64>,
    },
    ProviderStatus {
        generation: u64,
        status: ProviderRuntimeStatus,
    },
    RuntimeStatus {
        generation: u64,
        status: AgentRuntimeStatus,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeBackpressure {
    Full,
    Closed,
}

/// UI-side half of the bounded Agent runtime seam.
pub struct AgentRuntimeHandle {
    requests: SyncSender<AgentRuntimeRequest>,
    completions: Receiver<AgentRuntimeCompletion>,
    shutdown: Arc<AtomicBool>,
}

impl AgentRuntimeHandle {
    pub fn submit(&self, request: AgentRuntimeRequest) -> Result<(), RuntimeBackpressure> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => RuntimeBackpressure::Full,
                TrySendError::Disconnected(_) => RuntimeBackpressure::Closed,
            })
    }

    pub fn try_next(&self) -> Option<AgentRuntimeCompletion> {
        self.completions.try_recv().ok()
    }

    pub fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

/// Worker-side half used by the future runtime implementation.
pub struct AgentRuntimeEndpoint {
    requests: Receiver<AgentRuntimeRequest>,
    completions: SyncSender<AgentRuntimeCompletion>,
    shutdown: Arc<AtomicBool>,
}

impl AgentRuntimeEndpoint {
    pub fn try_request(&self) -> Option<AgentRuntimeRequest> {
        self.requests.try_recv().ok()
    }

    pub fn complete(&self, completion: AgentRuntimeCompletion) -> Result<(), RuntimeBackpressure> {
        self.completions
            .try_send(completion)
            .map_err(|error| match error {
                TrySendError::Full(_) => RuntimeBackpressure::Full,
                TrySendError::Disconnected(_) => RuntimeBackpressure::Closed,
            })
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

/// Construct channel endpoints without starting a thread or provider.
pub fn agent_runtime_channel(
    request_capacity: usize,
    completion_capacity: usize,
) -> (AgentRuntimeHandle, AgentRuntimeEndpoint) {
    let (request_sender, request_receiver) = sync_channel(request_capacity);
    let (completion_sender, completion_receiver) = sync_channel(completion_capacity);
    let shutdown = Arc::new(AtomicBool::new(false));
    (
        AgentRuntimeHandle {
            requests: request_sender,
            completions: completion_receiver,
            shutdown: Arc::clone(&shutdown),
        },
        AgentRuntimeEndpoint {
            requests: request_receiver,
            completions: completion_sender,
            shutdown,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_is_non_blocking_and_bounded() {
        let (handle, endpoint) = agent_runtime_channel(1, 1);
        handle
            .submit(AgentRuntimeRequest::RefreshProviders { generation: 1 })
            .expect("first request");
        assert_eq!(
            handle.submit(AgentRuntimeRequest::RefreshProviders { generation: 2 }),
            Err(RuntimeBackpressure::Full)
        );
        assert!(endpoint.try_request().is_some());

        endpoint
            .complete(AgentRuntimeCompletion::RuntimeStatus {
                generation: 1,
                status: AgentRuntimeStatus::Running,
            })
            .expect("completion");
        assert!(handle.try_next().is_some());
        handle.begin_shutdown();
        assert!(endpoint.shutdown_requested());
    }
}
