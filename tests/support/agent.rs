use std::{collections::VecDeque, hash::Hasher, sync::Mutex, time::Instant};

use latte_lens::agent::*;

pub fn digest(byte: u8) -> StableDigest {
    StableDigest::from_bytes([byte; 32])
}

fn fake_digest(parts: &[&[u8]]) -> StableDigest {
    let mut bytes = [0_u8; 32];
    for index in 0..4_u8 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        hasher.write_u8(index);
        for part in parts {
            hasher.write_usize(part.len());
            hasher.write(part);
        }
        bytes[usize::from(index) * 8..usize::from(index + 1) * 8]
            .copy_from_slice(&hasher.finish().to_le_bytes());
    }
    StableDigest::from_bytes(bytes)
}

pub struct FakeIdentityKeyer {
    install: InstallId,
}

impl FakeIdentityKeyer {
    pub fn new() -> Self {
        Self {
            install: InstallId::from_digest(digest(0xf0)),
        }
    }
}

impl IdentityKeyer for FakeIdentityKeyer {
    fn event_id(
        &self,
        observer: &ObserverId,
        instance: &ObserverInstanceId,
        epoch: &StreamEpoch,
        native_or_composite_id: SensitiveId<'_>,
    ) -> Result<EventId, IdentityError> {
        Ok(EventId::from_digest(fake_digest(&[
            observer.as_str().as_bytes(),
            instance.digest().as_bytes(),
            epoch.digest().as_bytes(),
            native_or_composite_id.as_bytes(),
        ])))
    }

    fn session_key(
        &self,
        subject: &SubjectNamespace,
        authority: &AuthorityId,
        native_id: SensitiveId<'_>,
    ) -> Result<SessionKey, IdentityError> {
        Ok(SessionKey::new(
            subject.clone(),
            self.install.clone(),
            authority.clone(),
            fake_digest(&[
                subject.as_str().as_bytes(),
                authority.digest().as_bytes(),
                native_id.as_bytes(),
            ]),
        ))
    }

    fn presence_ref(
        &self,
        observer: &ObserverId,
        instance: &ObserverInstanceId,
        native_presence_id: SensitiveId<'_>,
        subject_hint: Option<&SubjectNamespace>,
        workspace: Option<WorkspaceHint>,
    ) -> Result<PresenceRef, IdentityError> {
        Ok(PresenceRef::new(
            fake_digest(&[
                observer.as_str().as_bytes(),
                instance.digest().as_bytes(),
                native_presence_id.as_bytes(),
            ]),
            subject_hint.cloned(),
            workspace,
        ))
    }

    fn agent_key(
        &self,
        session: &SessionKey,
        native_id: SensitiveId<'_>,
    ) -> Result<AgentKey, IdentityError> {
        Ok(AgentKey::new(
            session.clone(),
            fake_digest(&[session.stable_id().as_bytes(), native_id.as_bytes()]),
        ))
    }

    fn turn_key(
        &self,
        session: &SessionKey,
        authority: &AuthorityId,
        native_id: SensitiveId<'_>,
    ) -> Result<TurnKey, IdentityError> {
        Ok(TurnKey::new(
            session.clone(),
            authority.clone(),
            fake_digest(&[
                session.stable_id().as_bytes(),
                authority.digest().as_bytes(),
                native_id.as_bytes(),
            ]),
        ))
    }

    fn workspace_hint(
        &self,
        locator: SensitiveWorkspaceLocator<'_>,
    ) -> Result<WorkspaceHint, IdentityError> {
        Ok(WorkspaceHint::from_digest(fake_digest(&[
            locator.as_bytes()
        ])))
    }
}

pub struct FakeAdapter {
    descriptor: ObserverDescriptor,
    template: InstanceContractTemplate,
    outcome: DecodeOutcome,
}

impl FakeAdapter {
    pub fn new(
        descriptor: ObserverDescriptor,
        template: InstanceContractTemplate,
        outcome: DecodeOutcome,
    ) -> Self {
        Self {
            descriptor,
            template,
            outcome,
        }
    }
}

