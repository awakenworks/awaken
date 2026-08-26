//! Assembling a runtime and its config, and reading a run's output.
//!
//! The "how to build a run" concern, kept out of the session substrate: the
//! permission policy, the advertised tool descriptors, the `ExecutableAgentSnapshot`, and
//! the per-thread `Runtime`. The session substrate (`host`) and the sub-agent
//! helper (`agent_runner`) both depend on this leaf rather than each other.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_ext_builtin_tools::{
    AGENT_RUN, Toolset, WebFetchPlugin, WebSearchPlugin, WebSearchProviderRegistry, all_hand_tools,
    builtin_tools,
};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RuleBasedToolPermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_ext_state_machine::{STATE_MACHINE_PLUGIN_ID, StateMachinePlugin};
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::capability::PluginCapability;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::permission::{ToolGateHook, ToolPermissionPolicy};
use awaken_runtime_contract::plugin::Plugin;
use awaken_runtime_contract::resolved::{ContextPolicy, ModelBinding, ToolDescriptor};
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_runtime_contract::tool::{RawTool, RawToolRegistry, ToolExecutor};
use awaken_sandbox_local::LocalSandbox;

const SYSTEM_PROMPT: &str = "You are a helpful assistant working in a local repository.";

/// Return the contract's canonical recursive plain-text view of content blocks.
pub fn block_text(content: &[ContentBlock]) -> String {
    extract_text(content)
}

/// The text of the last assistant message in a transcript.
pub(crate) fn latest_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default()
}

/// The built-in hand tools auto-allowed without a confirmation prompt (ADR-0030):
/// read/glob/grep are perception; mutations (bash/write/edit) are asked. Single source
/// for both the permission policy and a managed session's advertised confirmation
/// policy, so the gate and the advertisement cannot drift.
const AUTO_ALLOWED_HAND_TOOLS: [&str; 3] = ["read", "glob", "grep"];

/// The built-in baseline allow rules (ADR-0030): read/glob/grep perception,
/// delegation and skill discovery, plus a thread's pre-authorized MCP tool ids
/// (ADR-0043 Phase 3 — configuring the server, with its credential, was the
/// authorization decision). Factored out so both the default policy and an
/// authored policy share the exact same baseline.
fn base_allow_rules(extra_allowed: &[String]) -> Vec<PermissionRule> {
    let allow = |name: &str| {
        PermissionRule::new(
            ToolCallPattern::parse(name).expect("static pattern"),
            ToolPermissionBehavior::Allow,
        )
    };
    let mut rules: Vec<PermissionRule> = AUTO_ALLOWED_HAND_TOOLS.iter().map(|n| allow(n)).collect();
    // `agent_run` is allowed: the kernel executes it via the injected delegation
    // resolver (a sub-agent, native or remote), not the tool registry — delegation is
    // a runtime concern. Skill discovery/activation grant perception (they list
    // metadata and return instructions), not authorization; allow them without a
    // confirmation prompt. Any tool a skill then invokes is still gated.
    rules.push(allow(AGENT_RUN));
    rules.push(allow(awaken_runtime_contract::resolved::ADVISOR_TOOL_ID));
    rules.push(allow("list_skills"));
    rules.push(allow("Skill"));
    // Semantic Memory perception mirrors read/glob. Mutations intentionally
    // retain the default confirmation behavior, matching write/edit.
    rules.push(allow("list_memories"));
    rules.push(allow("read_memory"));
    rules.extend(extra_allowed.iter().map(|id| allow(id)));
    rules
}

fn server_policy(extra_allowed: &[String]) -> RuleBasedToolPermissionPolicy {
    RuleBasedToolPermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::RequireConfirmation,
        mode: Mode::Default,
        rules: base_allow_rules(extra_allowed),
    })
}

/// The `plugin_config` key an agent's authored permission policy lives under.
pub(crate) const PERMISSION_CONFIG_KEY: &str = "permission";

