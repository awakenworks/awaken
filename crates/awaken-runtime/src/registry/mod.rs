//! Registry traits, in-memory implementations, and agent resolution.
//!
//! See ADR-0010 for the full design rationale.

#[cfg(feature = "a2a")]
pub mod composite;
pub mod config;
pub mod diagnostics;
pub mod lifecycle;
pub mod memory;
pub(crate) mod model_capabilities;
pub mod pinned;
pub mod resolve;
pub mod resolver;
pub mod snapshot;
pub mod traits;

pub use awaken_contract::registry_spec::AgentSpec;
#[cfg(feature = "a2a")]
pub use composite::{CompositeAgentSpecRegistry, DiscoveryError, RemoteAgentSource};
pub use config::AgentSystemConfig;
pub use diagnostics::{
    RegistryDiagnostic, RegistryDiagnosticSeverity, RegistryResourceRef, RegistryValidationError,
    SerializableRegistryDiagnostic, diagnose_agent_spec, diagnose_registry_set,
    diagnose_registry_set_serializable, validate_agent_spec, validate_registry_set,
};
pub use lifecycle::{
    ProviderRemovalImpact, ProviderRemovalPolicy, ProviderRemovalPreview, RegistryUpdateError,
    RuntimeRegistryUpdate, preview_provider_removal, rebuild_agent_model_provider_registries,
};
#[cfg(feature = "a2a")]
pub use memory::MapBackendRegistry;
pub use memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapRegistry,
    MapToolRegistry,
};
pub use pinned::{
    PinnedAgentSpecRegistry, PinnedModelRegistry, PinnedRegistryError, PinnedSpecMap,
};
pub use resolve::ResolveError;
pub use resolver::{AgentResolver, ResolvedAgent, ResolvedBackendAgent};
pub use snapshot::{RegistryHandle, RegistrySnapshot};
#[cfg(feature = "a2a")]
pub use traits::BackendRegistry;
pub use traits::{
    AgentSpecRegistry, ModelRegistry, PluginSource, ProviderRegistry, RegistrySet, ToolRegistry,
};
