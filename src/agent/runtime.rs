use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    AdapterDelivery, AdapterInput, AdapterRegistry, BoundedSet, BoundedVec, ContractUpdate,
    DEFAULT_ENDED_RETENTION, DEFAULT_MAINTENANCE_RECORD_BUDGET, DEFAULT_NON_TERMINAL_RETENTION,
    DecodeOutcome, EventEnvelope, EventId, EvidenceDomain, IdentityKeyer, InstanceContract,
    InstanceRegistry, LiveObservationReceiver, MAX_LIVE_WORKSPACE_HINTS, MaintenanceBudget,
    MetadataLoadLimits, MetadataSnapshot, MetadataWriteOutcome, ObservationEnvelope,
    ObservationProvider, ObservedEntityKind, ObserverId, ObserverInstanceId, ProviderCursor,
    ProviderDiscoveryLimits, ProviderEventOutcome, ProviderInstance, ReceiveOutcome,
    RetentionPolicy, SessionKey, SessionMetadataDelta, SessionMetadataStore, SnapshotCompleteness,
    SnapshotEnvelope, SnapshotId, SnapshotLimits, SnapshotScope, StreamEpoch, StreamOp, StreamRef,
    StreamSequence, Timestamp, TransportRejectReason, ValidatedEnvelope, WorkspaceHint,
    WorkspaceScope, WorkspaceSelector, stable_hash,
};

