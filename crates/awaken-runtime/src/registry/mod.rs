//! Registry traits, in-memory implementations, and agent resolution.
//!
//! See ADR-0010 for the full design rationale.

pub mod composite;
pub mod config;
pub mod memory;
pub mod resolve;
pub mod resolver;
pub mod traits;

pub use awaken_contract::registry_spec::AgentSpec;
pub use composite::{CompositeAgentSpecRegistry, DiscoveryError, RemoteAgentSource};
pub use config::{AgentSystemConfig, ModelConfig};
pub use memory::{
    MapAgentRegistry, MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
    MapStopPolicyRegistry, MapToolRegistry,
};
pub use resolve::ResolveError;
pub use resolver::{AgentResolver, ResolvedAgent};
pub use traits::{
    AgentRegistry, AgentSpecRegistry, ModelEntry, ModelRegistry, PluginSource, ProviderRegistry,
    RegistrySet, StopPolicyRegistry, ToolRegistry,
};
