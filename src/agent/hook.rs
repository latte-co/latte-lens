use std::time::{Duration, Instant};

use super::{
    AdapterDelivery, AdapterInput, AdapterRegistry, BoundedText, ContractRevision, DispatchOutcome,
    HookDecodeOutcome, IdentityKeyer, LiveObservationPublisher, ObservationDispatcher, ObserverId,
    SessionMetadataStore, Timestamp, WorkspaceHint,
};

/// Bounded, vendor-neutral invocation passed from the Hook CLI to one adapter.
pub struct HookInvocation<'a> {
    pub observer: &'a ObserverId,
    pub event_name: &'a str,
    pub observer_version: Option<&'a str>,
    pub observed_at: Timestamp,
    pub workspace: WorkspaceHint,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookEmitOutcome {
    Dispatched(DispatchOutcome),
    Ignored,
    Rejected,
}

/// Decode and dispatch one Hook invocation without any vendor branch in core.
pub fn emit_hook_invocation(
    invocation: HookInvocation<'_>,
    adapters: &AdapterRegistry,
    identity: &dyn IdentityKeyer,
    publisher: &dyn LiveObservationPublisher,
    metadata: &dyn SessionMetadataStore,
    live_deadline: Instant,
    metadata_budget: Duration,
) -> HookEmitOutcome {
    let Some(adapter) = adapters.resolve(invocation.observer) else {
        return HookEmitOutcome::Ignored;
    };
    let input = AdapterInput {
        delivery: AdapterDelivery::HookEvent,
        event_name: invocation.event_name,
        observer_version: invocation.observer_version,
        observed_at: invocation.observed_at,
        workspace: Some(invocation.workspace),
        payload: invocation.payload,
    };
    if input.validate_bounds().is_err() {
        return HookEmitOutcome::Rejected;
    }
    let decoded = match adapter.decode_hook(input, identity) {
        Ok(HookDecodeOutcome::Event(event)) => *event,
        Ok(HookDecodeOutcome::Ignore(_)) => return HookEmitOutcome::Ignored,
        Err(_) => return HookEmitOutcome::Rejected,
    };
    let observer_version = match invocation.observer_version {
        Some(version) => match BoundedText::try_new(version) {
            Ok(version) => Some(version),
            Err(_) => return HookEmitOutcome::Rejected,
        },
        None => None,
    };
    let contract = adapter
        .contract_template(invocation.observer_version)
        .hook_contract(
            decoded.stream.instance.clone(),
            ContractRevision::new(1),
            observer_version,
        );
    HookEmitOutcome::Dispatched(
        ObservationDispatcher::new(adapters, publisher, metadata).dispatch_with_budget(
            decoded,
            &contract,
            live_deadline,
            metadata_budget,
        ),
    )
}