pub const DEFAULT_AGENT_REQUEST_CAPACITY: usize = 256;
pub const DEFAULT_AGENT_COMPLETION_CAPACITY: usize = 256;
pub const MAX_DROP_NOTICE_SESSIONS: usize = 64;
const PROVIDER_DISCOVERY_LIMIT: usize = 32;
const PROVIDER_SNAPSHOT_ITEM_LIMIT: usize = 256;
const PROVIDER_SNAPSHOT_BYTE_LIMIT: usize = 256 * 1024;
const PROVIDER_POLL_BUDGET: Duration = Duration::from_millis(10);
const PROVIDER_RECONCILE_BUDGET: Duration = Duration::from_millis(100);
const PROVIDER_REPROBE_INTERVAL: Duration = Duration::from_secs(30);
const METADATA_LOCK_BUDGET: Duration = Duration::from_millis(2);
const METADATA_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const METADATA_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceExpiry {
    pub key: EvidenceExpiryKey,
    pub valid_until: Timestamp,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EvidenceExpiryKey {
    pub generation: u64,
    pub session: SessionKey,
    pub observer: ObserverId,
    pub instance: ObserverInstanceId,
    pub domain: EvidenceDomain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceExpiryUpdate {
    pub key: EvidenceExpiryKey,
    pub valid_until: Timestamp,
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
    RefreshMetadata {
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
        workspace_hints: BoundedVec<WorkspaceHint, MAX_LIVE_WORKSPACE_HINTS>,
        envelope: Box<ValidatedEnvelope>,
    },
    EvidenceExpired {
        generation: u64,
        keys: BoundedVec<EvidenceExpiryKey, 64>,
    },
    LiveDropped {
        generation: u64,
        sessions: BoundedVec<SessionDropCount, MAX_DROP_NOTICE_SESSIONS>,
        unattributed: u32,
    },
    ProviderStatus {
        generation: u64,
        status: ProviderRuntimeStatus,
    },
    ContractUpdated {
        generation: u64,
        contract: Box<InstanceContract>,
    },
    MetadataWriteStatus {
        generation: u64,
        outcome: MetadataWriteOutcome,
    },
    IngressRejected {
        generation: u64,
        reason: TransportRejectReason,
    },
    RuntimeStatus {
        generation: u64,
        status: AgentRuntimeStatus,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDropCount {
    pub session: SessionKey,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeBackpressure {
    Full,
    Closed,
}

/// Clock seam used only by the bounded expiry scheduler.
pub trait AgentClock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[derive(Default)]
pub struct SystemAgentClock;

impl AgentClock for SystemAgentClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        Timestamp::from_unix_millis(millis)
    }
}

/// UI-side half of the bounded Agent runtime seam.
pub struct AgentRuntimeHandle {
    requests: SyncSender<AgentRuntimeRequest>,
    completions: Receiver<AgentRuntimeCompletion>,
    shutdown: Arc<AtomicBool>,
    dropped_live: Arc<Mutex<DropNotice>>,
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
        self.completions
            .try_recv()
            .ok()
            .or_else(|| take_drop_notice(&self.dropped_live))
    }

    pub fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

/// Worker-side channel endpoint, exposed for deterministic headless tests.
pub struct AgentRuntimeEndpoint {
    requests: Receiver<AgentRuntimeRequest>,
    completions: SyncSender<AgentRuntimeCompletion>,
    shutdown: Arc<AtomicBool>,
    dropped_live: Arc<Mutex<DropNotice>>,
}

impl AgentRuntimeEndpoint {
    pub fn try_request(&self) -> Option<AgentRuntimeRequest> {
        self.requests.try_recv().ok()
    }

    pub fn complete(&self, completion: AgentRuntimeCompletion) -> Result<(), RuntimeBackpressure> {
        try_send(&self.completions, completion)
    }

    fn complete_envelope(
        &self,
        generation: u64,
        workspace_hints: BoundedVec<WorkspaceHint, MAX_LIVE_WORKSPACE_HINTS>,
        envelope: ValidatedEnvelope,
    ) -> bool {
        let sessions = envelope
            .envelope()
            .observations()
            .iter()
            .filter_map(|observation| {
                observation
                    .session
                    .as_ref()
                    .map(|value| value.key().clone())
            })
            .collect::<BTreeSet<_>>();
        match self
            .completions
            .try_send(AgentRuntimeCompletion::EnvelopeReceived {
                generation,
                workspace_hints,
                envelope: Box::new(envelope),
            }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                record_drop_notice(&self.dropped_live, generation, &sessions);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

pub fn agent_runtime_channel(
    request_capacity: usize,
    completion_capacity: usize,
) -> (AgentRuntimeHandle, AgentRuntimeEndpoint) {
    let (request_sender, request_receiver) = sync_channel(request_capacity);
    let (completion_sender, completion_receiver) = sync_channel(completion_capacity);
    let shutdown = Arc::new(AtomicBool::new(false));
    let dropped_live = Arc::new(Mutex::new(DropNotice::default()));
    (
        AgentRuntimeHandle {
            requests: request_sender,
            completions: completion_receiver,
            shutdown: Arc::clone(&shutdown),
            dropped_live: Arc::clone(&dropped_live),
        },
        AgentRuntimeEndpoint {
            requests: request_receiver,
            completions: completion_sender,
            shutdown,
            dropped_live,
        },
    )
}

#[derive(Default)]
struct DropNotice {
    generation: u64,
    sessions: BTreeMap<SessionKey, u32>,
    unattributed: u32,
}

fn record_drop_notice(
    notice: &Mutex<DropNotice>,
    generation: u64,
    sessions: &BTreeSet<SessionKey>,
) {
    let Ok(mut notice) = notice.lock() else {
        return;
    };
    if notice.generation != generation {
        *notice = DropNotice {
            generation,
            ..DropNotice::default()
        };
    }
    if sessions.is_empty() {
        notice.unattributed = notice.unattributed.saturating_add(1);
        return;
    }
    for session in sessions {
        if let Some(count) = notice.sessions.get_mut(session) {
            *count = count.saturating_add(1);
        } else if notice.sessions.len() < MAX_DROP_NOTICE_SESSIONS {
            notice.sessions.insert(session.clone(), 1);
        } else {
            notice.unattributed = notice.unattributed.saturating_add(1);
        }
    }
}

fn take_drop_notice(notice: &Mutex<DropNotice>) -> Option<AgentRuntimeCompletion> {
    let mut notice = notice.lock().ok()?;
    if notice.sessions.is_empty() && notice.unattributed == 0 {
        return None;
    }
    let sessions = BoundedVec::try_from_vec(
        std::mem::take(&mut notice.sessions)
            .into_iter()
            .map(|(session, count)| SessionDropCount { session, count })
            .collect(),
    )
    .expect("drop notice map is bounded");
    let unattributed = std::mem::take(&mut notice.unattributed);
    Some(AgentRuntimeCompletion::LiveDropped {
        generation: notice.generation,
        sessions,
        unattributed,
    })
}

/// Injected service set for the concrete background runtime. Production may
/// leave adapters/providers/receiver empty; tests can supply contract fakes.
pub struct AgentRuntimeServices {
    pub adapters: Arc<AdapterRegistry>,
    pub instances: Arc<RwLock<InstanceRegistry>>,
    pub identity: Arc<dyn IdentityKeyer>,
    pub metadata: Arc<dyn SessionMetadataStore>,
    pub providers: Vec<Box<dyn ObservationProvider>>,
    pub receiver: Option<Box<dyn LiveObservationReceiver>>,
    pub clock: Arc<dyn AgentClock>,
}

impl AgentRuntimeServices {
    pub fn new(
        adapters: Arc<AdapterRegistry>,
        identity: Arc<dyn IdentityKeyer>,
        metadata: Arc<dyn SessionMetadataStore>,
    ) -> Self {
        Self {
            adapters,
            instances: Arc::new(RwLock::new(InstanceRegistry::new())),
            identity,
            metadata,
            providers: Vec::new(),
            receiver: None,
            clock: Arc::new(SystemAgentClock),
        }
    }
}

/// Owned background runtime. Dropping it requests bounded draining and joins
/// the worker so no Agent thread survives terminal shutdown.
pub struct AgentRuntime {
    handle: AgentRuntimeHandle,
    worker: Option<JoinHandle<()>>,
}

impl AgentRuntime {
    /// Wrap a bounded runtime channel without spawning a worker.
    ///
    /// This is useful for deterministic embedders that already own the
    /// execution context. The supplied endpoint remains responsible for
    /// draining requests and publishing completions.
    pub fn from_channel(handle: AgentRuntimeHandle) -> Self {
        Self {
            handle,
            worker: None,
        }
    }

    pub fn start(services: AgentRuntimeServices) -> Self {
        Self::start_with_capacities(
            services,
            DEFAULT_AGENT_REQUEST_CAPACITY,
            DEFAULT_AGENT_COMPLETION_CAPACITY,
        )
    }

    pub fn start_with_capacities(
        services: AgentRuntimeServices,
        request_capacity: usize,
        completion_capacity: usize,
    ) -> Self {
        let (handle, endpoint) = agent_runtime_channel(request_capacity, completion_capacity);
        let worker = thread::Builder::new()
            .name("latte-lens-agent-runtime".to_owned())
            .spawn(move || run_worker(endpoint, services))
            .expect("failed to start bounded Agent runtime");
        Self {
            handle,
            worker: Some(worker),
        }
    }

    pub fn submit(&self, request: AgentRuntimeRequest) -> Result<(), RuntimeBackpressure> {
        self.handle.submit(request)
    }

    pub fn try_next(&self) -> Option<AgentRuntimeCompletion> {
        self.handle.try_next()
    }

    pub fn begin_shutdown(&self) {
        self.handle.begin_shutdown();
    }
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        self.handle.begin_shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct ActiveProviderInstance {
    provider_index: usize,
    instance: ProviderInstance,
    stream: StreamRef,
    cursor: Option<ProviderCursor>,
}

fn run_worker(endpoint: AgentRuntimeEndpoint, mut services: AgentRuntimeServices) {
    let mut generation = 0_u64;
    let mut selector = WorkspaceSelector::default();
    let mut expiries = Vec::<EvidenceExpiry>::new();
    let mut active = Vec::<ActiveProviderInstance>::new();
    let mut provider_reconcile_pending = false;
    let mut next_provider_reconcile = Instant::now();
    let mut next_provider_reprobe = Instant::now() + PROVIDER_REPROBE_INTERVAL;
    let mut next_active_provider = 0_usize;
    let mut next_metadata_refresh = Instant::now() + METADATA_REFRESH_INTERVAL;
    let mut next_metadata_maintenance = Instant::now();
    let _ = endpoint.complete(AgentRuntimeCompletion::RuntimeStatus {
        generation,
        status: AgentRuntimeStatus::Starting,
    });
    let _ = endpoint.complete(AgentRuntimeCompletion::RuntimeStatus {
        generation,
        status: AgentRuntimeStatus::Running,
    });

    loop {
        if endpoint.shutdown_requested() {
            if let Some(receiver) = services.receiver.as_mut() {
                receiver.begin_draining();
            }
            for provider in &mut services.providers {
                provider.begin_draining();
            }
            active.clear();
            let _ = endpoint.complete(AgentRuntimeCompletion::RuntimeStatus {
                generation,
                status: AgentRuntimeStatus::Draining,
            });
            while let Ok(request) = endpoint.requests.try_recv() {
                if let AgentRuntimeRequest::PersistMetadata {
                    generation: request_generation,
                    delta,
                } = request
                    && request_generation == generation
                {
                    let _ = services
                        .metadata
                        .merge(&delta, Instant::now() + METADATA_LOCK_BUDGET);
                }
            }
            let _ = endpoint.complete(AgentRuntimeCompletion::RuntimeStatus {
                generation,
                status: AgentRuntimeStatus::Stopped,
            });
            break;
        }

        match endpoint.requests.recv_timeout(Duration::from_millis(2)) {
            Ok(request) => {
                provider_reconcile_pending |= handle_request(
                    request,
                    &endpoint,
                    &mut services,
                    &mut generation,
                    &mut selector,
                    &mut expiries,
                    &mut active,
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        poll_receiver(&endpoint, &mut services, generation);
        provider_reconcile_pending |= poll_providers(
            &endpoint,
            &mut services,
            generation,
            &mut active,
            &mut next_active_provider,
        );
        if provider_reconcile_pending
            && active.is_empty()
            && Instant::now() >= next_provider_reconcile
        {
            provider_reconcile_pending =
                !refresh_providers(&endpoint, &mut services, generation, &selector, &mut active);
            next_active_provider = 0;
            next_provider_reconcile = Instant::now() + Duration::from_millis(50);
        }
        if !services.providers.is_empty()
            && !selector.workspaces().is_empty()
            && Instant::now() >= next_provider_reprobe
        {
            provider_reconcile_pending =
                !refresh_providers(&endpoint, &mut services, generation, &selector, &mut active);
            next_active_provider = 0;
            next_provider_reprobe = Instant::now() + PROVIDER_REPROBE_INTERVAL;
        }
        if !selector.workspaces().is_empty() && Instant::now() >= next_metadata_refresh {
            load_metadata(&endpoint, &services, generation, &selector);
            next_metadata_refresh = Instant::now() + METADATA_REFRESH_INTERVAL;
        }
        if Instant::now() >= next_metadata_maintenance {
            run_metadata_maintenance(&services);
            next_metadata_maintenance = Instant::now() + METADATA_MAINTENANCE_INTERVAL;
        }
        emit_due_expiries(&endpoint, generation, services.clock.now(), &mut expiries);
    }
}

fn run_metadata_maintenance(services: &AgentRuntimeServices) {
    let _ = services.metadata.prune(
        &RetentionPolicy {
            now: services.clock.now(),
            ended_retention_ms: DEFAULT_ENDED_RETENTION.as_millis() as u64,
            non_terminal_retention_ms: DEFAULT_NON_TERMINAL_RETENTION.as_millis() as u64,
            max_sessions: super::MAX_METADATA_SESSIONS,
        },
        MaintenanceBudget {
            max_records: DEFAULT_MAINTENANCE_RECORD_BUDGET,
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    request: AgentRuntimeRequest,
    endpoint: &AgentRuntimeEndpoint,
    services: &mut AgentRuntimeServices,
    generation: &mut u64,
    selector: &mut WorkspaceSelector,
    expiries: &mut Vec<EvidenceExpiry>,
    active: &mut Vec<ActiveProviderInstance>,
) -> bool {
    match request {
        AgentRuntimeRequest::SelectWorkspace {
            generation: selected_generation,
            selector: selected,
        } => {
            *generation = selected_generation;
            *selector = selected;
            expiries.clear();
            active.clear();
            if let Ok(snapshot) = services
                .metadata
                .load_workspace(selector, MetadataLoadLimits::default())
            {
                let _ = endpoint.complete(AgentRuntimeCompletion::MetadataLoaded {
                    generation: *generation,
                    snapshot,
                });
            }
            false
        }
        AgentRuntimeRequest::RefreshProviders {
            generation: request_generation,
        } if request_generation == *generation => {
            !refresh_providers(endpoint, services, *generation, selector, active)
        }
        AgentRuntimeRequest::RefreshMetadata {
            generation: request_generation,
        } if request_generation == *generation => {
            load_metadata(endpoint, services, *generation, selector);
            false
        }
        AgentRuntimeRequest::PersistMetadata {
            generation: request_generation,
            delta,
        } if request_generation == *generation => {
            let outcome = services
                .metadata
                .merge(&delta, Instant::now() + METADATA_LOCK_BUDGET);
            let _ = endpoint.complete(AgentRuntimeCompletion::MetadataWriteStatus {
                generation: *generation,
                outcome,
            });
            false
        }
        AgentRuntimeRequest::ScheduleExpiry {
            generation: request_generation,
            expiry,
        } if request_generation == *generation && expiry.key.generation == *generation => {
            if let Some(existing) = expiries
                .iter_mut()
                .find(|current| current.key == expiry.key)
            {
                if expiry.valid_until > existing.valid_until {
                    *existing = expiry;
                }
            } else {
                expiries.push(expiry);
            }
            false
        }
        AgentRuntimeRequest::RefreshProviders { .. }
        | AgentRuntimeRequest::RefreshMetadata { .. }
        | AgentRuntimeRequest::PersistMetadata { .. }
        | AgentRuntimeRequest::ScheduleExpiry { .. } => false,
    }
}

fn load_metadata(
    endpoint: &AgentRuntimeEndpoint,
    services: &AgentRuntimeServices,
    generation: u64,
    selector: &WorkspaceSelector,
) {
    if let Ok(snapshot) = services
        .metadata
        .load_workspace(selector, MetadataLoadLimits::default())
    {
        let _ = endpoint.complete(AgentRuntimeCompletion::MetadataLoaded {
            generation,
            snapshot,
        });
    }
}

fn poll_receiver(
    endpoint: &AgentRuntimeEndpoint,
    services: &mut AgentRuntimeServices,
    generation: u64,
) {
    let Some(receiver) = services.receiver.as_mut() else {
        return;
    };
    match receiver.receive(Instant::now() + Duration::from_millis(5)) {
        ReceiveOutcome::Event {
            receiver_generation,
            workspace_hints,
            event,
        } if receiver_generation == generation => {
            let validated = services.instances.read().ok().and_then(|instances| {
                services
                    .adapters
                    .validate_registered_envelope(ObservationEnvelope::Event(*event), &instances)
                    .ok()
            });
            if let Some(validated) = validated {
                endpoint.complete_envelope(generation, workspace_hints, validated);
            }
        }
        ReceiveOutcome::Rejected(reason) => {
            let _ =
                endpoint.complete(AgentRuntimeCompletion::IngressRejected { generation, reason });
        }
        ReceiveOutcome::Event { .. } | ReceiveOutcome::Idle | ReceiveOutcome::Closed => {}
    }
}

fn refresh_providers(
    endpoint: &AgentRuntimeEndpoint,
    services: &mut AgentRuntimeServices,
    generation: u64,
    selector: &WorkspaceSelector,
    active: &mut Vec<ActiveProviderInstance>,
) -> bool {
    active.clear();
    let reconcile_deadline = Instant::now() + PROVIDER_RECONCILE_BUDGET;
    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
        generation,
        status: ProviderRuntimeStatus::Discovering,
    });
    let mut any_current = false;
    for provider_index in 0..services.providers.len() {
        if Instant::now() >= reconcile_deadline {
            break;
        }
        let deadline = (Instant::now() + PROVIDER_POLL_BUDGET).min(reconcile_deadline);
        let instances = match services.providers[provider_index].discover(
            selector,
            ProviderDiscoveryLimits {
                max_instances: PROVIDER_DISCOVERY_LIMIT,
            },
            deadline,
        ) {
            Ok(instances) if Instant::now() <= deadline => instances,
            Ok(_) => {
                let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                    generation,
                    status: ProviderRuntimeStatus::Degraded,
                });
                continue;
            }
            Err(_) => {
                let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                    generation,
                    status: ProviderRuntimeStatus::Unavailable,
                });
                continue;
            }
        };
        for instance in instances {
            if Instant::now() >= reconcile_deadline {
                break;
            }
            let deadline = (Instant::now() + PROVIDER_POLL_BUDGET).min(reconcile_deadline);
            let contract = match services.providers[provider_index].probe(&instance, deadline) {
                Ok(contract) if Instant::now() <= deadline => contract,
                Ok(_) => {
                    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                        generation,
                        status: ProviderRuntimeStatus::Degraded,
                    });
                    continue;
                }
                Err(_) => {
                    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                        generation,
                        status: ProviderRuntimeStatus::Degraded,
                    });
                    continue;
                }
            };
            let epoch = provider_epoch(&contract);
            let update = services
                .instances
                .write()
                .map_err(|_| ())
                .and_then(|mut instances| {
                    instances
                        .upsert(contract.clone(), epoch.clone())
                        .map_err(|_| ())
                });
            match update {
                Ok(ContractUpdate::Updated) => {
                    let _ = endpoint.complete(AgentRuntimeCompletion::ContractUpdated {
                        generation,
                        contract: Box::new(contract.clone()),
                    });
                }
                Ok(ContractUpdate::Inserted | ContractUpdate::Unchanged) => {}
                Err(_) => {
                    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                        generation,
                        status: ProviderRuntimeStatus::Degraded,
                    });
                    continue;
                }
            }
            let stream = StreamRef {
                observer: contract.observer.clone(),
                instance: contract.instance.clone(),
                epoch,
            };
            let snapshot_deadline = (Instant::now() + PROVIDER_POLL_BUDGET).min(reconcile_deadline);
            let raw = match services.providers[provider_index].snapshot(
                &instance,
                None,
                SnapshotLimits {
                    max_items: PROVIDER_SNAPSHOT_ITEM_LIMIT,
                    max_total_bytes: PROVIDER_SNAPSHOT_BYTE_LIMIT,
                },
                snapshot_deadline,
            ) {
                Ok(snapshot) if Instant::now() <= snapshot_deadline => snapshot,
                Ok(_) => {
                    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                        generation,
                        status: ProviderRuntimeStatus::Reconciling,
                    });
                    continue;
                }
                Err(_) => {
                    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                        generation,
                        status: ProviderRuntimeStatus::Reconciling,
                    });
                    continue;
                }
            };
            if emit_provider_snapshot(
                endpoint, services, generation, &instance, &contract, &stream, &raw,
            ) {
                any_current = true;
                active.push(ActiveProviderInstance {
                    provider_index,
                    instance,
                    stream,
                    cursor: raw.cursor().cloned(),
                });
            }
        }
    }
    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
        generation,
        status: if any_current {
            ProviderRuntimeStatus::Current
        } else {
            ProviderRuntimeStatus::Unavailable
        },
    });
    any_current
}

