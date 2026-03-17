use super::{
    bundle::BundleComposeError,
    delegation::AgentCatalogError,
    registry::{
        AgentRegistryError, BehaviorRegistryError, ModelRegistryError, ProviderRegistryError,
        StopPolicyRegistryError, ToolRegistryError,
    },
};
#[cfg(feature = "skills")]
use tirea_extension_skills::{SkillError, SkillRegistryError, SkillRegistryManagerError};

#[derive(Debug, thiserror::Error)]
pub enum AgentOsWiringError {
    #[error("reserved behavior id cannot be used: {0}")]
    ReservedBehaviorId(String),

    #[error("behavior not found: {0}")]
    BehaviorNotFound(String),

    #[error("stop condition not found: {0}")]
    StopConditionNotFound(String),

    #[error("behavior id already installed: {0}")]
    BehaviorAlreadyInstalled(String),

    #[cfg(feature = "skills")]
    #[error("skills tool id already registered: {0}")]
    SkillsToolIdConflict(String),

    #[cfg(feature = "skills")]
    #[error("skills behavior already installed: {0}")]
    SkillsBehaviorAlreadyInstalled(String),

    #[cfg(feature = "skills")]
    #[error("skills enabled but no skills configured")]
    SkillsNotConfigured,

    #[error("agent tool id already registered: {0}")]
    AgentToolIdConflict(String),

    #[error("agent tools behavior already installed: {0}")]
    AgentToolsBehaviorAlreadyInstalled(String),

    #[error("agent recovery behavior already installed: {0}")]
    AgentRecoveryBehaviorAlreadyInstalled(String),

    #[error("bundle '{bundle_id}' includes unsupported contribution in wiring: {kind}")]
    BundleUnsupportedContribution { bundle_id: String, kind: String },

    #[error("bundle '{bundle_id}' tool id already registered: {id}")]
    BundleToolIdConflict { bundle_id: String, id: String },

    #[error("bundle '{bundle_id}' behavior id mismatch: key={key} behavior.id()={behavior_id}")]
    BundleBehaviorIdMismatch {
        bundle_id: String,
        key: String,
        behavior_id: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AgentOsBuildError {
    #[error(transparent)]
    Agents(#[from] AgentRegistryError),

    #[error(transparent)]
    Bundle(#[from] BundleComposeError),

    #[error(transparent)]
    Tools(#[from] ToolRegistryError),

    #[error(transparent)]
    Behaviors(#[from] BehaviorRegistryError),

    #[error(transparent)]
    Providers(#[from] ProviderRegistryError),

    #[error(transparent)]
    Models(#[from] ModelRegistryError),

    #[error(transparent)]
    AgentCatalog(#[from] AgentCatalogError),

    #[cfg(feature = "skills")]
    #[error(transparent)]
    Skills(#[from] SkillError),

    #[cfg(feature = "skills")]
    #[error(transparent)]
    SkillRegistry(#[from] SkillRegistryError),

    #[cfg(feature = "skills")]
    #[error(transparent)]
    SkillRegistryManager(#[from] SkillRegistryManagerError),

    #[error(transparent)]
    StopPolicies(#[from] StopPolicyRegistryError),

    #[error("agent {agent_id} references an empty behavior id")]
    AgentEmptyBehaviorRef { agent_id: String },

    #[error("agent {agent_id} references reserved behavior id: {behavior_id}")]
    AgentReservedBehaviorId {
        agent_id: String,
        behavior_id: String,
    },

    #[error("agent {agent_id} references unknown behavior id: {behavior_id}")]
    AgentBehaviorNotFound {
        agent_id: String,
        behavior_id: String,
    },

    #[error("agent {agent_id} has duplicate behavior reference: {behavior_id}")]
    AgentDuplicateBehaviorRef {
        agent_id: String,
        behavior_id: String,
    },

    #[error("agent {agent_id} references an empty stop condition id")]
    AgentEmptyStopConditionRef { agent_id: String },

    #[error("agent {agent_id} references unknown stop condition id: {stop_condition_id}")]
    AgentStopConditionNotFound {
        agent_id: String,
        stop_condition_id: String,
    },

    #[error("agent {agent_id} has duplicate stop condition reference: {stop_condition_id}")]
    AgentDuplicateStopConditionRef {
        agent_id: String,
        stop_condition_id: String,
    },

    #[error("models configured but no ProviderRegistry configured")]
    ProvidersNotConfigured,

    #[error("provider not found: {provider_id} (for model id: {model_id})")]
    ProviderNotFound {
        provider_id: String,
        model_id: String,
    },

    #[cfg(feature = "skills")]
    #[error("skills enabled but no skills configured")]
    SkillsNotConfigured,
}
