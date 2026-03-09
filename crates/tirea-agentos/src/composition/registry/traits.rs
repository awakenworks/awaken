use crate::composition::AgentDefinition;
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::AgentBehavior;
use genai::chat::ChatOptions;
use genai::Client;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ToolRegistryError {
    #[error("tool id already registered: {0}")]
    ToolIdConflict(String),

    #[error("tool id mismatch: key={key} descriptor.id={descriptor_id}")]
    ToolIdMismatch { key: String, descriptor_id: String },
}

pub trait ToolRegistry: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Tool>>;

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>>;
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderRegistryError {
    #[error("provider id already registered: {0}")]
    ProviderIdConflict(String),

    #[error("provider id must be non-empty")]
    EmptyProviderId,
}

pub trait ProviderRegistry: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<Client>;

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, Client>;
}

#[derive(Debug, thiserror::Error)]
pub enum BehaviorRegistryError {
    #[error("behavior id already registered: {0}")]
    BehaviorIdConflict(String),

    #[error("behavior id mismatch: key={key} behavior.id()={behavior_id}")]
    BehaviorIdMismatch { key: String, behavior_id: String },
}

pub trait BehaviorRegistry: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<Arc<dyn AgentBehavior>>;

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, Arc<dyn AgentBehavior>>;
}

#[derive(Debug, thiserror::Error)]
pub enum AgentRegistryError {
    #[error("agent id already registered: {0}")]
    AgentIdConflict(String),
}

pub trait AgentRegistry: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<AgentDefinition>;

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, AgentDefinition>;
}

#[derive(Debug, Clone)]
pub struct ModelDefinition {
    pub provider: String,
    pub model: String,
    pub chat_options: Option<ChatOptions>,
}

impl ModelDefinition {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            chat_options: None,
        }
    }

    pub fn with_chat_options(mut self, opts: ChatOptions) -> Self {
        self.chat_options = Some(opts);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModelRegistryError {
    #[error("model id already registered: {0}")]
    ModelIdConflict(String),

    #[error("provider id must be non-empty")]
    EmptyProviderId,

    #[error("model name must be non-empty")]
    EmptyModelName,
}

pub trait ModelRegistry: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<ModelDefinition>;

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, ModelDefinition>;
}

/// Bundle-level contributor for registry composition.
///
/// A bundle contributes either direct definitions (maps) and/or registry sources.
/// The orchestrator composes these contributions into concrete
/// in-memory/composite registries with deterministic conflict checks.
pub trait RegistryBundle: Send + Sync {
    /// Stable bundle identifier for diagnostics.
    fn id(&self) -> &str;

    fn agent_definitions(&self) -> HashMap<String, AgentDefinition> {
        HashMap::new()
    }

    fn agent_registries(&self) -> Vec<Arc<dyn AgentRegistry>> {
        Vec::new()
    }

    fn tool_definitions(&self) -> HashMap<String, Arc<dyn Tool>> {
        HashMap::new()
    }

    fn tool_registries(&self) -> Vec<Arc<dyn ToolRegistry>> {
        Vec::new()
    }

    fn behavior_definitions(&self) -> HashMap<String, Arc<dyn AgentBehavior>> {
        HashMap::new()
    }

    fn behavior_registries(&self) -> Vec<Arc<dyn BehaviorRegistry>> {
        Vec::new()
    }

    fn provider_definitions(&self) -> HashMap<String, Client> {
        HashMap::new()
    }

    fn provider_registries(&self) -> Vec<Arc<dyn ProviderRegistry>> {
        Vec::new()
    }

    fn model_definitions(&self) -> HashMap<String, ModelDefinition> {
        HashMap::new()
    }

    fn model_registries(&self) -> Vec<Arc<dyn ModelRegistry>> {
        Vec::new()
    }
}
