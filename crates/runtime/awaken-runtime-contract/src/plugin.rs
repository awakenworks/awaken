//! Plugin mechanism: manifests, capability bounds, resolved contributions, and
//! the per-run execution environment.
//!
//! A `Plugin` is a factory that declares a `PluginManifest` (id, dependencies,
//! config sections, and a `CapabilityBound`) and resolves once into
//! `Contributions`. The runtime merges every active plugin's contributions into a
//! `ResolvedExecutionEnv`, enforcing that each plugin's actual contributions are a
//! subset of its declared bound (G30, fail-closed) and that ids are unique and
//! dependency-ordered. Hooks emit state through the commit path; they never write
//! a store or bypass permission (G9).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{
    Command as StateCommand, MergePolicy, Scope, StateKey, Store,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::permission::ToolGateHook;
use crate::resolved::ToolDescriptor;
use crate::tool::{RawTool, ToolCall, ToolOutput};

/// The phases a hook can observe in one model/tool step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhaseHookPoint {
    StepStart,
    BeforeInference,
    AfterInference,
    /// After one tool call produced its output (fires once per executed call, and
    /// again when an approved pending call is replayed on resume). The executed
    /// call and its output are carried on [`PhaseContext::after_tool`]. Folds the
    /// former separate `ToolOutcomeHook` into the phase-hook model (ADR-0055).
    AfterTool,
    StepEnd,
}

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

/// The executed tool call and its output, carried by [`PhaseKind::AfterTool`] so a
/// phase hook can react to a tool result (the data the former `ToolOutcomeHook`
/// received directly).
#[derive(Debug, Clone, PartialEq)]
pub struct AfterToolContext {
    pub call: ToolCall,
    pub output: ToolOutput,
}

/// Which phase a hook is being invoked at, carrying exactly the data valid at that
/// phase. Only [`PhaseKind::AfterTool`] carries a call/output, so a StepStart hook
/// cannot be handed a tool result — the illegal combination is unrepresentable
/// (ADR-0055), replacing the former `point` + `Option<after_tool>` pair.
#[derive(Debug, Clone, PartialEq)]
pub enum PhaseKind {
    StepStart,
    BeforeInference,
    AfterInference,
    AfterTool(AfterToolContext),
    StepEnd,
}

impl PhaseKind {
    /// The lightweight subscription discriminant for this phase (the axis a
    /// `CapabilityBound` and `hooks_for` key on).
    #[must_use]
    pub fn point(&self) -> PhaseHookPoint {
        match self {
            PhaseKind::StepStart => PhaseHookPoint::StepStart,
            PhaseKind::BeforeInference => PhaseHookPoint::BeforeInference,
            PhaseKind::AfterInference => PhaseHookPoint::AfterInference,
            PhaseKind::AfterTool(_) => PhaseHookPoint::AfterTool,
            PhaseKind::StepEnd => PhaseHookPoint::StepEnd,
        }
    }
}

/// Context passed to a phase hook. Immutable data; a hook returns state commands
/// rather than mutating anything directly.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseContext {
    pub run_id: RunId,
    pub step: usize,
    pub kind: PhaseKind,
}

/// The run-scoped state a `BeforeInference` hook writes to inject **request-only**
/// context (recalled memories, a compaction summary): the kernel reads it at
/// request assembly and prepends the flattened blocks to that inference, never
/// committing them to the transcript (ADR-0055). Keyed by contributing plugin id
/// and `Commutative`-merged, so several producers coexist and each replays across
/// steps and a resumed run from committed state — request-only-ness is a property
/// of *this key*, not of an overloaded message field (resolving the former
/// dual-meaning `HookReaction.messages`).
pub struct ContextMessages;

impl StateKey for ContextMessages {
    const KEY: &'static str = "context_messages";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Commutative;
    type Value = BTreeMap<String, Vec<Message>>;
}

/// What a hook stages back into the loop: durable state commands plus committed
/// messages. `messages` are **committed** reminder messages an `AfterTool` hook
/// appends to the transcript (so they reach the next inference and replay
/// deterministically). Request-only context is *not* a message here — a
/// `BeforeInference` hook writes it to the [`ContextMessages`] state key, which the
/// kernel reads and prepends to the request.
#[derive(Debug, Default, Clone)]
pub struct HookReaction {
    pub state: Vec<StateCommand>,
    pub messages: Vec<Message>,
}

