use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use super::{
    AgentRuntime, AgentRuntimeServices, FilesystemLiveReceiverRegistry, FilesystemMetadataStore,
    IdentityKeyer, InstanceRegistry, LiveIngressPolicy, SessionMetadataStore, WorkspaceSelector,
    bind_registered_live_receiver, load_or_create_install_identity, production_adapter_registry,
    resolve_runtime_root_from_environment, resolve_state_root_from_environment, resolve_workspace,
};

pub struct ProductionAgentRuntime {
    pub runtime: AgentRuntime,
    pub selector: WorkspaceSelector,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentBootstrapError {
    StateUnavailable,
    WorkspaceUnavailable,
    RuntimeUnavailable,
}

impl fmt::Display for AgentBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Agent observability bootstrap failed: {self:?}")
    }
}

impl Error for AgentBootstrapError {}

/// Build the real Lens Agent runtime with only explicitly approved adapters.
/// The provider registry remains empty until a read-only integration is added.
pub fn start_production_agent_runtime(
    selected_path: &Path,
) -> Result<ProductionAgentRuntime, AgentBootstrapError> {
    let state_root =
        resolve_state_root_from_environment().map_err(|_| AgentBootstrapError::StateUnavailable)?;
    let runtime_root = resolve_runtime_root_from_environment()
        .map_err(|_| AgentBootstrapError::RuntimeUnavailable)?;
    start_production_agent_runtime_with_roots(selected_path, state_root, runtime_root)
}

fn start_production_agent_runtime_with_roots(
    selected_path: &Path,
    state_root: PathBuf,
    runtime_root: PathBuf,
) -> Result<ProductionAgentRuntime, AgentBootstrapError> {
    let identity = load_or_create_install_identity(state_root.clone())
        .map_err(|_| AgentBootstrapError::StateUnavailable)?;
    let install = identity.install_id().clone();
    let workspace = resolve_workspace(selected_path, &identity)
        .map_err(|_| AgentBootstrapError::WorkspaceUnavailable)?;
    let selector = workspace.selector().clone();

    let adapters = Arc::new(production_adapter_registry());
    let instances = Arc::new(RwLock::new(InstanceRegistry::new()));
    let identity: Arc<dyn IdentityKeyer> = Arc::new(identity);
    let metadata: Arc<dyn SessionMetadataStore> = Arc::new(
        FilesystemMetadataStore::new(state_root, install.clone())
            .map_err(|_| AgentBootstrapError::StateUnavailable)?,
    );
    let policy = Arc::new(LiveIngressPolicy::new(
        1,
        install.clone(),
        selector.workspaces().iter().cloned(),
        Arc::clone(&adapters),
        Arc::clone(&instances),
    ));
    let registry = FilesystemLiveReceiverRegistry::new(runtime_root, install)
        .map_err(|_| AgentBootstrapError::RuntimeUnavailable)?;
    let receiver = bind_registered_live_receiver(&registry, policy, selector.workspaces())
        .map_err(|_| AgentBootstrapError::RuntimeUnavailable)?;

    let mut services = AgentRuntimeServices::new(adapters, identity, metadata);
    services.instances = instances;
    services.receiver = Some(receiver);
    Ok(ProductionAgentRuntime {
        runtime: AgentRuntime::start(services),
        selector,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn production_bootstrap_registers_and_removes_its_receiver_lease() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let workspace = sandbox.path().join("workspace");
        let state_root = sandbox.path().join("state");
        let runtime_root = sandbox.path().join("runtime");
        fs::create_dir_all(workspace.join(".git")).expect("workspace");
        let agent = start_production_agent_runtime_with_roots(
            &workspace,
            state_root.clone(),
            runtime_root.clone(),
        )
        .expect("bootstrap");
        let identity = load_or_create_install_identity(state_root).expect("identity");
        let registry =
            FilesystemLiveReceiverRegistry::new(runtime_root, identity.install_id().clone())
                .expect("registry");
        let selected = agent.selector.workspaces().to_vec();
        assert_eq!(
            registry
                .discover_matching(&selected)
                .expect("live receiver")
                .len(),
            1
        );

        drop(agent);
        assert!(
            registry
                .discover_matching(&selected)
                .expect("receiver removed")
                .is_empty()
        );
    }
}