/// Parse an agent's authored permission policy from its `plugin_config`, if present.
/// A missing or malformed section returns `None`, so the caller keeps the strict
/// built-in default — a bad policy is never silently reinterpreted into fail-open.
pub(crate) fn config_permission_ruleset(
    plugin_config: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Option<PermissionRuleset> {
    let raw = plugin_config.get(PERMISSION_CONFIG_KEY)?;
    awaken_ext_permission::parse_ruleset(raw).ok()
}

/// The ruleset a thread actually enforces: the built-in baseline (auto-allowed
/// perception tools + pre-authorized MCP ids) with an authored policy's rules
/// layered on top and its `default_behavior`/`mode` governing unmatched calls
/// (`deny` in any rule still wins, absolutely). With no authored policy this is the
/// strict built-in default (perception allowed, mutations asked).
pub(crate) fn effective_ruleset(
    authored: Option<PermissionRuleset>,
    extra_allowed: &[String],
) -> PermissionRuleset {
    let mut rules = base_allow_rules(extra_allowed);
    let (default_behavior, mode) = authored
        .as_ref()
        .map(|rs| (rs.default_behavior, rs.mode))
        .unwrap_or((ToolPermissionBehavior::RequireConfirmation, Mode::Default));
    if let Some(rs) = authored {
        rules.extend(rs.rules);
    }
    PermissionRuleset {
        default_behavior,
        mode,
        rules,
    }
}

fn toolset_permission_rules(
    toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
) -> Vec<PermissionRule> {
    use awaken_runtime_contract::agent_bindings::{ToolPermissionRequirement, ToolsetSource};
    let behavior = |enabled: bool, permission: ToolPermissionRequirement| {
        if !enabled {
            ToolPermissionBehavior::Deny
        } else {
            match permission {
                ToolPermissionRequirement::AlwaysAllow => ToolPermissionBehavior::Allow,
                ToolPermissionRequirement::AlwaysAsk => ToolPermissionBehavior::RequireConfirmation,
            }
        }
    };
    let rule =
        |pattern: String, policy: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy| {
            PermissionRule::new(
                ToolCallPattern::parse(&pattern).expect("typed toolset produces a valid pattern"),
                behavior(policy.enabled, policy.permission),
            )
        };
    let mut rules = Vec::new();
    for toolset in toolsets {
        match &toolset.source {
            ToolsetSource::Agent => rules.extend(
                toolset
                    .overrides
                    .iter()
                    .map(|entry| rule(entry.name.clone(), entry.policy)),
            ),
            ToolsetSource::Mcp { server_name } => {
                rules.push(rule(format!("mcp__{server_name}__*"), toolset.default));
                rules.extend(toolset.overrides.iter().map(|entry| {
                    rule(format!("mcp__{server_name}__{}", entry.name), entry.policy)
                }));
            }
        }
    }
    rules
}

pub(crate) fn effective_ruleset_with_toolsets(
    authored: Option<PermissionRuleset>,
    extra_allowed: &[String],
    toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
) -> PermissionRuleset {
    let mut resolved = effective_ruleset(authored, extra_allowed);
    // Put exact toolset rules first so they are authoritative over the built-in
    // baseline on equal specificity. Compile rejects a second authored permission
    // section when toolsets exist.
    let mut rules = toolset_permission_rules(toolsets);
    rules.extend(resolved.rules);
    resolved.rules = rules;
    resolved
}

/// One compiled authorization value consumed by Native and ACP execution.
/// Agent publication, Session-local replacement, and legacy plugin policy all
/// converge here; callers cannot independently rebuild a gate and ACP policy.
#[derive(Clone)]
pub(crate) struct EffectiveToolAuthorization {
    pub(crate) gate: Arc<dyn ToolGateHook>,
    pub(crate) policy: Arc<dyn ToolPermissionPolicy>,
    explicit: bool,
}

pub(crate) fn effective_tool_authorization(
    configuration: &awaken_runtime_contract::agent_bindings::ResolvedConfiguration,
    extra_allowed: &[String],
    toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
) -> EffectiveToolAuthorization {
    let authored = config_permission_ruleset(configuration.plugins());
    let explicit = authored.is_some() || !toolsets.is_empty() || !extra_allowed.is_empty();
    let policy: Arc<dyn ToolPermissionPolicy> = Arc::new(RuleBasedToolPermissionPolicy::new(
        effective_ruleset_with_toolsets(authored, extra_allowed, toolsets),
    ));
    EffectiveToolAuthorization {
        gate: Arc::new(PermissionGate::new(policy.clone())),
        policy,
        explicit,
    }
}

fn hand_tool_descriptors() -> Vec<ToolDescriptor> {
    let registered: HashSet<String> = all_hand_tools()
        .iter()
        .map(|t| t.id().to_string())
        .collect();
    builtin_tools()
        .into_iter()
        .filter(|tool| {
            tool.toolset() == Toolset::Hand && registered.contains(&tool.descriptor().id)
        })
        .map(|tool| tool.into_descriptor())
        .collect()
}

/// The registered built-in hand tools advertised on a managed session: each id and
/// whether its calls require confirmation (`true` = not in the auto-allow set). Shares
/// [`AUTO_ALLOWED_HAND_TOOLS`] with [`server_policy`], so the gate and the
/// advertisement stay in lockstep.
pub(crate) fn builtin_hand_tools() -> Vec<(String, bool)> {
    hand_tool_descriptors()
        .into_iter()
        .map(|d| {
            let ask = !AUTO_ALLOWED_HAND_TOOLS.contains(&d.id.as_str());
            (d.id, ask)
        })
        .collect()
}

/// A client-executed tool descriptor: model-visible, but no `RawTool` is
/// registered, so a call awaits (gate `ask`) and the *client* supplies the result.
pub(crate) fn client_tool_descriptor(id: &str) -> ToolDescriptor {
    ToolDescriptor::client_executed(
        id,
        format!("Client-executed tool `{id}`; the caller runs it and returns the result."),
        serde_json::json!({ "type": "object" }),
    )
}

/// Preserve the exact Session-owned client tool contract while lowering it to
/// the Runtime's canonical execution-ownership descriptor.
pub(crate) fn session_client_tool_descriptor(
    tool: &awaken_agent_contract::ClientToolDescriptor,
) -> ToolDescriptor {
    ToolDescriptor::client_executed(
        tool.name.clone(),
        tool.description.clone(),
        tool.input_schema.clone(),
    )
}

/// Apply the complete Session-owned tool replacement to one attempt-local
/// snapshot. The retained Agent publication stays immutable; direct, durable,
/// and recovered attempts all consume this same projection.
pub(crate) fn project_session_tools(
    snapshot: &mut ExecutableAgentSnapshot,
    tools: &awaken_session_contract::SessionToolConfiguration,
) -> Result<(), serde_json::Error> {
    let projected = tools
        .client_tools
        .iter()
        .map(session_client_tool_descriptor)
        .collect::<Vec<_>>();
    let current = snapshot
        .resolved_spec
        .tool_descriptors
        .iter()
        .filter(|descriptor| {
            descriptor.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted
        })
        .cloned()
        .collect::<Vec<_>>();
    if snapshot.resolved_spec.plugin_config.agent.toolsets == tools.toolsets && current == projected
    {
        return Ok(());
    }
    snapshot.resolved_spec.plugin_config.agent.toolsets = tools.toolsets.clone();
    snapshot
        .resolved_spec
        .tool_descriptors
        .retain(|descriptor| {
            descriptor.kind != awaken_runtime_contract::resolved::ToolKind::ClientExecuted
        });
    snapshot.resolved_spec.tool_descriptors.extend(projected);
    snapshot.recompute_fingerprint()?;
    Ok(())
}

/// The `agent_run` delegation descriptor (advertised only when a roster is set).
pub(crate) fn delegation_descriptor() -> ToolDescriptor {
    builtin_tools()
        .into_iter()
        .find(|tool| tool.toolset() == Toolset::Delegation)
        .map(|tool| tool.into_descriptor())
        .expect("agent_run descriptor exists")
}

/// The advertised tool descriptors for a thread: hand tools, client-executed tools,
/// the dynamically provisioned per-thread tools (the skill tools of ADR-0036 and a
/// session's connected MCP tools, ADR-0043 Phase 3 — renamed from
/// `skill_descriptors` when MCP joined the flow), and `agent_run` when a delegate
/// roster is set. The single source for both the run config and a managed
/// session's advertised capability surface, so the two never drift.
pub fn advertised_tools(
    client_tools: &HashSet<String>,
    delegates: &HashSet<String>,
    dynamic_descriptors: &[ToolDescriptor],
) -> Vec<ToolDescriptor> {
    let mut tools = hand_tool_descriptors();
    tools.extend(client_tools.iter().map(|id| client_tool_descriptor(id)));
    // The `Skill` / `list_skills` descriptors (ADR-0036) and the MCP tool
    // descriptors: catalog-free, and the runtime registers the matching RawTools.
    tools.extend(dynamic_descriptors.iter().cloned());
    if !delegates.is_empty() {
        tools.push(delegation_descriptor());
    }
    tools
}

/// Complete builtin catalog available to Agent publication. Unlike one
/// Session's advertised face, this includes delegation even before targets are
/// known; the config compiler selects or hides it from typed `MultiagentConfig`.
pub fn authorable_tools() -> Vec<ToolDescriptor> {
    let mut tools = hand_tool_descriptors();
    // Both web capabilities are owned exclusively by their configured plugins.
    tools.push(awaken_ext_builtin_tools::web_fetch_descriptor());
    tools.push(awaken_ext_builtin_tools::web_search_descriptor());
    tools.push(delegation_descriptor());
    tools
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn server_config(
    agent_id: &str,
    model_ref: &str,
    client_tools: &HashSet<String>,
    delegates: &HashSet<String>,
    plugin_ids: &[String],
    plugin_config: &std::collections::BTreeMap<String, serde_json::Value>,
    dynamic_descriptors: &[ToolDescriptor],
    context_policy: ContextPolicy,
) -> ExecutableAgentSnapshot {
    let tools = advertised_tools(client_tools, delegates, dynamic_descriptors);
    ExecutableAgentSnapshot::builder(agent_id)
        .instructions(SYSTEM_PROMPT)
        .model(ModelBinding::new("default", model_ref, "default"))
        .model_candidates(Vec::new())
        .tools(tools)
        .max_steps(20)
        .plugins(plugin_ids.iter().cloned())
        .plugin_config(plugin_config.iter().map(|(k, v)| (k.clone(), v.clone())))
        .agent_bindings(awaken_runtime_contract::agent_bindings::AgentBindings {
            delegates: delegates
                .iter()
                .cloned()
                .map(
                    |id| awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                        agent_id: awaken_runtime_contract::snapshot::AgentId(id),
                        source_revision: None,
                        recursive_self: false,
                    },
                )
                .collect(),
            ..Default::default()
        })
        .context_policy(context_policy)
        .build()
}

/// The plugins this server configures, advertised with their config schema so a
/// config frontend can discover and author each section. One place declares a
/// plugin's id and its schema, so registration and discovery cannot drift.
pub fn platform_plugin_capabilities() -> Vec<PluginCapability> {
    platform_plugin_capabilities_with_web_search(&WebSearchProviderRegistry::builtins())
}

/// Capability projection for an externally extended WebSearch registry. The
/// same descriptors used for dispatch derive this schema, so an embedding
/// startup can advertise custom providers without modifying core enums.
pub fn platform_plugin_capabilities_with_web_search(
    providers: &WebSearchProviderRegistry,
) -> Vec<PluginCapability> {
    let web_search = WebSearchPlugin::new(providers.clone(), None);
    let manifest = web_search.manifest();
    let web_fetch = WebFetchPlugin::new(providers.clone(), None);
    let fetch_manifest = web_fetch.manifest();
    vec![
        PluginCapability {
            id: awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string(),
            schema_keys: vec![awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string()],
            config_schema: Some(awaken_ext_background_task::config_schema()),
            bound: awaken_ext_background_task::BackgroundTaskPlugin::new(Default::default())
                .manifest()
                .bound,
        },
        PluginCapability {
            id: STATE_MACHINE_PLUGIN_ID.to_string(),
            schema_keys: vec![STATE_MACHINE_PLUGIN_ID.to_string()],
            config_schema: Some(awaken_ext_state_machine::config_schema()),
            bound: Default::default(),
        },
        PluginCapability {
            id: awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            schema_keys: vec![awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
            config_schema: Some(awaken_ext_memory::memory_config_schema()),
            bound: Default::default(),
        },
        PluginCapability {
            id: awaken_ext_compact::COMPACT_PLUGIN_ID.to_string(),
            schema_keys: vec![awaken_ext_compact::COMPACT_PLUGIN_ID.to_string()],
            config_schema: Some(awaken_ext_compact::compact_config_schema()),
            bound: Default::default(),
        },
        PluginCapability {
            id: manifest.id,
            schema_keys: manifest.config_sections,
            config_schema: Some(providers.config_schema()),
            bound: manifest.bound,
        },
        PluginCapability {
            id: fetch_manifest.id,
            schema_keys: fetch_manifest.config_sections,
            config_schema: Some(providers.fetch_config_schema()),
            bound: fetch_manifest.bound,
        },
    ]
}

/// Every `plugin_config` section an author may set, WITH its JSON Schema: the
/// installable plugins PLUS the always-on `permission` policy. Permission is a
/// `plugin_config` section (not an installable plugin), so it is absent from
/// [`platform_plugin_capabilities`]; the assistant needs it advertised — with its
/// schema — or it cannot author a permission gate (it does not know the key/shape).
pub fn authorable_config_sections() -> Vec<PluginCapability> {
    authorable_config_sections_with_web_search(&WebSearchProviderRegistry::builtins())
}

pub fn authorable_config_sections_with_web_search(
    providers: &WebSearchProviderRegistry,
) -> Vec<PluginCapability> {
    let mut sections = platform_plugin_capabilities_with_web_search(providers);
    sections.push(PluginCapability {
        id: PERMISSION_CONFIG_KEY.to_string(),
        schema_keys: vec![PERMISSION_CONFIG_KEY.to_string()],
        config_schema: Some(awaken_ext_permission::permission_config_schema()),
        bound: Default::default(),
    });
    sections
}

/// The server's base authorization gate (the declarative permission policy). A
/// process-startup helper so a caller can wrap it (e.g. to observe file paths for
/// conditional skills) and re-inject it.
pub(crate) fn server_gate() -> Arc<dyn awaken_runtime_contract::permission::ToolGateHook> {
    server_gate_allowing(&[])
}

/// The base gate with extra pre-authorized tool ids (a thread's connected MCP
/// tools, ADR-0043 Phase 3). With an empty slice this IS `server_gate()`.
pub(crate) fn server_gate_allowing(
    extra_allowed: &[String],
) -> Arc<dyn awaken_runtime_contract::permission::ToolGateHook> {
    Arc::new(PermissionGate::new(Arc::new(server_policy(extra_allowed))))
}

/// A per-thread runtime whose hand tools come from `env` (placement-agnostic). No
/// `agent_run` executor is registered: a delegate call is advertised by the config
/// but the kernel runs it via the injected resolver, not the tool registry.
pub(crate) trait RuntimeToolSource {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>;
}

pub(crate) struct DeferredHandToolSource;

impl RuntimeToolSource for DeferredHandToolSource {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>> {
        all_hand_tools()
    }
}

/// The only Hand route installed for a filesystem-free Session. It contains the
/// Session-scoped semantic tools and rejects every Sandbox target; host-owned web
/// plugins remain Brain tools and therefore never enter this executor.
pub(crate) struct FilesystemFreeAgentToolExecutor {
    tools: RawToolRegistry,
}

impl FilesystemFreeAgentToolExecutor {
    pub(crate) fn try_new(
        session_tools: impl IntoIterator<Item = Arc<dyn RawTool>>,
    ) -> Result<Self, String> {
        let mut tools = RawToolRegistry::default();
        for tool in session_tools {
            let id = tool.id().to_string();
            if !tools.insert(tool) {
                return Err(format!(
                    "filesystem-free Session tool id `{id}` is duplicated"
                ));
            }
        }
        Ok(Self { tools })
    }
}

#[async_trait::async_trait]
impl ToolExecutor for FilesystemFreeAgentToolExecutor {
    async fn invoke(
        &self,
        call: &awaken_runtime_contract::tool::ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
        if self.tools.get(&call.tool_id).is_none() {
            return Err(awaken_runtime_contract::tool::ToolError::Execution(
                format!("filesystem-free Session cannot execute `{}`", call.tool_id),
            ));
        }
        self.tools.invoke(call).await
    }
}

impl RuntimeToolSource for LocalSandbox {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>> {
        self.rooted_tools()
    }
}

impl RuntimeToolSource for crate::session_environment::SessionEnvironment {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>> {
        self.rooted_tools()
    }
}

pub(crate) fn build_runtime<S: RuntimeToolSource + ?Sized>(
    llm: Arc<dyn LlmExecutor>,
    sandbox: &S,
) -> Runtime {
    let mut runtime = Runtime::new()
        .with_llm(llm)
        .with_gate(server_gate())
        // Structure-only metrics at the model/tool chokepoints (#2). Binds to the
        // global meter installed by `observability::init()`; a no-op when none is.
        .with_metrics(Arc::new(awaken_observability::OtelMetricsRecorder::new()))
        // The tool state machine is available on every runtime; an agent activates
        // it via `plugin_ids` and configures its machines via `plugin_config`.
        .with_plugin(Arc::new(StateMachinePlugin::empty()))
        .with_plugin(Arc::new(
            awaken_ext_background_task::BackgroundTaskPlugin::new(Default::default()),
        ));
    // The full capability surface (ADR-0035 D8): hand tools plus provisioned skill
    // tools. Placement-agnostic — the kernel sees `RawTool`s, not "skills".
    for tool in sandbox.runtime_tools() {
        runtime = runtime.with_tool(tool);
    }
    runtime
}

/// Materialize the effective Agent/Session authorization onto the ordinary
/// Native runtime. Root Runs, delegated Runs, and recovered delegated Runs use
/// this same installation seam; only their frozen configuration input differs.
pub(crate) fn build_runtime_with_authorization<S: RuntimeToolSource + ?Sized>(
    llm: Arc<dyn LlmExecutor>,
    sandbox: &S,
    authorization: &EffectiveToolAuthorization,
) -> Runtime {
    let runtime = build_runtime(llm, sandbox);
    if authorization.explicit {
        runtime.with_gate(authorization.gate.clone())
    } else {
        runtime
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
    use awaken_runtime_contract::resolved::ResolvedSpec;

    struct NoLlm;
    #[async_trait::async_trait]
    impl LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            unreachable!("plugin validation never calls the model")
        }
    }

    fn config_with(section: serde_json::Value) -> ResolvedSpec {
        let config = server_config(
            "assistant",
            "m",
            &HashSet::new(),
            &HashSet::new(),
            &["state_machine".to_string()],
            &Default::default(),
            &[],
            ContextPolicy::KeepAll,
        );
        let mut spec = config.resolved_spec.clone();
        spec.plugin_config
            .insert(STATE_MACHINE_PLUGIN_ID.to_string(), section);
        spec
    }

    #[test]
    fn client_tool_uses_the_canonical_execution_ownership() {
        // Cause graph: managed client declaration -> canonical descriptor
        // constructor -> ClientExecuted ownership -> runtime awaits the caller.
        // A generic pinned descriptor would instead claim server execution.
        //
        // Decision table:
        // | declaration | descriptor kind | execution owner |
        // | client      | ClientExecuted  | caller          |
        let descriptor = client_tool_descriptor("lookup");
        assert_eq!(
            descriptor.kind,
            awaken_runtime_contract::resolved::ToolKind::ClientExecuted
        );
    }

    #[test]
    fn authorable_network_tools_match_their_single_runtime_owners() {
        // Cause/effect graph and decision table:
        // C1 WebFetch/WebSearch each have one configured plugin owner. E1 each
        // exact id appears once in the publication catalog; E2 neither is in
        // the static bundle. R1=C1 => E1+E2. FMECA: registering either
        // statically would create a second unconfigured execution path.
        let catalog = authorable_tools();
        let count = |id: &str| catalog.iter().filter(|tool| tool.id == id).count();
        assert_eq!(count("web_fetch"), 1, "R1/E1");
        assert_eq!(count("web_search"), 1, "R1/E1");

        let static_ids = awaken_ext_builtin_tools::all_hand_tools()
            .into_iter()
            .map(|tool| tool.id().to_string())
            .collect::<Vec<_>>();
        assert!(!static_ids.iter().any(|id| id == "web_fetch"), "R1/E2");
        assert!(!static_ids.iter().any(|id| id == "web_search"), "R1/E2");
    }

    #[test]
    fn platform_capabilities_expose_the_schema() {
        // Cause/effect: installed provider descriptors derive one plugin schema
        // and exact tool bound. The free provider is the first authoring default;
        // paid provider authentication remains the common CredentialUsage wire.
        let caps = platform_plugin_capabilities();
        assert!(
            caps.iter()
                .any(|c| c.id == STATE_MACHINE_PLUGIN_ID && c.config_schema.is_some())
        );
        let web = caps
            .iter()
            .find(|capability| capability.id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID)
            .expect("WebSearch capability");
        assert_eq!(
            web.bound.tools,
            awaken_runtime_contract::plugin::IdBound::Exact(vec![
                awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID.into()
            ])
        );
        let branches = web.config_schema.as_ref().unwrap()["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(branches.len(), 3);
        assert_eq!(
            branches[0]["properties"]["provider_id"]["const"],
            "duckduckgo"
        );
        let fetch = caps
            .iter()
            .find(|capability| capability.id == awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID)
            .expect("WebFetch capability");
        assert_eq!(
            fetch.bound.tools,
            awaken_runtime_contract::plugin::IdBound::Exact(vec![
                awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID.into()
            ])
        );
        assert_eq!(
            branches[1]["properties"]["credential"]["x-awaken-credential-application"]["type"],
            "http_header"
        );
    }

    #[test]
    fn config_permission_ruleset_reads_the_section_else_none() {
        // Absent → None (keeps the strict built-in default).
        assert!(config_permission_ruleset(&Default::default()).is_none());
        // Present + valid → Some.
        let mut cfg = std::collections::BTreeMap::new();
        cfg.insert(
            PERMISSION_CONFIG_KEY.to_string(),
            serde_json::json!({ "default_behavior": "deny", "rules": [] }),
        );
        assert_eq!(
            config_permission_ruleset(&cfg).unwrap().default_behavior,
            ToolPermissionBehavior::Deny
        );
        // Malformed (fail-open regex operator) → None, never a silent fail-open.
        let mut bad = std::collections::BTreeMap::new();
        bad.insert(
            PERMISSION_CONFIG_KEY.to_string(),
            serde_json::json!({ "rules": [{ "pattern": "Bash(command =~ \"x\")", "behavior": "deny" }] }),
        );
        assert!(config_permission_ruleset(&bad).is_none());
    }

    #[test]
    fn effective_ruleset_layers_author_over_baseline() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect rules: with no authored policy, canonical perception and
        // delegation tools are allowed while a mutation requires confirmation;
        // authored allow/deny rules then layer over that same baseline, with deny
        // remaining absolute. Using the exported delegation id here proves the
        // descriptor and permission owners cannot drift behind duplicate literals.
        //
        // | Rule | Policy | Tool | Effect |
        // |---|---|---|---|
        // | P1 | baseline | read / AGENT_RUN | allow |
        // | P2 | baseline | write | require confirmation |
        // | P3 | authored allow | Bash(ls) | allow |
        // | P4 | authored allow+deny | Bash(rm) | deny |
        let base = effective_ruleset(None, &[]);
        assert_eq!(
            base.decide("read", &serde_json::json!({})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            base.decide(AGENT_RUN, &serde_json::json!({})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            base.decide("write", &serde_json::json!({})),
            ToolPermissionBehavior::RequireConfirmation
        );

        // Authored: allow bash but deny rm; default stays ask. Baseline read still allowed.
        let authored = awaken_ext_permission::parse_ruleset(&serde_json::json!({
            "default_behavior": "ask",
            "rules": [
                { "pattern": "Bash", "behavior": "allow" },
                { "pattern": "Bash(*rm*)", "behavior": "deny" }
            ]
        }))
        .unwrap();
        let merged = effective_ruleset(Some(authored), &[]);
        assert_eq!(
            merged.decide("read", &serde_json::json!({})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            merged.decide("Bash", &serde_json::json!({ "command": "ls" })),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            merged.decide("Bash", &serde_json::json!({ "command": "rm -rf" })),
            ToolPermissionBehavior::Deny // deny is absolute, even over the author's allow
        );
    }

    #[test]
    fn toolset_policy_controls_execution_gate_behavior() {
        // Cause graph: normalized exact toolset policy -> permission rules at
        // runtime construction -> gate decision before RawTool invocation.
        // Visibility is tested at request assembly; this table proves execution
        // behavior and therefore prevents a data-only projection from passing.
        // Effects: disabled tools deny, explicit ask remains ask, and the enabled
        // default remains allow. Constraint/Invariant: the frozen normalized
        // ToolsetPolicy is the only execution-gate input. Decision rule: cover
        // disabled, explicit ask, inherited ask, and default-allow partitions.
        //
        // Decision table:
        // | tool                         | enabled | permission   | gate result |
        // | read                         | false   | allow        | deny        |
        // | write                        | true    | ask          | ask         |
        // | mcp__docs__search            | true    | ask          | ask         |
        // | mcp__docs__fetch (default)   | true    | allow        | allow       |
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };
        let policy = |enabled, permission| ToolExecutionPolicy {
            enabled,
            permission,
        };
        let toolsets = vec![
            ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: vec![
                    ToolPolicyOverride::new(
                        "read",
                        policy(false, ToolPermissionRequirement::AlwaysAllow),
                    ),
                    ToolPolicyOverride::new(
                        "write",
                        policy(true, ToolPermissionRequirement::AlwaysAsk),
                    ),
                ],
            },
            ToolsetPolicy {
                source: ToolsetSource::Mcp {
                    server_name: "docs".into(),
                },
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::new(
                    "search",
                    policy(true, ToolPermissionRequirement::AlwaysAsk),
                )],
            },
        ];
        let rules = effective_ruleset_with_toolsets(None, &[], &toolsets);
        assert_eq!(
            rules.decide("read", &serde_json::json!({})),
            ToolPermissionBehavior::Deny
        );
        assert_eq!(
            rules.decide("write", &serde_json::json!({})),
            ToolPermissionBehavior::RequireConfirmation
        );
        assert_eq!(
            rules.decide("mcp__docs__search", &serde_json::json!({})),
            ToolPermissionBehavior::RequireConfirmation
        );
        assert_eq!(
            rules.decide("mcp__docs__fetch", &serde_json::json!({})),
            ToolPermissionBehavior::Allow
        );
    }

    #[tokio::test]
    async fn build_runtime_registers_the_plugin_and_validates_config() {
        // A1: the configured runtime has the plugin (a valid section resolves).
        // A3: a malformed section fails closed at publish-time validation.
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = awaken_sandbox_local::LocalProvider::new(tmp.path())
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("t"))
            .await
            .unwrap();
        let runtime = build_runtime(Arc::new(NoLlm), &sandbox);
        assert!(
            runtime
                .validate_plugins(&config_with(serde_json::json!({"machines": []})))
                .is_ok(),
            "the state machine plugin is registered and resolves a valid section"
        );
        let malformed = serde_json::json!({"machines":[{"name":"m","initial":"a",
            "transitions":[{"on":"Read(","from":"a","to":"b"}]}]});
        assert!(
            runtime.validate_plugins(&config_with(malformed)).is_err(),
            "a malformed section is rejected before publish"
        );
    }
}