fn emit_provider_snapshot(
    endpoint: &AgentRuntimeEndpoint,
    services: &AgentRuntimeServices,
    generation: u64,
    instance: &ProviderInstance,
    contract: &InstanceContract,
    stream: &StreamRef,
    raw: &super::RawSnapshot,
) -> bool {
    let Some(adapter) = services.adapters.resolve(&stream.observer) else {
        return false;
    };
    let mut observations = Vec::new();
    for item in raw.items() {
        let input = AdapterInput {
            delivery: AdapterDelivery::ProviderSnapshotItem,
            event_name: item.event_name.as_str(),
            observer_version: instance.version.as_ref().map(super::BoundedText::as_str),
            observed_at: item.observed_at,
            workspace: None,
            payload: item.payload.as_slice(),
        };
        match adapter.decode(input, services.identity.as_ref()) {
            Ok(DecodeOutcome::Observations(decoded)) => observations.extend(decoded),
            Ok(DecodeOutcome::Ignore(_)) => {}
            Err(_) => return false,
        }
        if observations.len() > PROVIDER_SNAPSHOT_ITEM_LIMIT {
            return false;
        }
    }
    let scope = raw
        .scope()
        .cloned()
        .unwrap_or_else(|| contract_snapshot_scope(contract));
    let snapshot_id = snapshot_id(stream, raw.cursor(), raw.watermark());
    let supports_chunking = contract.snapshot_semantics.chunked;
    let completeness = if !raw.is_complete() || (!supports_chunking && observations.len() > 64) {
        SnapshotCompleteness::Truncated
    } else {
        SnapshotCompleteness::Complete
    };
    let emitted = if supports_chunking {
        observations.as_slice()
    } else {
        &observations[..observations.len().min(64)]
    };
    let chunks = if emitted.is_empty() {
        vec![emitted]
    } else {
        emitted.chunks(64).collect::<Vec<_>>()
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let snapshot = SnapshotEnvelope {
            stream: stream.clone(),
            snapshot_id: snapshot_id.clone(),
            chunk_index: index as u16,
            final_chunk: index + 1 == chunks.len(),
            captured_at: raw.captured_at(),
            scope: scope.clone(),
            completeness,
            watermark: raw.watermark(),
            observations: BoundedVec::try_from_vec(chunk.to_vec()).expect("chunk capped"),
        };
        let Ok(validated) = services
            .adapters
            .validate_envelope(ObservationEnvelope::Snapshot(snapshot), contract)
        else {
            return false;
        };
        if !endpoint.complete_envelope(generation, BoundedVec::new(), validated) {
            return false;
        }
    }
    true
}