impl HookReaction {
    /// A reaction that only stages state (the common case).
    pub fn state(state: Vec<StateCommand>) -> Self {
        Self {
            state,
            messages: Vec::new(),
        }
    }

    /// A reaction that only appends committed reminder messages (an `AfterTool`
    /// hook's transcript contribution).
    pub fn messages(messages: Vec<Message>) -> Self {
        Self {
            state: Vec::new(),
            messages,
        }
    }
}

/// A phase hook: behavior contributed by a plugin at one phase point. Async so a
/// real hook can consult an external system (e.g. run a selection sub-agent). It
/// stages state commands; a `BeforeInference` hook injects request-only context
/// (e.g. recalled memories) by writing the [`ContextMessages`] state key, which
/// the kernel reads and prepends. `conversation` is the transcript at this point,
/// so a `BeforeInference` hook can select context relevant to the user's message.
/// `state` is the run's read-only materialized state, so a hook whose work is
/// once-per-run (recall, compaction) gates on its own run-scoped state key and
/// replays across steps and resume instead of caching by `run_id` (ADR-0055).
#[async_trait]
pub trait PhaseHook: Send + Sync {
    fn point(&self) -> PhaseHookPoint;
    async fn on_phase(
        &self,
        ctx: &PhaseContext,
        conversation: &[Message],
        state: &Store,
    ) -> HookReaction;
}

/// What a run-end guard sees when the model/tool loop reaches a natural end (a
/// text-only turn). Immutable: a guard reads the conversation and the run-scoped
/// forced-continuation count, then returns a decision.
pub struct RunEndContext<'a> {
    pub run_id: RunId,
    /// The full conversation transcript at the natural-end boundary.
    pub conversation: &'a [Message],
    /// How many times a guard has already steered this run — the runtime's
    /// run-scoped continuation counter. A guard reads it to enforce its own
    /// iteration budget; the runtime also caps total steps as a runaway backstop.
    pub forced_continuations: usize,
    /// The run's cancellation token, if any. A guard that grades through a judge
    /// sub-run forwards it, so cancelling the parent cancels the judge too rather
    /// than orphaning it.
    pub cancellation: Option<&'a tokio_util::sync::CancellationToken>,
    /// The run's read-only materialized state, so a continuation predicate can
    /// inspect accumulated state (e.g. whether a machine instance is terminal).
    pub state: &'a Store,
}

/// A run-end guard's decision at a natural-end boundary. The runtime owns *when*
/// the loop stops; the guard supplies the *predicate* and any feedback. `detail`
/// is opaque to the runtime (anti-corruption): the guard's own classification,
/// forwarded to the host without the kernel interpreting it.
pub enum RunEndDecision {
    /// End the run. `detail` is surfaced to the host as an opaque round result.
    Complete { detail: serde_json::Value },
    /// Continue for another turn: append `feedback` as a user message and loop.
    /// `detail` describes this non-terminal round, opaque to the runtime.
    Steer {
        feedback: String,
        detail: serde_json::Value,
    },
}

