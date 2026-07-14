use std::collections::BTreeMap;

use super::{
    AgentObservation, BoundedSet, BoundedText, ContractRevision, EvidenceAuthority, EvidenceDomain,
    EvidenceProvenance, ObservationEnvelope, ObservationError, ObservedEntityKind, ObserverId,
    ObserverInstanceId, SnapshotScope, StreamEpoch, StreamOp, StreamRef, SubjectNamespace,
    WorkspaceScope,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AcquisitionMode {
    HookEvent,
    NativeSnapshot,
    NativeEventStream,
    AggregatedSnapshot,
    AggregatedEventStream,
    ProcessPresence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterfaceStability {
    Stable,
    VersionedExperimental,
    PrivateExperimental,
    Unknown,
}

impl InterfaceStability {
    const fn rank(self) -> u8 {
        match self {
            Self::Stable => 3,
            Self::VersionedExperimental => 2,
            Self::PrivateExperimental => 1,
            Self::Unknown => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotSemantics {
    pub supported: bool,
    pub atomic_boundary: bool,
    pub chunked: bool,
    pub provides_watermark: bool,
}

impl SnapshotSemantics {
    pub const fn unsupported() -> Self {
        Self {
            supported: false,
            atomic_boundary: false,
            chunked: false,
            provides_watermark: false,
        }
    }

    const fn permits(self, actual: Self) -> bool {
        (!actual.supported || self.supported)
            && (!actual.atomic_boundary || self.atomic_boundary)
            && (!actual.chunked || self.chunked)
            && (!actual.provides_watermark || self.provides_watermark)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamSemantics {
    pub supported: bool,
    pub sequenced: bool,
    pub reports_reset: bool,
    pub reports_gap: bool,
}

impl StreamSemantics {
    pub const fn unsupported() -> Self {
        Self {
            supported: false,
            sequenced: false,
            reports_reset: false,
            reports_gap: false,
        }
    }

    const fn permits(self, actual: Self) -> bool {
        (!actual.supported || self.supported)
            && (!actual.sequenced || self.sequenced)
            && (!actual.reports_reset || self.reports_reset)
            && (!actual.reports_gap || self.reports_gap)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityClaim {
    pub support: super::CapabilitySupport,
    pub max_authority: EvidenceAuthority,
    pub provenance: EvidenceProvenance,
    pub reason: BoundedText<128>,
    /// Whether `Lifecycle::Open` may carry a finite validity lease.
    pub lease_backed: bool,
}

impl CapabilityClaim {
    fn permits(&self, actual: &super::EvidenceClaim) -> Result<(), ObservationError> {
        if !self.support.permits(actual.support) {
            return Err(ObservationError::UnsupportedCapability);
        }
        if !self.max_authority.permits(actual.authority) {
            return Err(ObservationError::AuthorityExceeded);
        }
        if self.provenance != actual.provenance {
            return Err(ObservationError::ProvenanceMismatch);
        }
        Ok(())
    }

    fn is_within(&self, template: &Self) -> bool {
        template.support.permits(self.support)
            && template.max_authority.permits(self.max_authority)
            && self.provenance == template.provenance
            && (!self.lease_backed || template.lease_backed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceContractTemplate {
    pub observer: ObserverId,
    pub subjects: BoundedSet<SubjectNamespace, 32>,
    pub acquisition: BoundedSet<AcquisitionMode, 6>,
    pub capabilities: BTreeMap<EvidenceDomain, CapabilityClaim>,
    pub snapshot_semantics: SnapshotSemantics,
    pub stream_semantics: StreamSemantics,
    pub requires_instrumentation: bool,
    pub stability: InterfaceStability,
}

impl InstanceContractTemplate {
    /// Verify that runtime probing only narrowed the adapter's static template.
    pub fn permits(&self, contract: &InstanceContract) -> bool {
        self.observer == contract.observer
            && contract
                .subjects
                .iter()
                .all(|item| self.subjects.contains(item))
            && contract
                .acquisition
                .iter()
                .all(|item| self.acquisition.contains(item))
            && contract.capabilities.iter().all(|(domain, claim)| {
                self.capabilities
                    .get(domain)
                    .is_some_and(|template| claim.is_within(template))
            })
            && self.snapshot_semantics.permits(contract.snapshot_semantics)
            && self.stream_semantics.permits(contract.stream_semantics)
            && (!self.requires_instrumentation || contract.requires_instrumentation)
            && contract.stability.rank() <= self.stability.rank()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceContract {
    pub observer: ObserverId,
    pub instance: ObserverInstanceId,
    pub revision: ContractRevision,
    pub observer_version: Option<BoundedText<64>>,
    pub subjects: BoundedSet<SubjectNamespace, 32>,
    pub acquisition: BoundedSet<AcquisitionMode, 6>,
    pub capabilities: BTreeMap<EvidenceDomain, CapabilityClaim>,
    pub snapshot_semantics: SnapshotSemantics,
    pub stream_semantics: StreamSemantics,
    pub requires_instrumentation: bool,
    pub stability: InterfaceStability,
}

impl InstanceContract {
    pub fn validate_envelope(
        &self,
        envelope: &ObservationEnvelope,
    ) -> Result<(), ObservationError> {
        let stream = envelope.stream();
        if stream.observer != self.observer {
            return Err(ObservationError::ObserverMismatch);
        }
        if stream.instance != self.instance {
            return Err(ObservationError::InstanceMismatch);
        }

        if let ObservationEnvelope::Snapshot(snapshot) = envelope {
            if !self.snapshot_semantics.supported
                || !self.supports_any(&[
                    AcquisitionMode::NativeSnapshot,
                    AcquisitionMode::AggregatedSnapshot,
                ])
            {
                return Err(ObservationError::UnsupportedCapability);
            }
            self.validate_snapshot_scope(&snapshot.scope)?;
            if snapshot
                .observations
                .iter()
                .any(|observation| !Self::scope_covers(&snapshot.scope, observation))
            {
                return Err(ObservationError::InvalidSnapshotScope);
            }
        }

        if let ObservationEnvelope::Event(event) = envelope {
            if !self.supports_any(&[
                AcquisitionMode::HookEvent,
                AcquisitionMode::NativeEventStream,
                AcquisitionMode::AggregatedEventStream,
            ]) {
                return Err(ObservationError::UnsupportedCapability);
            }
            if !self.acquisition.contains(&AcquisitionMode::HookEvent)
                && !self.stream_semantics.supported
            {
                return Err(ObservationError::UnsupportedCapability);
            }
            if matches!(event.op, StreamOp::Reset | StreamOp::Gap { .. })
                && !self.stream_semantics.supported
            {
                return Err(ObservationError::UnsupportedCapability);
            }
            if let StreamOp::Delete { entity, domains } = &event.op {
                if let Some(subject) = entity.subject() {
                    self.validate_subject(subject)?;
                }
                for domain in domains.iter() {
                    let claim = self
                        .capabilities
                        .get(domain)
                        .ok_or(ObservationError::UnsupportedCapability)?;
                    if !matches!(
                        claim.support,
                        super::CapabilitySupport::Confirmed | super::CapabilitySupport::Partial
                    ) || claim.max_authority != EvidenceAuthority::Authoritative
                    {
                        return Err(ObservationError::DestructiveOperationDenied);
                    }
                }
            }
        }

        for observation in envelope.observations() {
            self.validate_observation(observation)?;
        }
        Ok(())
    }

    fn supports_any(&self, modes: &[AcquisitionMode]) -> bool {
        modes.iter().any(|mode| self.acquisition.contains(mode))
    }

    fn validate_subject(&self, subject: &SubjectNamespace) -> Result<(), ObservationError> {
        if self.subjects.contains(subject) {
            Ok(())
        } else {
            Err(ObservationError::UnsupportedSubject)
        }
    }

    fn validate_observation(&self, observation: &AgentObservation) -> Result<(), ObservationError> {
        if let Some(session) = &observation.session {
            self.validate_subject(session.key().subject())?;
        } else if let Some(subject) = observation
            .presence
            .as_ref()
            .and_then(super::PresenceRef::subject_hint)
        {
            self.validate_subject(subject)?;
        }

        let capability = self
            .capabilities
            .get(&observation.domain())
            .ok_or(ObservationError::UnsupportedCapability)?;
        capability.permits(&observation.evidence)?;

        if observation.kind.is_destructive()
            && observation.evidence.authority != EvidenceAuthority::Authoritative
        {
            return Err(ObservationError::DestructiveOperationDenied);
        }
        if observation.valid_until.is_some()
            && matches!(
                observation.kind,
                super::ObservationKind::Lifecycle(super::LifecycleOp::Set(
                    super::ReportedSessionLifecycle::Open
                ))
            )
            && !capability.lease_backed
        {
            return Err(ObservationError::UnsupportedCapability);
        }
        Ok(())
    }

    fn validate_snapshot_scope(&self, scope: &SnapshotScope) -> Result<(), ObservationError> {
        if !scope
            .subjects
            .iter()
            .all(|subject| self.subjects.contains(subject))
            || !scope.domains.iter().all(|domain| {
                self.capabilities.get(domain).is_some_and(|claim| {
                    matches!(
                        claim.support,
                        super::CapabilitySupport::Confirmed | super::CapabilitySupport::Partial
                    )
                })
            })
        {
            return Err(ObservationError::InvalidSnapshotScope);
        }
        Ok(())
    }

    fn scope_covers(scope: &SnapshotScope, observation: &AgentObservation) -> bool {
        if !scope.domains.contains(&observation.domain()) {
            return false;
        }

        let subject = observation
            .session
            .as_ref()
            .map(|session| session.key().subject())
            .or_else(|| {
                observation
                    .presence
                    .as_ref()
                    .and_then(super::PresenceRef::subject_hint)
            });
        if subject.is_some_and(|subject| !scope.subjects.contains(subject)) {
            return false;
        }

        let workspace = observation
            .workspace
            .as_ref()
            .or_else(|| {
                observation
                    .session
                    .as_ref()
                    .map(super::SessionRef::workspace)
            })
            .or_else(|| {
                observation
                    .presence
                    .as_ref()
                    .and_then(super::PresenceRef::workspace)
            });
        if let WorkspaceScope::Explicit(workspaces) = &scope.workspaces
            && workspace.is_some_and(|workspace| !workspaces.contains(workspace))
        {
            return false;
        }

        let entity_kind = match &observation.kind {
            super::ObservationKind::Presence(_) => Some(ObservedEntityKind::Presence),
            super::ObservationKind::Agent(_) => Some(ObservedEntityKind::Agent),
            super::ObservationKind::Turn(_) => Some(ObservedEntityKind::Turn),
            super::ObservationKind::Artifact(_) => Some(ObservedEntityKind::Artifact),
            super::ObservationKind::Diagnostic(_) if observation.session.is_none() => None,
            _ => Some(ObservedEntityKind::Session),
        };
        entity_kind.is_none_or(|kind| scope.entity_kinds.contains(&kind))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractUpdate {
    Inserted,
    Updated,
    Unchanged,
}

#[derive(Clone, Debug)]
struct RegisteredInstance {
    contract: InstanceContract,
    epoch: StreamEpoch,
}

/// Tracks the current contract revision and epoch for each logical instance.
#[derive(Default)]
pub struct InstanceRegistry {
    instances: BTreeMap<(ObserverId, ObserverInstanceId), RegisteredInstance>,
}

impl InstanceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(
        &mut self,
        contract: InstanceContract,
        epoch: StreamEpoch,
    ) -> Result<ContractUpdate, ObservationError> {
        let key = (contract.observer.clone(), contract.instance.clone());
        let Some(current) = self.instances.get_mut(&key) else {
            self.instances
                .insert(key, RegisteredInstance { contract, epoch });
            return Ok(ContractUpdate::Inserted);
        };

        if contract.revision < current.contract.revision {
            return Err(ObservationError::StaleContractRevision);
        }
        if contract.revision == current.contract.revision
            && contract == current.contract
            && epoch == current.epoch
        {
            return Ok(ContractUpdate::Unchanged);
        }
        if contract.revision == current.contract.revision && contract != current.contract {
            return Err(ObservationError::StaleContractRevision);
        }
        current.contract = contract;
        current.epoch = epoch;
        Ok(ContractUpdate::Updated)
    }

    pub fn contract_for(&self, stream: &StreamRef) -> Result<&InstanceContract, ObservationError> {
        let registered = self
            .instances
            .get(&(stream.observer.clone(), stream.instance.clone()))
            .ok_or(ObservationError::InstanceMismatch)?;
        if registered.epoch != stream.epoch {
            return Err(ObservationError::WrongEpoch);
        }
        Ok(&registered.contract)
    }
}