fn poll_providers(
    endpoint: &AgentRuntimeEndpoint,
    services: &mut AgentRuntimeServices,
    generation: u64,
    active: &mut Vec<ActiveProviderInstance>,
    next_active_provider: &mut usize,
) -> bool {
    if active.is_empty() {
        *next_active_provider = 0;
        return false;
    }
    let active_index = *next_active_provider % active.len();
    let provider_instance = &mut active[active_index];
    let deadline = Instant::now() + PROVIDER_POLL_BUDGET;
    let outcome = services.providers[provider_instance.provider_index]
        .next_event(&provider_instance.instance, deadline);
    let mut remove = false;
    let mut reconcile = false;
    match outcome {
        ProviderEventOutcome::Event(raw) => {
            provider_instance.cursor = raw.cursor.clone();
            if Instant::now() > deadline
                || !emit_provider_event(endpoint, services, generation, provider_instance, raw)
            {
                emit_gap(
                    endpoint,
                    services,
                    generation,
                    &provider_instance.stream,
                    None,
                    None,
                );
                reconcile = true;
                remove = true;
            }
        }
        ProviderEventOutcome::Reset => {
            emit_control_event(
                endpoint,
                services,
                generation,
                &provider_instance.stream,
                StreamOp::Reset,
            );
            reconcile = true;
            remove = true;
        }
        ProviderEventOutcome::Gap { expected, received } => {
            emit_gap(
                endpoint,
                services,
                generation,
                &provider_instance.stream,
                expected,
                received,
            );
            reconcile = true;
            remove = true;
        }
        ProviderEventOutcome::Closed | ProviderEventOutcome::Failed(_) => {
            let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
                generation,
                status: ProviderRuntimeStatus::Reconciling,
            });
            reconcile = true;
            remove = true;
        }
        ProviderEventOutcome::Idle => {}
    }
    if remove {
        active.remove(active_index);
        if active.is_empty() || *next_active_provider >= active.len() {
            *next_active_provider = 0;
        }
    } else {
        *next_active_provider = (active_index + 1) % active.len();
    }
    reconcile
}