/// A run-end continuation guard: consulted at the natural-end boundary to decide
/// whether the run ends or takes another steered turn (e.g. goal/outcome
/// evaluation). The runtime consults registered guards in dependency order and
/// takes the first that steers; if none steer, the run ends carrying the last
/// guard's completion detail. Async so a guard can grade through an external
/// judge before deciding.
#[async_trait]
pub trait RunEndGuard: Send + Sync {
    /// Stable id, checked against the plugin's `CapabilityBound` (G30).
    fn id(&self) -> &str;
    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision;
}

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

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeHook(PhaseHookPoint);

    #[async_trait]
    impl PhaseHook for FakeHook {
        fn point(&self) -> PhaseHookPoint {
            self.0
        }
        async fn on_phase(
            &self,
            _ctx: &PhaseContext,
            _conversation: &[Message],
            _state: &Store,
        ) -> HookReaction {
            HookReaction::default()
        }
    }

    #[test]
    fn id_bound_admits_by_variant() {
        // Any admits everything.
        assert!(IdBound::Any.allows("anything"));
        // Exact admits only listed ids.
        let exact = IdBound::Exact(vec!["a".into(), "b".into()]);
        assert!(exact.allows("a"));
        assert!(!exact.allows("z"));
        // Namespace admits any id under the prefix.
        let ns = IdBound::Namespace("mcp__srv__".into());
        assert!(ns.allows("mcp__srv__echo"));
        assert!(!ns.allows("other__echo"));
        // NamespacedExact requires BOTH the prefix AND the explicit id — a stray id
        // under the prefix that was not discovered fails closed.
        let nse = IdBound::NamespacedExact {
            prefix: "mcp__srv__".into(),
            ids: vec!["mcp__srv__echo".into()],
        };
        assert!(nse.allows("mcp__srv__echo"));
        assert!(!nse.allows("mcp__srv__backdoor")); // prefix ok, not discovered
        assert!(!nse.allows("other__echo")); // discovered-shaped but wrong prefix
    }

    #[test]
    fn id_bound_default_is_deny_all() {
        // An unset axis admits nothing (fail-closed) — the former empty-Vec semantics.
        assert_eq!(IdBound::default(), IdBound::Exact(Vec::new()));
        assert!(!IdBound::default().allows("anything"));
    }

    fn manifest(id: &str, bound: CapabilityBound) -> PluginManifest {
        PluginManifest {
            id: id.to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound,
        }
    }

    #[test]
    fn enforce_bound_accepts_a_subset() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Exact(vec!["t".into()]),
                state_keys: IdBound::Exact(vec!["k".into()]),
                phase_hooks: vec![PhaseHookPoint::StepStart],
                action_kinds: IdBound::Exact(vec!["a".into()]),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.tools.push("t".into());
        c.state_keys.push("k".into());
        c.action_kinds.push("a".into());
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepStart)));
        assert!(enforce_bound(&m, &c).is_ok());
    }

    #[test]
    fn enforce_bound_rejects_out_of_bound_contributions() {
        let m = manifest("p", CapabilityBound::default());

        let mut tool = Contributions::new("p");
        tool.tools.push("t".into());
        assert!(matches!(
            enforce_bound(&m, &tool),
            Err(BoundViolation::Tool { .. })
        ));

        let mut key = Contributions::new("p");
        key.state_keys.push("k".into());
        assert!(matches!(
            enforce_bound(&m, &key),
            Err(BoundViolation::StateKey { .. })
        ));

        let mut hook = Contributions::new("p");
        hook.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepEnd)));
        assert!(matches!(
            enforce_bound(&m, &hook),
            Err(BoundViolation::Hook { .. })
        ));

        let mut kind = Contributions::new("p");
        kind.action_kinds.push("a".into());
        assert!(matches!(
            enforce_bound(&m, &kind),
            Err(BoundViolation::ActionKind { .. })
        ));
    }

    fn with_action_kind(id: &str, kind: &str) -> (PluginManifest, Contributions) {
        let m = manifest(
            id,
            CapabilityBound {
                action_kinds: IdBound::Exact(vec![kind.into()]),
                ..Default::default()
            },
        );
        let mut c = Contributions::new(id);
        c.action_kinds.push(kind.into());
        (m, c)
    }

    #[test]
    fn merge_collects_action_kinds_and_rejects_duplicates() {
        // A selected plugin's action kind is in the resolved env; an unselected
        // one's is absent (RS-SCH-005).
        let env =
            ResolvedExecutionEnv::merge(vec![with_action_kind("p", "remind")]).expect("merges");
        assert!(env.permits_action_kind("remind"));
        assert!(!env.permits_action_kind("not-contributed"));

        let dup = vec![with_action_kind("a", "k"), with_action_kind("b", "k")];
        assert!(matches!(
            ResolvedExecutionEnv::merge(dup),
            Err(MergeError::DuplicateActionKind { .. })
        ));
    }

    fn with_tool(id: &str, tool: &str) -> (PluginManifest, Contributions) {
        let m = manifest(
            id,
            CapabilityBound {
                tools: IdBound::Exact(vec![tool.into()]),
                ..Default::default()
            },
        );
        let mut c = Contributions::new(id);
        c.tools.push(tool.into());
        (m, c)
    }

    #[test]
    fn merge_rejects_duplicate_tool_ids() {
        let plugins = vec![with_tool("a", "dup"), with_tool("b", "dup")];
        assert!(matches!(
            ResolvedExecutionEnv::merge(plugins),
            Err(MergeError::DuplicateTool { .. })
        ));
    }

    #[test]
    fn merge_rejects_missing_dependency() {
        let mut m = manifest("a", CapabilityBound::default());
        m.requires.push("missing".into());
        let plugins = vec![(m, Contributions::new("a"))];
        assert!(matches!(
            ResolvedExecutionEnv::merge(plugins),
            Err(MergeError::MissingDependency { .. })
        ));
    }

    #[test]
    fn merge_rejects_a_dependency_cycle() {
        let mut a = manifest("a", CapabilityBound::default());
        a.requires.push("b".into());
        let mut b = manifest("b", CapabilityBound::default());
        b.requires.push("a".into());
        let plugins = vec![(a, Contributions::new("a")), (b, Contributions::new("b"))];
        assert_eq!(
            ResolvedExecutionEnv::merge(plugins).err(),
            Some(MergeError::DependencyCycle)
        );
    }

    #[test]
    fn merge_orders_dependencies_first() {
        let mut b = manifest("b", CapabilityBound::default());
        b.requires.push("a".into());
        let a = manifest("a", CapabilityBound::default());
        // Input order is [b, a]; a must come first because b requires it.
        let plugins = vec![(b, Contributions::new("b")), (a, Contributions::new("a"))];
        let env = ResolvedExecutionEnv::merge(plugins).expect("merges");
        assert_eq!(env.order, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn hooks_for_filters_by_point() {
        let m = manifest(
            "p",
            CapabilityBound {
                phase_hooks: vec![PhaseHookPoint::StepStart, PhaseHookPoint::StepEnd],
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepStart)));
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepEnd)));
        let env = ResolvedExecutionEnv::merge(vec![(m, c)]).expect("merges");
        assert_eq!(env.hooks_for(PhaseHookPoint::StepStart).len(), 1);
        assert_eq!(env.hooks_for(PhaseHookPoint::StepEnd).len(), 1);
        assert_eq!(env.hooks_for(PhaseHookPoint::BeforeInference).len(), 0);
    }

    struct FakeRawTool(&'static str);

    #[async_trait]
    impl crate::tool::RawTool for FakeRawTool {
        fn id(&self) -> &str {
            self.0
        }
        async fn invoke(
            &self,
            call: crate::tool::ToolCall,
        ) -> Result<crate::tool::ToolOutput, crate::tool::ToolError> {
            Ok(crate::tool::ToolOutput::ok(call.call_id, "ok"))
        }
    }

    fn dynamic_tool(id: &'static str) -> DynamicTool {
        DynamicTool {
            descriptor: crate::resolved::ToolDescriptor::pinned(
                "mcp",
                id,
                "a dynamic tool",
                serde_json::json!({ "type": "object" }),
            ),
            tool: Arc::new(FakeRawTool(id)),
        }
    }

    #[test]
    fn enforce_bound_accepts_a_dynamic_tool_within_its_namespace() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Namespace("mcp__srv__".into()),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.dynamic_tools.push(dynamic_tool("mcp__srv__echo"));
        assert!(enforce_bound(&m, &c).is_ok());
    }

    #[test]
    fn enforce_bound_rejects_a_dynamic_tool_outside_its_namespace() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Namespace("mcp__srv__".into()),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.dynamic_tools.push(dynamic_tool("other__x"));
        assert!(matches!(
            enforce_bound(&m, &c),
            Err(BoundViolation::Tool { .. })
        ));
    }

    #[test]
    fn merge_exposes_dynamic_descriptors_and_lookup() {
        let m = manifest(
            "p",
            CapabilityBound {
                tools: IdBound::Namespace("mcp__srv__".into()),
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.dynamic_tools.push(dynamic_tool("mcp__srv__echo"));
        let env = ResolvedExecutionEnv::merge(vec![(m, c)]).expect("merges");
        assert_eq!(env.dynamic_descriptors().len(), 1);
        assert_eq!(env.dynamic_descriptors()[0].id, "mcp__srv__echo");
        assert!(env.dynamic_tool("mcp__srv__echo").is_some());
        assert!(env.dynamic_tool("missing").is_none());
    }
}
