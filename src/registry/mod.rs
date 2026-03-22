//! Registry traits, in-memory implementations, and agent resolution.
//!
//! See ADR-0010 for the full design rationale.

pub mod config;
pub mod memory;
pub mod resolve;
pub mod spec;
pub mod traits;

pub use config::{AgentSystemConfig, ModelConfig};
pub use memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};
pub use resolve::ResolveError;
pub use spec::AgentSpec;
pub use traits::{
    AgentSpecRegistry, ModelEntry, ModelRegistry, PluginSource, ProviderRegistry, RegistrySet,
    ToolRegistry,
};