fn emit_provider_event(
    endpoint: &AgentRuntimeEndpoint,
    services: &AgentRuntimeServices,
    generation: u64,
    active: &ActiveProviderInstance,
    raw: super::RawEvent,
) -> bool {
    let Some(adapter) = services.adapters.resolve(&active.stream.observer) else {
        return false;
    };
    let input = AdapterInput {
        delivery: AdapterDelivery::ProviderEvent,
        event_name: raw.item.event_name.as_str(),
        observer_version: active
            .instance
            .version
            .as_ref()
            .map(super::BoundedText::as_str),
        observed_at: raw.item.observed_at,
        workspace: None,
        payload: raw.item.payload.as_slice(),
    };
    let observations = match adapter.decode(input, services.identity.as_ref()) {
        Ok(DecodeOutcome::Observations(observations)) => observations,
        Ok(DecodeOutcome::Ignore(_)) => return true,
        Err(_) => return false,
    };
    let composite = provider_event_composite(raw.cursor.as_ref(), raw.sequence, &raw.item);
    let Ok(event_id) = services.identity.event_id(
        &active.stream.observer,
        &active.stream.instance,
        &active.stream.epoch,
        super::SensitiveId::new(&composite),
    ) else {
        return false;
    };
    let envelope = EventEnvelope {
        stream: active.stream.clone(),
        event_id,
        sequence: raw.sequence,
        op: StreamOp::Upsert(observations),
    };
    let Ok(instances) = services.instances.read() else {
        return false;
    };
    let Ok(validated) = services
        .adapters
        .validate_registered_envelope(ObservationEnvelope::Event(envelope), &instances)
    else {
        return false;
    };
    endpoint.complete_envelope(generation, BoundedVec::new(), validated)
}

