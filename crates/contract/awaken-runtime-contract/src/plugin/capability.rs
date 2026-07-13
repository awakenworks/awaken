//! Capability bounds: the upper limit of what a plugin may contribute, the
//! id-admission value object every axis shares, the plugin manifest that declares
//! them, and the fail-closed `enforce_bound` check (G30).

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::contributions::Contributions;
use super::phase::PhaseHookPoint;

/// The set of contribution ids one id-bearing axis of a [`CapabilityBound`]
/// admits (G30). This value object owns the single "is this id permitted?"
/// decision for every id axis — tools, state keys, action kinds, guards, gates,
/// observers — so [`enforce_bound`] checks each axis the same way, `bound.allows(id)`,
/// instead of open-coding a per-axis loop (and it folds the former separate
/// `tool_namespaces` axis into the `tools` bound). The default is the empty
/// allow-list, i.e. deny-all: an axis left unset admits nothing (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdBound {
    /// Any id is admitted. Use only where the axis is genuinely unbounded by design.
    Any,
    /// Only these exact ids.
    Exact(Vec<String>),
    /// Any id under this namespace prefix — the ceiling for a plugin whose tool
    /// ids are not known at composition time (e.g. an MCP server's live set).
    Namespace(String),
    /// A namespace prefix **plus** an explicit per-id allow-list: an id is admitted
    /// only if it both starts with `prefix` and appears in `ids`. Tightens a
    /// dynamically discovered family (MCP/skills tools) to exactly the ids its
    /// source resolved, so a stray id under the prefix still fails closed.
    NamespacedExact { prefix: String, ids: Vec<String> },
}

impl Default for IdBound {
    /// Deny-all — an unset axis admits nothing (fail-closed), matching the former
    /// empty-`Vec` semantics.
    fn default() -> Self {
        IdBound::Exact(Vec::new())
    }
}

impl IdBound {
    /// Whether `id` is admitted by this bound.
    #[must_use]
    pub fn allows(&self, id: &str) -> bool {
        match self {
            IdBound::Any => true,
            IdBound::Exact(ids) => ids.iter().any(|allowed| allowed == id),
            IdBound::Namespace(prefix) => id.starts_with(prefix.as_str()),
            IdBound::NamespacedExact { prefix, ids } => {
                id.starts_with(prefix.as_str()) && ids.iter().any(|allowed| allowed == id)
            }
        }
    }

    /// Whether this bound admits nothing — the deny-all `Exact([])` default. An
    /// operator overlay reads this to tell a plugin that reserves a high-privilege
    /// axis (e.g. a tool gate) from one that leaves it unused.
    #[must_use]
    pub fn is_deny_all(&self) -> bool {
        matches!(self, IdBound::Exact(ids) if ids.is_empty())
    }
}

/// The upper bound of what a plugin may contribute. Actual contributions must be
/// a subset of this (G30); anything outside is a fail-closed violation. Each
/// id-bearing axis is an [`IdBound`]; `phase_hooks` is an enum-membership axis
/// (which phase points, not ids), so it stays an explicit list.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilityBound {
    /// Tool ids this plugin may contribute — static (catalog) and dynamic
    /// (MCP/skills) tools alike. `Exact` pins a fixed set; `Namespace` /
    /// `NamespacedExact` admit a composition-time-unknown live set. (Folds the
    /// former separate `tool_ids` + `tool_namespaces` axes into one.)
    pub tools: IdBound,
    pub state_keys: IdBound,
    pub phase_hooks: Vec<PhaseHookPoint>,
    /// Scheduled-action kinds this plugin may contribute (ADR-0027).
    pub action_kinds: IdBound,
    /// Run-end continuation guard ids this plugin may contribute.
    pub run_end_guards: IdBound,
    /// Tool-gate ids this plugin may contribute (a pre-execution decision that
    /// can only restrict, never grant — permission stays the sole grant, G21).
    pub tool_gates: IdBound,
}

/// Declared plugin identity and bound. One `validate`-able home for config.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    /// Other plugin ids that must resolve before this one (ordering only).
    pub requires: Vec<String>,
    pub config_sections: Vec<String>,
    pub bound: CapabilityBound,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BoundViolation {
    #[error("plugin {plugin} contributes tool {id:?} outside its declared bound")]
    Tool { plugin: String, id: String },
    #[error("plugin {plugin} contributes state key {id:?} outside its declared bound")]
    StateKey { plugin: String, id: String },
    #[error("plugin {plugin} registers hook {point:?} outside its declared bound")]
    Hook {
        plugin: String,
        point: PhaseHookPoint,
    },
    #[error("plugin {plugin} contributes action kind {id:?} outside its declared bound")]
    ActionKind { plugin: String, id: String },
    #[error("plugin {plugin} contributes run-end guard {id:?} outside its declared bound")]
    RunEndGuard { plugin: String, id: String },
    #[error("plugin {plugin} contributes tool gate {id:?} outside its declared bound")]
    ToolGate { plugin: String, id: String },
}

/// Enforce that a plugin's actual contributions are a subset of its bound (G30).
pub fn enforce_bound(
    manifest: &PluginManifest,
    contributions: &Contributions,
) -> Result<(), BoundViolation> {
    let bound = &manifest.bound;
    let id = &manifest.id;

    // Static tools and dynamic (MCP/skills) tools share one `tools` bound: a
    // static tool is admitted by its id, a dynamic tool by its (namespaced) id.
    for tool in &contributions.tools {
        if !bound.tools.allows(tool) {
            return Err(BoundViolation::Tool {
                plugin: id.clone(),
                id: tool.clone(),
            });
        }
    }
    for dynamic in &contributions.dynamic_tools {
        let tool_id = dynamic.tool.id();
        if !bound.tools.allows(tool_id) {
            return Err(BoundViolation::Tool {
                plugin: id.clone(),
                id: tool_id.to_string(),
            });
        }
    }
    for key in &contributions.state_keys {
        if !bound.state_keys.allows(key) {
            return Err(BoundViolation::StateKey {
                plugin: id.clone(),
                id: key.clone(),
            });
        }
    }
    // Phase hooks are bounded by which phase points a plugin may hook (an enum
    // axis, not ids), so this stays a membership check.
    for hook in &contributions.phase_hooks {
        if !bound.phase_hooks.contains(&hook.point()) {
            return Err(BoundViolation::Hook {
                plugin: id.clone(),
                point: hook.point(),
            });
        }
    }
    for kind in &contributions.action_kinds {
        if !bound.action_kinds.allows(kind) {
            return Err(BoundViolation::ActionKind {
                plugin: id.clone(),
                id: kind.clone(),
            });
        }
    }
    for guard in &contributions.run_end_guards {
        if !bound.run_end_guards.allows(guard.id()) {
            return Err(BoundViolation::RunEndGuard {
                plugin: id.clone(),
                id: guard.id().to_string(),
            });
        }
    }
    for gate in &contributions.tool_gates {
        if !bound.tool_gates.allows(gate.id()) {
            return Err(BoundViolation::ToolGate {
                plugin: id.clone(),
                id: gate.id().to_string(),
            });
        }
    }
    Ok(())
}
