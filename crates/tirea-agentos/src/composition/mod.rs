mod agent_definition;
mod builder;
mod bundle;
mod config;
mod delegation;
mod errors;
pub mod registry;
mod stop_condition;
mod wiring;

pub use agent_definition::{AgentDefinition, ToolExecutionMode};
pub use builder::AgentOsBuilder;
pub use bundle::ToolBehaviorBundle;
pub use bundle::{
    BundleComposeError, BundleComposer, BundleRegistryAccumulator, BundleRegistryKind,
};
pub use config::{
    A2aAgentConfig, AgentConfig, AgentConfigEntry, AgentConfigError, AgentToolsConfig,
    LocalAgentConfig, RemoteAuthConfig, SkillsConfig, TaggedAgentConfigEntry,
    ToolExecutionModeConfig,
};
pub use delegation::{
    A2aAgentBinding, AgentBinding, AgentCatalog, AgentCatalogError, AgentDefinitionSpec,
    AgentDescriptor, CompositeAgentCatalog, HostedAgentCatalog, InMemoryAgentCatalog,
    RemoteAgentBinding, RemoteAgentDefinition, RemoteSecurityConfig, ResolvedAgent,
};
pub use errors::{AgentOsBuildError, AgentOsWiringError};
pub use registry::RegistrySet;
pub use registry::{
    AgentRegistry, AgentRegistryError, BehaviorRegistry, BehaviorRegistryError, ModelDefinition,
    ModelRegistry, ModelRegistryError, ProviderRegistry, ProviderRegistryError, RegistryBundle,
    StopPolicyRegistry, ToolRegistry, ToolRegistryError,
};
pub use registry::{
    CompositeAgentRegistry, CompositeBehaviorRegistry, CompositeModelRegistry,
    CompositeProviderRegistry, CompositeToolRegistry, InMemoryAgentRegistry,
    InMemoryBehaviorRegistry, InMemoryModelRegistry, InMemoryProviderRegistry,
    InMemoryToolRegistry,
};
pub use registry::{
    CompositeStopPolicyRegistry, InMemoryStopPolicyRegistry, StopPolicyRegistryError,
};
pub use stop_condition::StopConditionSpec;
pub use wiring::{SystemWiring, WiringContext};
