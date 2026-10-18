// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Installers of the tool, Skill, plugin, and gate offering registered on every thread;
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! TOOLS capability of [`ExecutorComponents`]: client tools, the Skill offering,
//! runtime plugins and their configuration, WebSearch providers, the gate
//! override, and management tools installed on every thread.

use crate::host::*;

/// Tool/Skill/plugin offering. Consumer: every thread's runtime assembly and
/// the adapters' capability projections.
pub(crate) struct ToolOffering {
    pub(crate) client_tools: HashSet<String>,
    /// The skill offering (ADR-0036): the static configured set, the optional durable
    /// `/v1/skills` catalog, and its sync-read cache — grouped behind one type that
    /// owns the cache↔store coherence invariant. See [`crate::skill_catalog`].
    pub(crate) skills: crate::skill_catalog::SkillCatalog,
    /// Runtime plugins this host activates on every thread, and their config
    /// sections (e.g. the tool state machine). Empty by default.
    pub(crate) plugin_ids: Vec<String>,
    pub(crate) plugin_config: std::collections::BTreeMap<String, serde_json::Value>,
    /// One provider registry is used to derive authoring schema and to dispatch
    /// Native/ACP WebSearch calls. External deployments extend this registry;
    /// sessions never construct a provider-specific side registry.
    pub(crate) web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    /// An optional tool gate that replaces the default authorization gate on every
    /// thread's runtime. Used to exercise scheduled actions (ADR-0020, slice E): a
    /// gate that defers tool calls as `ScheduledAction`s so the durable dispatch
    /// worker performs them out of band.
    pub(crate) gate_override: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    /// Globally-registered management tool executables (ADR-0052 D3/D4). Registered
    /// on every thread's runtime (the executor registry stays global); only the
    /// reserved-scope assistant's compiled config *names* them, so no other run can
    /// invoke them. Their ids are also pre-authorized on the gate (read-only tools).
    /// Empty by default.
    pub(crate) admin_tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
}

impl ToolOffering {
    /// The built-in WebSearch providers and `skills`; no client tools,
    /// plugins, gate override, or management tools yet.
    pub(in crate::host) fn new(skills: crate::skill_catalog::SkillCatalog) -> Self {
        Self {
            client_tools: HashSet::new(),
            skills,
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
            gate_override: None,
            admin_tools: Vec::new(),
        }
    }
}
