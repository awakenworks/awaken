//! The per-run execution environment: the merge that validates each plugin
//! against its bound, rejects duplicate ids, orders dependencies first, and
//! exposes the merged tool face / hooks / guards / gates to the kernel.

use std::sync::Arc;

use thiserror::Error;

use crate::permission::ToolGateHook;
use crate::resolved::ToolDescriptor;
use crate::tool::RawTool;

use super::capability::{BoundViolation, PluginManifest, enforce_bound};
use super::contributions::{Contributions, DynamicTool, PluginConfigError};
use super::guard::RunEndGuard;
use super::phase::{PhaseHook, PhaseHookPoint};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MergeError {
    #[error("bound violation: {0}")]
    Bound(#[from] BoundViolation),
    #[error("duplicate tool id {id:?} contributed by {first} and {second}")]
    DuplicateTool {
        id: String,
        first: String,
        second: String,
    },
    #[error("duplicate action kind {id:?} contributed by {first} and {second}")]
    DuplicateActionKind {
        id: String,
        first: String,
        second: String,
    },
    #[error("plugin {plugin} requires {missing:?}, which is not active")]
    MissingDependency { plugin: String, missing: String },
    #[error("dependency cycle among active plugins")]
    DependencyCycle,
    #[error(transparent)]
    Config(#[from] PluginConfigError),
}

/// The merged contributions of every active plugin for one run. Built by
/// validating each plugin against its bound, rejecting duplicate tool ids, and
/// ordering plugins so dependencies resolve first.
pub struct ResolvedExecutionEnv {
    pub order: Vec<String>,
    pub tools: Vec<String>,
    pub state_keys: Vec<String>,
    pub phase_hooks: Vec<Arc<dyn PhaseHook>>,
    /// Scheduled-action kinds the selected plugins contribute. A kind absent here
    /// (its plugin is not selected for the run) cannot be staged (ADR-0027).
    pub action_kinds: Vec<String>,
    /// Run-end continuation guards the selected plugins contribute, in dependency
    /// order. Consulted at the natural-end boundary.
    pub run_end_guards: Vec<Arc<dyn RunEndGuard>>,
    /// Pre-execution tool gates the selected plugins contribute, in dependency
    /// order. Consulted after the host gate; each can only restrict.
    pub tool_gates: Vec<Arc<dyn ToolGateHook>>,
    /// Dynamic tools (descriptor + executable) contributed by the selected
    /// plugins, in dependency order. Merged into the model-visible tool face and
    /// consulted for execution alongside the runtime's static tool registry.
    pub dynamic_tools: Vec<DynamicTool>,
}

impl ResolvedExecutionEnv {
    /// Merge active `(manifest, contributions)` pairs. Fails closed on a bound
    /// violation, duplicate tool id, missing dependency, or dependency cycle.
    pub fn merge(plugins: Vec<(PluginManifest, Contributions)>) -> Result<Self, MergeError> {
        for (manifest, contributions) in &plugins {
            enforce_bound(manifest, contributions)?;
        }

        let active: Vec<String> = plugins.iter().map(|(m, _)| m.id.clone()).collect();
        for (manifest, _) in &plugins {
            for required in &manifest.requires {
                if !active.contains(required) {
                    return Err(MergeError::MissingDependency {
                        plugin: manifest.id.clone(),
                        missing: required.clone(),
                    });
                }
            }
        }

        let order = topological_order(&plugins)?;

        // Re-emit contributions in dependency order.
        let mut tools: Vec<String> = Vec::new();
        let mut tool_owner: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut state_keys: Vec<String> = Vec::new();
        let mut phase_hooks: Vec<Arc<dyn PhaseHook>> = Vec::new();
        let mut run_end_guards: Vec<Arc<dyn RunEndGuard>> = Vec::new();
        let mut tool_gates: Vec<Arc<dyn ToolGateHook>> = Vec::new();
        let mut dynamic_tools: Vec<DynamicTool> = Vec::new();
        let mut action_kinds: Vec<String> = Vec::new();
        let mut action_owner: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();

        for id in &order {
            let (_, contributions) = plugins
                .iter()
                .find(|(m, _)| &m.id == id)
                .expect("ordered id is present");
            for tool in &contributions.tools {
                if let Some(first) = tool_owner.get(tool) {
                    return Err(MergeError::DuplicateTool {
                        id: tool.clone(),
                        first: first.clone(),
                        second: id.clone(),
                    });
                }
                tool_owner.insert(tool.clone(), id.clone());
                tools.push(tool.clone());
            }
            for dynamic in &contributions.dynamic_tools {
                let tool_id = dynamic.tool.id().to_string();
                if let Some(first) = tool_owner.get(&tool_id) {
                    return Err(MergeError::DuplicateTool {
                        id: tool_id,
                        first: first.clone(),
                        second: id.clone(),
                    });
                }
                tool_owner.insert(tool_id, id.clone());
                dynamic_tools.push(dynamic.clone());
            }
            for key in &contributions.state_keys {
                if !state_keys.contains(key) {
                    state_keys.push(key.clone());
                }
            }
            phase_hooks.extend(contributions.phase_hooks.iter().cloned());
            run_end_guards.extend(contributions.run_end_guards.iter().cloned());
            tool_gates.extend(contributions.tool_gates.iter().cloned());
            for kind in &contributions.action_kinds {
                if let Some(first) = action_owner.get(kind) {
                    return Err(MergeError::DuplicateActionKind {
                        id: kind.clone(),
                        first: first.clone(),
                        second: id.clone(),
                    });
                }
                action_owner.insert(kind.clone(), id.clone());
                action_kinds.push(kind.clone());
            }
        }

        Ok(Self {
            order,
            tools,
            state_keys,
            phase_hooks,
            action_kinds,
            run_end_guards,
            tool_gates,
            dynamic_tools,
        })
    }

    /// Whether a scheduled-action `kind` is contributed by a selected plugin — the
    /// fail-closed check before staging a kind-based scheduled action (ADR-0027).
    pub fn permits_action_kind(&self, kind: &str) -> bool {
        self.action_kinds.iter().any(|k| k == kind)
    }

    /// The descriptors of all contributed dynamic tools, in dependency order —
    /// merged into the model-visible tool face for a step.
    pub fn dynamic_descriptors(&self) -> Vec<ToolDescriptor> {
        self.dynamic_tools
            .iter()
            .map(|d| d.descriptor.clone())
            .collect()
    }

    /// Look up a dynamic tool's executable behavior by id.
    pub fn dynamic_tool(&self, id: &str) -> Option<Arc<dyn RawTool>> {
        self.dynamic_tools
            .iter()
            .find(|d| d.tool.id() == id)
            .map(|d| Arc::clone(&d.tool))
    }

    /// Hooks registered for one phase point, in dependency order.
    pub fn hooks_for(&self, point: PhaseHookPoint) -> Vec<Arc<dyn PhaseHook>> {
        self.phase_hooks
            .iter()
            .filter(|h| h.point() == point)
            .cloned()
            .collect()
    }

    /// Run-end continuation guards, in dependency order.
    pub fn run_end_guards(&self) -> &[Arc<dyn RunEndGuard>] {
        &self.run_end_guards
    }

    /// Plugin-contributed pre-execution tool gates, in dependency order.
    pub fn tool_gates(&self) -> &[Arc<dyn ToolGateHook>] {
        &self.tool_gates
    }
}

/// Order plugins so every `requires` dependency precedes the plugin (Kahn). The
/// input order breaks ties, keeping merges deterministic.
fn topological_order(
    plugins: &[(PluginManifest, Contributions)],
) -> Result<Vec<String>, MergeError> {
    let ids: Vec<String> = plugins.iter().map(|(m, _)| m.id.clone()).collect();
    let mut ordered: Vec<String> = Vec::new();

    while ordered.len() < ids.len() {
        let mut progressed = false;
        for (manifest, _) in plugins {
            if ordered.contains(&manifest.id) {
                continue;
            }
            let deps_ready = manifest.requires.iter().all(|dep| ordered.contains(dep));
            if deps_ready {
                ordered.push(manifest.id.clone());
                progressed = true;
            }
        }
        if !progressed {
            return Err(MergeError::DependencyCycle);
        }
    }
    Ok(ordered)
}
