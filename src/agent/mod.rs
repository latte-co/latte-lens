//! Vendor-neutral Code Agent observability contracts.
//!
//! This feature-gated module implements the vendor-neutral core described in
//! `docs/design/code-agent-observability.md`: bounded contracts, deterministic
//! state reduction, metadata-only persistence, local live transport, provider
//! runtime seams, and the application view model. Real integrations stay
//! isolated behind explicit adapters.

mod adapter;
mod bootstrap;
mod bounded;
mod claude;
mod codex;
mod contract;
mod crypto;
mod dispatcher;
mod envelope;
mod error;
mod explain;
mod hook;
mod hook_json;
mod identity;
mod live;
mod metadata;
mod model;
mod opencode;
mod provider;
mod runtime;
mod state;
mod traex;
mod transport;
mod workspace;

pub use adapter::*;
pub use bootstrap::*;
pub use bounded::*;
pub use claude::*;
pub use codex::*;
pub use contract::*;
pub use crypto::*;
pub use dispatcher::*;
pub use envelope::*;
pub use error::*;
pub use explain::*;
pub use hook::*;
pub use identity::*;
pub use live::*;
pub use metadata::*;
pub use model::*;
pub use opencode::*;
pub use provider::*;
pub use runtime::*;
pub use state::*;
pub use traex::*;
pub use transport::*;
pub use workspace::*;

/// Build the explicitly approved production adapter registry.
pub fn production_adapter_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    registry
        .register(std::sync::Arc::new(CodexHookAdapter::new()))
        .expect("production observer ids are unique");
    registry
        .register(std::sync::Arc::new(ClaudeHookAdapter::new()))
        .expect("production observer ids are unique");
    registry
        .register(std::sync::Arc::new(OpenCodePluginAdapter::new()))
        .expect("production observer ids are unique");
    registry
        .register(std::sync::Arc::new(TraexHookAdapter::new()))
        .expect("production observer ids are unique");
    registry
}