impl CodeAgentAdapter for FakeAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        self.descriptor.clone()
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        self.template.clone()
    }

    fn decode(
        &self,
        input: AdapterInput<'_>,
        _identity: &dyn IdentityKeyer,
    ) -> Result<DecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        Ok(self.outcome.clone())
    }
}

pub struct FakeProvider {
    observer: ObserverId,
    instances: BoundedVec<ProviderInstance, MAX_PROVIDER_INSTANCES>,
    contract: InstanceContract,
    snapshot: RawSnapshot,
    events: VecDeque<ProviderEventOutcome>,
}

impl FakeProvider {
    pub fn new(
        observer: ObserverId,
        instances: BoundedVec<ProviderInstance, MAX_PROVIDER_INSTANCES>,
        contract: InstanceContract,
        snapshot: RawSnapshot,
        events: impl IntoIterator<Item = ProviderEventOutcome>,
    ) -> Self {
        Self {
            observer,
            instances,
            contract,
            snapshot,
            events: events.into_iter().collect(),
        }
    }
}

impl ObservationProvider for FakeProvider {
    fn observer_id(&self) -> ObserverId {
        self.observer.clone()
    }

    fn discover(
        &mut self,
        _selector: &WorkspaceSelector,
        _limits: ProviderDiscoveryLimits,
    ) -> Result<BoundedVec<ProviderInstance, MAX_PROVIDER_INSTANCES>, ProviderError> {
        Ok(self.instances.clone())
    }

    fn probe(
        &mut self,
        _instance: &ProviderInstance,
        _deadline: Instant,
    ) -> Result<InstanceContract, ProviderError> {
        Ok(self.contract.clone())
    }

    fn snapshot(
        &mut self,
        _instance: &ProviderInstance,
        _cursor: Option<&ProviderCursor>,
        _limits: SnapshotLimits,
        _deadline: Instant,
    ) -> Result<RawSnapshot, ProviderError> {
        Ok(self.snapshot.clone())
    }

    fn next_event(
        &mut self,
        _instance: &ProviderInstance,
        _deadline: Instant,
    ) -> ProviderEventOutcome {
        self.events
            .pop_front()
            .unwrap_or(ProviderEventOutcome::Idle)
    }
}

#[derive(Default)]
pub struct InMemoryMetadataStore {
    writes: Mutex<Vec<SessionMetadataDelta>>,
}

impl InMemoryMetadataStore {
    pub fn writes(&self) -> Vec<SessionMetadataDelta> {
        self.writes.lock().expect("metadata writes lock").clone()
    }
}

impl SessionMetadataStore for InMemoryMetadataStore {
    fn load_workspace(
        &self,
        _selector: &WorkspaceSelector,
        _limits: MetadataLoadLimits,
    ) -> Result<MetadataSnapshot, MetadataError> {
        Ok(MetadataSnapshot {
            workspaces: BoundedVec::new(),
            sessions: BoundedVec::new(),
            truncated: false,
            corrupt_records_ignored: 0,
        })
    }

    fn merge(&self, delta: &SessionMetadataDelta, _deadline: Instant) -> MetadataWriteOutcome {
        self.writes
            .lock()
            .expect("metadata writes lock")
            .push(delta.clone());
        MetadataWriteOutcome::Updated
    }

    fn prune(
        &self,
        _policy: &RetentionPolicy,
        _budget: MaintenanceBudget,
    ) -> Result<PruneSummary, MetadataError> {
        Ok(PruneSummary {
            inspected: 0,
            removed: 0,
            truncated: false,
        })
    }
}

pub struct FakePublisher {
    outcome: PublishOutcome,
    published: Mutex<Vec<EventEnvelope>>,
}

impl FakePublisher {
    pub fn new(outcome: PublishOutcome) -> Self {
        Self {
            outcome,
            published: Mutex::new(Vec::new()),
        }
    }

    pub fn published(&self) -> Vec<EventEnvelope> {
        self.published.lock().expect("published lock").clone()
    }
}

impl LiveObservationPublisher for FakePublisher {
    fn publish(&self, event: &EventEnvelope, _deadline: Instant) -> PublishOutcome {
        self.published
            .lock()
            .expect("published lock")
            .push(event.clone());
        self.outcome
    }
}