fn emit_gap(
    endpoint: &AgentRuntimeEndpoint,
    services: &AgentRuntimeServices,
    generation: u64,
    stream: &StreamRef,
    expected: Option<StreamSequence>,
    received: Option<StreamSequence>,
) {
    emit_control_event(
        endpoint,
        services,
        generation,
        stream,
        StreamOp::Gap { expected, received },
    );
    let _ = endpoint.complete(AgentRuntimeCompletion::ProviderStatus {
        generation,
        status: ProviderRuntimeStatus::GapDetected,
    });
}

fn emit_control_event(
    endpoint: &AgentRuntimeEndpoint,
    services: &AgentRuntimeServices,
    generation: u64,
    stream: &StreamRef,
    op: StreamOp,
) {
    let marker = match &op {
        StreamOp::Reset => b"reset".as_slice(),
        StreamOp::Gap { .. } => b"gap".as_slice(),
        StreamOp::Upsert(_) | StreamOp::Delete { .. } => b"control".as_slice(),
    };
    let event_id = EventId::from_digest(stable_hash(
        b"provider-control-event",
        &[stream.epoch.digest().as_bytes(), marker],
    ));
    let event = EventEnvelope {
        stream: stream.clone(),
        event_id,
        sequence: None,
        op,
    };
    if let Ok(instances) = services.instances.read()
        && let Ok(validated) = services
            .adapters
            .validate_registered_envelope(ObservationEnvelope::Event(event), &instances)
    {
        endpoint.complete_envelope(generation, BoundedVec::new(), validated);
    }
}

