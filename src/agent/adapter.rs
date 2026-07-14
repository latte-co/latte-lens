use std::{collections::BTreeMap, error::Error, fmt, sync::Arc};

use super::{
    AgentObservation, BoundedVec, ContractRevision, IdentityKeyer, InstanceContract,
    InstanceContractTemplate, ObservationEnvelope, ObservationError, ObserverDescriptor,
    ObserverId, Timestamp,
};

pub const MAX_ADAPTER_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_DECODED_OBSERVATIONS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterDelivery {
    HookEvent,
    ProviderSnapshotItem,
    ProviderEvent,
}

/// Bounded adapter input. Payload bytes are borrowed and never enter core models.
pub struct AdapterInput<'a> {
    pub delivery: AdapterDelivery,
    pub event_name: &'a str,
    pub observer_version: Option<&'a str>,
    pub observed_at: Timestamp,
    pub payload: &'a [u8],
}

impl AdapterInput<'_> {
    pub fn validate_bounds(&self) -> Result<(), AdapterError> {
        if self.payload.len() > MAX_ADAPTER_INPUT_BYTES {
            return Err(AdapterError::InputTooLarge);
        }
        if self.event_name.is_empty() || self.event_name.len() > 128 {
            return Err(AdapterError::InvalidEventName);
        }
        if self
            .observer_version
            .is_some_and(|version| version.len() > 64)
        {
            return Err(AdapterError::InvalidObserverVersion);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgnoreReason {
    UnsupportedEvent,
    MissingStableIdentity,
    OutsideSelectedWorkspace,
    NoObservableFact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeOutcome {
    Observations(BoundedVec<AgentObservation, MAX_DECODED_OBSERVATIONS>),
    Ignore(IgnoreReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterError {
    InputTooLarge,
    InvalidEventName,
    InvalidObserverVersion,
    MalformedInput,
    UnsupportedVersion,
    IdentityRejected,
    OutputRejected,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "adapter rejected input: {self:?}")
    }
}

impl Error for AdapterError {}

/// Vendor-specific decoding and normalization boundary.
pub trait CodeAgentAdapter: Send + Sync {
    fn descriptor(&self) -> ObserverDescriptor;

    fn contract_template(&self, observer_version: Option<&str>) -> InstanceContractTemplate;

    fn decode(
        &self,
        input: AdapterInput<'_>,
        identity: &dyn IdentityKeyer,
    ) -> Result<DecodeOutcome, AdapterError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateObserverId {
    pub observer: ObserverId,
}

impl fmt::Display for DuplicateObserverId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an adapter with this observer id is already registered")
    }
}

impl Error for DuplicateObserverId {}

/// Concrete adapter registry. No default or fallback decoder exists.
#[derive(Default)]
pub struct AdapterRegistry {
    adapters: BTreeMap<ObserverId, Arc<dyn CodeAgentAdapter>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        adapter: Arc<dyn CodeAgentAdapter>,
    ) -> Result<(), DuplicateObserverId> {
        let observer = adapter.descriptor().id;
        if self.adapters.contains_key(&observer) {
            return Err(DuplicateObserverId { observer });
        }
        self.adapters.insert(observer, adapter);
        Ok(())
    }

    pub fn resolve(&self, observer: &ObserverId) -> Option<&dyn CodeAgentAdapter> {
        self.adapters.get(observer).map(Arc::as_ref)
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    pub fn validate_envelope(
        &self,
        envelope: ObservationEnvelope,
        contract: &InstanceContract,
    ) -> Result<ValidatedEnvelope, ObservationError> {
        let adapter = self
            .resolve(&envelope.stream().observer)
            .ok_or(ObservationError::UnknownObserver)?;
        let template = adapter.contract_template(
            contract
                .observer_version
                .as_ref()
                .map(super::BoundedText::as_str),
        );
        if !template.permits(contract) {
            return Err(ObservationError::UnsupportedCapability);
        }
        envelope.validate_shape()?;
        contract.validate_envelope(&envelope)?;
        Ok(ValidatedEnvelope {
            envelope,
            contract_revision: contract.revision,
        })
    }

    /// Validate against the registry's current instance epoch and contract.
    pub fn validate_registered_envelope(
        &self,
        envelope: ObservationEnvelope,
        instances: &super::InstanceRegistry,
    ) -> Result<ValidatedEnvelope, ObservationError> {
        let contract = instances.contract_for(envelope.stream())?;
        self.validate_envelope(envelope, contract)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedEnvelope {
    envelope: ObservationEnvelope,
    contract_revision: ContractRevision,
}

impl ValidatedEnvelope {
    pub const fn envelope(&self) -> &ObservationEnvelope {
        &self.envelope
    }

    pub const fn contract_revision(&self) -> ContractRevision {
        self.contract_revision
    }

    pub fn into_inner(self) -> ObservationEnvelope {
        self.envelope
    }
}
