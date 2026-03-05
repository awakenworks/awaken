use super::{
    AgentRegistry, BehaviorRegistry, ModelDefinition, ModelRegistry, ProviderRegistry,
    RegistryBundle, ToolRegistry,
};
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::AgentBehavior;
use crate::orchestrator::AgentDefinition;
use genai::Client;
use std::collections::HashMap;
use std::sync::Arc;

/// Aggregated registry set used by [`super::super::AgentOs`] after build-time composition.
#[derive(Clone)]
pub struct RegistrySet {
    pub agents: Arc<dyn AgentRegistry>,
    pub tools: Arc<dyn ToolRegistry>,
    pub behaviors: Arc<dyn BehaviorRegistry>,
    pub providers: Arc<dyn ProviderRegistry>,
    pub models: Arc<dyn ModelRegistry>,
}

impl RegistrySet {
    pub fn new(
        agents: Arc<dyn AgentRegistry>,
        tools: Arc<dyn ToolRegistry>,
        behaviors: Arc<dyn BehaviorRegistry>,
        providers: Arc<dyn ProviderRegistry>,
        models: Arc<dyn ModelRegistry>,
    ) -> Self {
        Self {
            agents,
            tools,
            behaviors,
            providers,
            models,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleRegistryKind {
    Agent,
    Tool,
    Behavior,
    Provider,
    Model,
}

impl std::fmt::Display for BundleRegistryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent => write!(f, "agent"),
            Self::Tool => write!(f, "tool"),
            Self::Behavior => write!(f, "behavior"),
            Self::Provider => write!(f, "provider"),
            Self::Model => write!(f, "model"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BundleComposeError {
    #[error("bundle '{bundle_id}' contributed duplicate {kind} id: {id}")]
    DuplicateId {
        bundle_id: String,
        kind: BundleRegistryKind,
        id: String,
    },

    #[error("bundle '{bundle_id}' contributed empty {kind} id")]
    EmptyId {
        bundle_id: String,
        kind: BundleRegistryKind,
    },
}

pub struct BundleRegistryAccumulator<'a> {
    pub agent_definitions: &'a mut HashMap<String, AgentDefinition>,
    pub agent_registries: &'a mut Vec<Arc<dyn AgentRegistry>>,
    pub tool_definitions: &'a mut HashMap<String, Arc<dyn Tool>>,
    pub tool_registries: &'a mut Vec<Arc<dyn ToolRegistry>>,
    pub behavior_definitions: &'a mut HashMap<String, Arc<dyn AgentBehavior>>,
    pub behavior_registries: &'a mut Vec<Arc<dyn BehaviorRegistry>>,
    pub provider_definitions: &'a mut HashMap<String, Client>,
    pub provider_registries: &'a mut Vec<Arc<dyn ProviderRegistry>>,
    pub model_definitions: &'a mut HashMap<String, ModelDefinition>,
    pub model_registries: &'a mut Vec<Arc<dyn ModelRegistry>>,
}

pub struct BundleComposer;

impl BundleComposer {
    pub fn apply(
        bundles: &[Arc<dyn RegistryBundle>],
        acc: BundleRegistryAccumulator<'_>,
    ) -> Result<(), BundleComposeError> {
        let BundleRegistryAccumulator {
            agent_definitions,
            agent_registries,
            tool_definitions,
            tool_registries,
            behavior_definitions,
            behavior_registries,
            provider_definitions,
            provider_registries,
            model_definitions,
            model_registries,
        } = acc;

        for bundle in bundles {
            let bundle_id = bundle.id().to_string();
            Self::merge_named(
                agent_definitions,
                bundle.agent_definitions(),
                &bundle_id,
                BundleRegistryKind::Agent,
            )?;
            agent_registries.extend(bundle.agent_registries());

            Self::merge_named(
                tool_definitions,
                bundle.tool_definitions(),
                &bundle_id,
                BundleRegistryKind::Tool,
            )?;
            tool_registries.extend(bundle.tool_registries());

            Self::merge_named(
                behavior_definitions,
                bundle.behavior_definitions(),
                &bundle_id,
                BundleRegistryKind::Behavior,
            )?;
            behavior_registries.extend(bundle.behavior_registries());

            Self::merge_named(
                provider_definitions,
                bundle.provider_definitions(),
                &bundle_id,
                BundleRegistryKind::Provider,
            )?;
            provider_registries.extend(bundle.provider_registries());

            Self::merge_named(
                model_definitions,
                bundle.model_definitions(),
                &bundle_id,
                BundleRegistryKind::Model,
            )?;
            model_registries.extend(bundle.model_registries());
        }

        Ok(())
    }

    fn merge_named<V>(
        target: &mut HashMap<String, V>,
        incoming: HashMap<String, V>,
        bundle_id: &str,
        kind: BundleRegistryKind,
    ) -> Result<(), BundleComposeError> {
        for (id, value) in incoming {
            if id.trim().is_empty() {
                return Err(BundleComposeError::EmptyId {
                    bundle_id: bundle_id.to_string(),
                    kind,
                });
            }
            if target.contains_key(&id) {
                return Err(BundleComposeError::DuplicateId {
                    bundle_id: bundle_id.to_string(),
                    kind,
                    id,
                });
            }
            target.insert(id, value);
        }
        Ok(())
    }
}

/// Lightweight bundle carrying only tools/behaviors contributions.
///
/// This is useful for runtime/system wiring where agent/model/provider registries
/// should not be mutated, but tools/behaviors still need a uniform bundle path.
#[derive(Clone, Default)]
pub struct ToolBehaviorBundle {
    id: String,
    tools: HashMap<String, Arc<dyn Tool>>,
    behaviors: HashMap<String, Arc<dyn AgentBehavior>>,
}

impl ToolBehaviorBundle {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.descriptor().id, tool);
        self
    }

    pub fn with_tools(mut self, tools: HashMap<String, Arc<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn with_behavior(mut self, behavior: Arc<dyn AgentBehavior>) -> Self {
        self.behaviors.insert(behavior.id().to_string(), behavior);
        self
    }
}

impl RegistryBundle for ToolBehaviorBundle {
    fn id(&self) -> &str {
        &self.id
    }

    fn tool_definitions(&self) -> HashMap<String, Arc<dyn Tool>> {
        self.tools.clone()
    }

    fn behavior_definitions(&self) -> HashMap<String, Arc<dyn AgentBehavior>> {
        self.behaviors.clone()
    }
}