fn emit_due_expiries(
    endpoint: &AgentRuntimeEndpoint,
    generation: u64,
    now: Timestamp,
    expiries: &mut Vec<EvidenceExpiry>,
) {
    let mut due = Vec::new();
    expiries.retain(|expiry| {
        if expiry.key.generation != generation {
            return false;
        }
        if expiry.valid_until <= now && due.len() < 64 {
            due.push(expiry.key.clone());
            false
        } else {
            true
        }
    });
    if !due.is_empty() {
        let _ = endpoint.complete(AgentRuntimeCompletion::EvidenceExpired {
            generation,
            keys: BoundedVec::try_from_vec(due).expect("expiry batch capped"),
        });
    }
}

fn provider_epoch(contract: &InstanceContract) -> StreamEpoch {
    StreamEpoch::from_digest(stable_hash(
        b"provider-instance-epoch",
        &[
            contract.instance.digest().as_bytes(),
            &contract.revision.get().to_be_bytes(),
        ],
    ))
}

fn snapshot_id(
    stream: &StreamRef,
    cursor: Option<&ProviderCursor>,
    watermark: Option<StreamSequence>,
) -> SnapshotId {
    let watermark = watermark
        .map(StreamSequence::get)
        .unwrap_or_default()
        .to_be_bytes();
    SnapshotId::from_digest(stable_hash(
        b"provider-snapshot",
        &[
            stream.instance.digest().as_bytes(),
            stream.epoch.digest().as_bytes(),
            cursor.map(ProviderCursor::as_bytes).unwrap_or_default(),
            &watermark,
        ],
    ))
}

fn provider_event_composite(
    cursor: Option<&ProviderCursor>,
    sequence: Option<StreamSequence>,
    item: &super::RawProviderItem,
) -> Vec<u8> {
    let mut output = Vec::new();
    if let Some(cursor) = cursor {
        output.extend_from_slice(cursor.as_bytes());
    }
    if let Some(sequence) = sequence {
        output.extend_from_slice(&sequence.get().to_be_bytes());
    }
    output.extend_from_slice(item.event_name.as_str().as_bytes());
    output
}

fn contract_snapshot_scope(contract: &InstanceContract) -> SnapshotScope {
    let subjects = contract.subjects.iter().cloned().collect::<BTreeSet<_>>();
    let mut entity_kinds = BTreeSet::new();
    let mut domains = BTreeSet::new();
    for (domain, claim) in &contract.capabilities {
        if !matches!(
            claim.support,
            super::CapabilitySupport::Confirmed | super::CapabilitySupport::Partial
        ) {
            continue;
        }
        domains.insert(*domain);
        entity_kinds.insert(match domain {
            EvidenceDomain::Presence => ObservedEntityKind::Presence,
            EvidenceDomain::AgentTopology => ObservedEntityKind::Agent,
            EvidenceDomain::Turn | EvidenceDomain::Permission | EvidenceDomain::Tool => {
                ObservedEntityKind::Turn
            }
            EvidenceDomain::Artifact => ObservedEntityKind::Artifact,
            EvidenceDomain::Session
            | EvidenceDomain::Lifecycle
            | EvidenceDomain::Activity
            | EvidenceDomain::Change
            | EvidenceDomain::Presentation
            | EvidenceDomain::Diagnostic => ObservedEntityKind::Session,
        });
    }
    SnapshotScope {
        workspaces: WorkspaceScope::Selected,
        subjects: BoundedSet::try_from_iter(subjects).expect("contract subjects are bounded"),
        entity_kinds: BoundedSet::try_from_iter(entity_kinds).expect("entity kinds are bounded"),
        domains: BoundedSet::try_from_iter(domains).expect("evidence domains are bounded"),
    }
}

