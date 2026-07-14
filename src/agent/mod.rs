//! Vendor-neutral Code Agent observability contracts.
//!
//! This module is the feature-gated C0 framework described in
//! `docs/design/code-agent-observability.md`. It intentionally contains no production
//! Code Agent adapter, provider, metadata store, transport, or application
//! integration. Enabling the feature only exposes the contracts and bounded
//! runtime seam.

mod adapter;
mod bounded;
mod contract;
mod dispatcher;
mod envelope;
mod error;
mod identity;
mod metadata;
mod model;
mod provider;
mod runtime;
mod state;
mod transport;

pub use adapter::*;
pub use bounded::*;
pub use contract::*;
pub use dispatcher::*;
pub use envelope::*;
pub use error::*;
pub use identity::*;
pub use metadata::*;
pub use model::*;
pub use provider::*;
pub use runtime::*;
pub use state::*;
pub use transport::*;

/// Build the production registry.
///
/// C0 deliberately returns an empty registry. Real integrations must be added
/// behind an explicit product decision instead of becoming implicit defaults.
pub fn production_adapter_registry() -> AdapterRegistry {
    AdapterRegistry::new()
}
