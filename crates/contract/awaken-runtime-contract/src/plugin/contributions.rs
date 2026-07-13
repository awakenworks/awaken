//! What a plugin contributes: the `Plugin` factory trait, the `Contributions`
//! registrar it fills, the dynamically discovered tool, and the fail-closed
//! config error a malformed section raises.

use std::sync::Arc;

use thiserror::Error;

use crate::permission::ToolGateHook;
use crate::resolved::ToolDescriptor;
use crate::tool::RawTool;

use super::capability::PluginManifest;
use super::guard::RunEndGuard;
use super::phase::PhaseHook;

/// A tool contributed with its executable behavior and descriptor. Used by
/// plugins whose tool set is dynamic (e.g. an MCP server's live tools), where
/// the id is not known at composition time and so cannot be pre-registered on
/// the runtime like a static built-in tool.
#[derive(Clone)]
pub struct DynamicTool {
    pub descriptor: ToolDescriptor,
    pub tool: Arc<dyn RawTool>,
}

/// One plugin's resolved contributions. Built once by `Plugin::resolve`; holds
/// live hook behavior, so it is runtime-side wiring, not serialized truth.
#[derive(Clone)]
pub struct Contributions {
    pub plugin_id: String,
    pub tools: Vec<String>,
    pub state_keys: Vec<String>,
    pub phase_hooks: Vec<Arc<dyn PhaseHook>>,
    /// Scheduled-action kinds this plugin contributes (ADR-0027).
    pub action_kinds: Vec<String>,
    /// Run-end continuation guards this plugin contributes.
    pub run_end_guards: Vec<Arc<dyn RunEndGuard>>,
    /// Pre-execution tool gates this plugin contributes.
    pub tool_gates: Vec<Arc<dyn ToolGateHook>>,
    /// Tools contributed with their executable behavior, for dynamic tool sets.
    pub dynamic_tools: Vec<DynamicTool>,
}

impl Contributions {
    pub fn new(plugin_id: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            tools: Vec::new(),
            state_keys: Vec::new(),
            phase_hooks: Vec::new(),
            action_kinds: Vec::new(),
            run_end_guards: Vec::new(),
            tool_gates: Vec::new(),
            dynamic_tools: Vec::new(),
        }
    }

    // A small registrar over the contribution axes (ADR-0055): a plugin declares
    // what it contributes through these chainable methods rather than reaching
    // into each `Vec`, so the seam a plugin uses is one method per axis and the
    // fields stay the merge/enforce_bound reading surface. Each declared state key
    // must sit within the plugin's `CapabilityBound` (G30).

    /// Declare a state key this plugin writes (must be within its bound, G30).
    pub fn declare_state_key(&mut self, key: impl Into<String>) -> &mut Self {
        self.state_keys.push(key.into());
        self
    }

    /// Register a phase hook (any point, including `AfterTool`).
    pub fn register_hook(&mut self, hook: Arc<dyn PhaseHook>) -> &mut Self {
        self.phase_hooks.push(hook);
        self
    }

    /// Register a pre-execution tool gate.
    pub fn register_gate(&mut self, gate: Arc<dyn ToolGateHook>) -> &mut Self {
        self.tool_gates.push(gate);
        self
    }

    /// Register a run-end continuation guard.
    pub fn register_guard(&mut self, guard: Arc<dyn RunEndGuard>) -> &mut Self {
        self.run_end_guards.push(guard);
        self
    }

    /// Register a dynamically discovered tool (descriptor + executable).
    pub fn register_dynamic_tool(&mut self, tool: DynamicTool) -> &mut Self {
        self.dynamic_tools.push(tool);
        self
    }
}

/// A plugin factory. `resolve` is called once per run to compile contributions
/// from config; it must not perform mutable registration side effects.
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> PluginManifest;

    /// Config-agnostic contributions — the plugin's default behavior.
    fn resolve(&self) -> Contributions;

    /// Config-aware resolve. The runtime hands the plugin its own configuration
    /// section (the raw JSON at `ResolvedSpec.plugin_config[manifest.id]`), or
    /// `None` when the agent set none. The default ignores config and returns the
    /// config-agnostic `resolve`; a configurable plugin overrides this, decodes
    /// its section, and fails closed (`Err`) on a malformed one. Publish-time
    /// validation is a dry run of this same method — the validator is the applier.
    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let _ = config;
        Ok(self.resolve())
    }

    /// A monotonically advancing version for a plugin whose contributions can
    /// change during a run (e.g. an MCP server firing `tools/list_changed`).
    /// The runtime re-resolves the plugin at a safe step boundary when this
    /// advances. `None` (the default) means the plugin is static.
    fn live_version(&self) -> Option<u64> {
        None
    }
}

/// A plugin's configuration section could not be applied. Carried as a fail-closed
/// error: a run whose plugin config is malformed does not start (G30).
#[derive(Debug, Error, PartialEq, Eq)]
#[error("malformed config for plugin {plugin}: {message}")]
pub struct PluginConfigError {
    pub plugin: String,
    pub message: String,
}

impl PluginConfigError {
    pub fn new(plugin: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            message: message.into(),
        }
    }
}