fn try_send<T>(sender: &SyncSender<T>, value: T) -> Result<(), RuntimeBackpressure> {
    sender.try_send(value).map_err(|error| match error {
        TrySendError::Full(_) => RuntimeBackpressure::Full,
        TrySendError::Disconnected(_) => RuntimeBackpressure::Closed,
    })
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

    #[test]
    fn expiry_batches_are_generation_scoped_without_wall_clock_sleep() {
        let (handle, endpoint) = agent_runtime_channel(1, 2);
        let session = SessionKey::new(
            super::super::SubjectNamespace::parse("test/agent").expect("subject"),
            super::super::InstallId::from_digest(super::super::StableDigest::from_bytes([1; 32])),
            super::super::AuthorityId::from_digest(super::super::StableDigest::from_bytes([2; 32])),
            super::super::StableDigest::from_bytes([3; 32]),
        );
        let due_key = EvidenceExpiryKey {
            generation: 4,
            session: session.clone(),
            observer: ObserverId::parse("test/hook").expect("observer"),
            instance: ObserverInstanceId::from_digest(super::super::StableDigest::from_bytes(
                [4; 32],
            )),
            domain: EvidenceDomain::Activity,
        };
        let mut stale_generation = due_key.clone();
        stale_generation.generation = 3;
        let mut future = due_key.clone();
        future.domain = EvidenceDomain::Lifecycle;
        let mut expiries = vec![
            EvidenceExpiry {
                key: stale_generation,
                valid_until: Timestamp::from_unix_millis(1),
            },
            EvidenceExpiry {
                key: due_key.clone(),
                valid_until: Timestamp::from_unix_millis(10),
            },
            EvidenceExpiry {
                key: future.clone(),
                valid_until: Timestamp::from_unix_millis(11),
            },
        ];
        emit_due_expiries(&endpoint, 4, Timestamp::from_unix_millis(10), &mut expiries);
        assert_eq!(
            expiries,
            vec![EvidenceExpiry {
                key: future,
                valid_until: Timestamp::from_unix_millis(11),
            }]
        );
        assert_eq!(
            handle.try_next(),
            Some(AgentRuntimeCompletion::EvidenceExpired {
                generation: 4,
                keys: BoundedVec::try_from_vec(vec![due_key]).expect("due key"),
            })
        );
    }

    #[test]
    fn dropped_live_notice_is_bounded_and_delivered_after_the_queue_drains() {
        let (handle, endpoint) = agent_runtime_channel(1, 1);
        endpoint
            .complete(AgentRuntimeCompletion::RuntimeStatus {
                generation: 4,
                status: AgentRuntimeStatus::Running,
            })
            .expect("fill completion queue");
        let session = SessionKey::new(
            super::super::SubjectNamespace::parse("test/agent").expect("subject"),
            super::super::InstallId::from_digest(super::super::StableDigest::from_bytes([1; 32])),
            super::super::AuthorityId::from_digest(super::super::StableDigest::from_bytes([2; 32])),
            super::super::StableDigest::from_bytes([3; 32]),
        );
        record_drop_notice(
            &endpoint.dropped_live,
            4,
            &BTreeSet::from([session.clone()]),
        );
        assert!(matches!(
            handle.try_next(),
            Some(AgentRuntimeCompletion::RuntimeStatus { .. })
        ));
        assert_eq!(
            handle.try_next(),
            Some(AgentRuntimeCompletion::LiveDropped {
                generation: 4,
                sessions: BoundedVec::try_from_vec(vec![SessionDropCount { session, count: 1 }])
                    .expect("drop count"),
                unattributed: 0,
            })
        );
    }

    #[test]
    fn drop_notices_reset_on_generation_change_and_cap_session_attribution() {
        let (handle, endpoint) = agent_runtime_channel(1, 1);
        let subject = super::super::SubjectNamespace::parse("test/agent").expect("subject");
        let install =
            super::super::InstallId::from_digest(super::super::StableDigest::from_bytes([1; 32]));
        let authority =
            super::super::AuthorityId::from_digest(super::super::StableDigest::from_bytes([2; 32]));
        for index in 0..=MAX_DROP_NOTICE_SESSIONS {
            let session = SessionKey::new(
                subject.clone(),
                install.clone(),
                authority.clone(),
                super::super::StableDigest::from_bytes([index as u8; 32]),
            );
            record_drop_notice(&endpoint.dropped_live, 4, &BTreeSet::from([session]));
        }
        let Some(AgentRuntimeCompletion::LiveDropped {
            generation,
            sessions,
            unattributed,
        }) = handle.try_next()
        else {
            panic!("bounded drop notice was not delivered");
        };
        assert_eq!(generation, 4);
        assert_eq!(sessions.len(), MAX_DROP_NOTICE_SESSIONS);
        assert_eq!(unattributed, 1);

        let first = SessionKey::new(
            subject,
            install,
            authority,
            super::super::StableDigest::from_bytes([99; 32]),
        );
        record_drop_notice(&endpoint.dropped_live, 4, &BTreeSet::from([first]));
        record_drop_notice(&endpoint.dropped_live, 5, &BTreeSet::new());
        assert_eq!(
            handle.try_next(),
            Some(AgentRuntimeCompletion::LiveDropped {
                generation: 5,
                sessions: BoundedVec::new(),
                unattributed: 1,
            })
        );
    }

    #[test]
    fn channel_reports_closed_and_full_on_both_halves() {
        let (handle, endpoint) = agent_runtime_channel(1, 1);
        endpoint
            .complete(AgentRuntimeCompletion::RuntimeStatus {
                generation: 1,
                status: AgentRuntimeStatus::Running,
            })
            .expect("fill completion queue");
        assert_eq!(
            endpoint.complete(AgentRuntimeCompletion::RuntimeStatus {
                generation: 1,
                status: AgentRuntimeStatus::Draining,
            }),
            Err(RuntimeBackpressure::Full)
        );
        drop(endpoint);
        assert_eq!(
            handle.submit(AgentRuntimeRequest::RefreshMetadata { generation: 1 }),
            Err(RuntimeBackpressure::Closed)
        );

        let (handle, endpoint) = agent_runtime_channel(1, 1);
        drop(handle);
        assert_eq!(
            endpoint.complete(AgentRuntimeCompletion::RuntimeStatus {
                generation: 1,
                status: AgentRuntimeStatus::Stopped,
            }),
            Err(RuntimeBackpressure::Closed)
        );
    }
}
