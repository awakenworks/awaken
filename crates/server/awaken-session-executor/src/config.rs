// @formal entrypoint tool_policy.managed_agent_and_mcp_defaults_are_source_exact_and_configurable
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
use awaken_executable_agent_contract::capability::PluginCapability;
use awaken_ext_builtin_tools::{
    AGENT_RUN, Toolset, WebFetchPlugin, WebSearchPlugin, WebSearchProviderRegistry, all_hand_tools,
    builtin_tools,
};
use awaken_ext_permission::{
    PermissionRule, PermissionRuleset, RuleBasedToolPermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_ext_state_machine::{STATE_MACHINE_PLUGIN_ID, StateMachinePlugin};
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::permission::{ToolGateHook, ToolPermissionPolicy};
use awaken_runtime_contract::plugin::Plugin;
use awaken_runtime_contract::resolved::{
    ContextPolicy, InvalidModelBinding, ModelBinding, ToolDescriptor,
};
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
#[cfg(test)]
use awaken_sandbox_local::LocalSandbox;
use awaken_tool_contract::tool::{RawTool, RawToolRegistry, ToolExecutor};

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

/// The built-in baseline rules. The closed Agent-toolset contract is the sole
/// membership and controlled-modification owner: perception is allowed, while
/// Bash/write/edit remain ask without typed authoring. The
/// descriptor registry proves availability, so execution and advertisement
/// cannot drift behind a second literal list. MCP keeps its separate default-ask
/// policy.
fn base_allow_rules(extra_allowed: &[String], deferred: &[String]) -> Vec<PermissionRule> {
    let allow = |name: &str| {
        PermissionRule::new(
            ToolCallPattern::parse(name).expect("static pattern"),
            ToolPermissionBehavior::Allow,
        )
    };
    // The perception allowance is the host's default for tools no author spoke
    // for. A member the author deferred to `auto` is spoken for: emitting the
    // baseline allow for it would let the host's default pre-empt the
    // evaluator, and `auto` would then be decorative for every observing tool.
    let mut rules: Vec<PermissionRule> = hand_tool_descriptors()
        .into_iter()
        .filter(|descriptor| {
            awaken_session_configuration_contract::is_agent_toolset_member(&descriptor.id)
                && !awaken_session_configuration_contract::is_controlled_modification_member(
                    &descriptor.id,
                )
                && !deferred.iter().any(|name| name == &descriptor.id)
        })
        .map(|descriptor| allow(&descriptor.id))
        .collect();
    // `agent_run` is allowed: the kernel executes it via the injected delegation
    // resolver (a sub-agent, native or remote), not the tool registry — delegation is
    // a runtime concern. Skill discovery/activation grant perception (they list
    // metadata and return instructions), not authorization; allow them without a
    // confirmation prompt. Any tool a skill then invokes is still gated.
    rules.push(allow(AGENT_RUN));
    rules.push(allow(awaken_runtime_contract::resolved::ADVISOR_TOOL_ID));
    rules.push(allow("list_skills"));
    rules.push(allow("Skill"));
    // Semantic Memory is outside the Agent toolset contract. Perception is
    // allowed explicitly while mutations retain the unmatched default.
    rules.push(allow("list_memories"));
    rules.push(allow("read_memory"));
    // Thread-local management is safe to admit. The selected ordinary tool
    // crosses PreparedToolExecutor::start_task's own authorization boundary;
    // this allowance neither grants it nor suppresses its deny/ask decision.
    rules.extend(
        awaken_ext_background_task::management_tool_ids()
            .into_iter()
            .map(allow),
    );
    rules.extend(extra_allowed.iter().map(|id| allow(id)));
    rules
}

/// The tools whose calls only observe, for the `auto` evaluator.
///
/// Derived from the closed Agent toolset contract rather than a literal list:
/// the same membership that decides which tools are controlled modifications
/// decides which are safe to run unattended, so the two can never disagree.
fn observing_tool_ids() -> Vec<String> {
    awaken_session_configuration_contract::agent_toolset_members()
        .filter(|name| {
            !awaken_session_configuration_contract::is_controlled_modification_member(name)
        })
        .map(str::to_string)
        .collect()
}

fn server_policy(extra_allowed: &[String]) -> RuleBasedToolPermissionPolicy {
    RuleBasedToolPermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::RequireConfirmation,
        rules: base_allow_rules(extra_allowed, &[]),
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

/// The ruleset a thread actually enforces: the built-in Agent-tool baseline
/// plus pre-authorized dynamic ids, with an authored policy's rules
/// layered on top and its `default_behavior` governing unmatched calls
/// (`deny` in any rule still wins, absolutely). With no authored policy this is
/// the fail-closed default: perception is allowed, controlled modifications and
/// unmatched tools ask.
#[cfg(test)]
pub(crate) fn effective_ruleset(
    authored: Option<PermissionRuleset>,
    extra_allowed: &[String],
) -> PermissionRuleset {
    effective_ruleset_deferring(authored, extra_allowed, &[])
}

/// [`effective_ruleset`] for a Session whose toolsets deferred some members to
/// `auto`; those members get no baseline rule so the evaluator can be heard.
fn effective_ruleset_deferring(
    authored: Option<PermissionRuleset>,
    extra_allowed: &[String],
    deferred: &[String],
) -> PermissionRuleset {
    let mut rules = base_allow_rules(extra_allowed, deferred);
    let default_behavior = authored
        .as_ref()
        .map_or(ToolPermissionBehavior::RequireConfirmation, |rs| {
            rs.default_behavior
        });
    if let Some(rs) = authored {
        rules.extend(rs.rules);
    }
    PermissionRuleset {
        default_behavior,
        rules,
    }
}

/// The closed Agent toolset, desugared.
///
/// Its membership is fixed and versioned, so the default is a shorthand for
/// "this policy, for every member" and is expanded here. The expansion splits:
/// a rule states a decision, so only the fixed requirements become rules, while
/// `auto` is a deferral and its members are named instead for the policy to
/// consult the evaluator about — after every authored rule has had its say.
struct AgentToolsetProjection {
    rules: Vec<PermissionRule>,
    deferred_members: Vec<String>,
}

fn agent_toolset_permission_rules(
    toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
) -> AgentToolsetProjection {
    use awaken_runtime_contract::agent_bindings::{ToolPermissionRequirement, ToolsetSource};
    let mut deferred_members = Vec::new();
    // A fixed requirement compiles into an exact rule that still names the
    // requirement it came from, so the audit (and the Managed `evaluation`)
    // reports `always_allow`/`always_ask` rather than an anonymous rule. An
    // enabled `auto` compiles into no rule: it is deferred to the evaluator.
    let fixed_rule =
        |pattern: &str, policy: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy| {
            PermissionRule::from_fixed_policy(
                ToolCallPattern::parse(pattern).expect("typed toolset produces a valid pattern"),
                policy,
            )
        };
    let mut rules = Vec::new();
    for toolset in toolsets {
        match &toolset.source {
            ToolsetSource::Agent => {
                // The versioned Agent Toolset is closed, so its default is an
                // executable policy for every omitted member rather than mere
                // authoring metadata. Project the complete canonical roster;
                // retain explicit Runtime-only Agent overrides separately.
                for name in awaken_session_configuration_contract::agent_toolset_members() {
                    let mut policy = toolset.policy_for(name);
                    // Ask-by-default for a controlled modification the author never
                    // named. `auto` is the author naming it — a deferral is a
                    // decision — so it passes through to the evaluator like any
                    // other authored requirement.
                    if awaken_session_configuration_contract::is_controlled_modification_member(
                        name,
                    ) && !toolset.overrides.iter().any(|entry| entry.name == name)
                        && policy.permission != ToolPermissionRequirement::Auto
                    {
                        policy.permission = ToolPermissionRequirement::AlwaysAsk;
                    }
                    match fixed_rule(name, policy) {
                        Some(rule) => rules.push(rule),
                        None => deferred_members.push(name.to_string()),
                    }
                }
                for entry in toolset.overrides.iter().filter(|entry| {
                    !awaken_session_configuration_contract::is_agent_toolset_member(&entry.name)
                }) {
                    match fixed_rule(&entry.name, entry.policy) {
                        Some(rule) => rules.push(rule),
                        None => deferred_members.push(entry.name.clone()),
                    }
                }
            }
            // Open MCP membership is resolved by the canonical typed lookup
            // per invocation, not by overlapping wildcard/exact permission rules.
            ToolsetSource::Mcp { .. } => {}
        }
    }
    AgentToolsetProjection {
        rules,
        deferred_members,
    }
}

pub(crate) fn effective_policy_with_toolsets(
    authored: Option<PermissionRuleset>,
    extra_allowed: &[String],
    toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
    evaluator: Arc<dyn awaken_ext_permission::AutoPermissionEvaluator>,
) -> RuleBasedToolPermissionPolicy {
    let projection = agent_toolset_permission_rules(toolsets);
    let mut resolved =
        effective_ruleset_deferring(authored, extra_allowed, &projection.deferred_members);
    // Put exact toolset rules first so they are authoritative over the built-in
    // baseline on equal specificity. Compile rejects a second authored permission
    // section when toolsets exist.
    let mut rules = projection.rules;
    rules.extend(resolved.rules);
    resolved.rules = rules;
    RuleBasedToolPermissionPolicy::new(resolved)
        .with_mcp_toolsets(toolsets)
        .with_deferred_members(projection.deferred_members)
        .with_auto_evaluator(evaluator)
}

/// The evaluator this host wires for the `auto` policy: the observing members
/// of the closed Agent toolset run unattended, everything else reaches a person.
/// This host owns the tool catalogue, so it states the observing set once
/// instead of the policy crate keeping a second copy of anyone's tool names.
pub(crate) fn platform_auto_evaluator() -> Arc<dyn awaken_ext_permission::AutoPermissionEvaluator> {
    Arc::new(awaken_ext_permission::ObservingToolEvaluator::new(
        observing_tool_ids(),
    ))
}

#[cfg(test)]
pub(crate) fn test_agent_toolset_permission(
    name: &str,
    permission: awaken_runtime_contract::agent_bindings::ToolPermissionRequirement,
) -> awaken_runtime_contract::agent_bindings::ToolsetPolicy {
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };
    ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy::default(),
        overrides: vec![ToolPolicyOverride::new(
            name,
            ToolExecutionPolicy {
                enabled: true,
                permission,
            },
        )],
    }
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
    let policy: Arc<dyn ToolPermissionPolicy> = Arc::new(effective_policy_with_toolsets(
        authored,
        extra_allowed,
        toolsets,
        platform_auto_evaluator(),
    ));
    EffectiveToolAuthorization {
        gate: Arc::new(PermissionGate::new(policy.clone())),
        policy,
        explicit,
    }
}

pub(crate) fn hand_tool_descriptors() -> Vec<ToolDescriptor> {
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

/// The registered built-in Hand tools advertised on a managed Session. The ask
/// bit projects the same controlled-modification predicate as authorization;
/// exact typed overrides are applied separately from the Session configuration.
pub(crate) fn builtin_hand_tools() -> Vec<(String, bool)> {
    hand_tool_descriptors()
        .into_iter()
        .map(|descriptor| {
            let ask =
                !awaken_session_configuration_contract::is_agent_toolset_member(&descriptor.id)
                    || awaken_session_configuration_contract::is_controlled_modification_member(
                        &descriptor.id,
                    );
            (descriptor.id, ask)
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionToolsetProjection {
    PreservePublished,
    ProjectIntoSnapshot,
}

/// Apply the Session's exact client-tool overlay in one place. Toolset policy
/// remains outside an immutable primary publication but is embedded in a
/// generated config or inherited child dispatch snapshot, whose executor has
/// no separate Session policy input.
pub(crate) fn project_session_tool_override(
    snapshot: &mut ExecutableAgentSnapshot,
    tools: &awaken_session_configuration_contract::SessionToolConfiguration,
    toolsets: SessionToolsetProjection,
) -> bool {
    let mut changed = false;
    if toolsets == SessionToolsetProjection::ProjectIntoSnapshot
        && snapshot.resolved_spec.plugin_config.agent.toolsets != tools.toolsets
    {
        snapshot.resolved_spec.plugin_config.agent.toolsets = tools.toolsets.clone();
        changed = true;
    }
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
    if current != projected {
        snapshot
            .resolved_spec
            .tool_descriptors
            .retain(|descriptor| {
                descriptor.kind != awaken_runtime_contract::resolved::ToolKind::ClientExecuted
            });
        snapshot.resolved_spec.tool_descriptors.extend(projected);
        changed = true;
    }
    changed
}

/// Apply the complete Session-owned tool replacement to one attempt-local
/// snapshot. The retained Agent publication stays immutable; direct, durable,
/// and recovered attempts all consume this same projection.
pub(crate) fn project_session_tools(
    snapshot: &mut ExecutableAgentSnapshot,
    tools: &awaken_session_configuration_contract::SessionToolConfiguration,
) -> Result<(), serde_json::Error> {
    if project_session_tool_override(
        snapshot,
        tools,
        SessionToolsetProjection::ProjectIntoSnapshot,
    ) {
        snapshot.recompute_fingerprint()?;
    }
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

/// The generated host-default snapshot for `agent_id` on `model_ref`.
///
/// `model_ref` is the host's configured default model reference (an operator
/// string, not a decoded binding), so it is admitted here and a blank or padded
/// reference is refused at the Session boundary instead of panicking.
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
) -> Result<ExecutableAgentSnapshot, InvalidModelBinding> {
    let model = ModelBinding::parse("default", model_ref, "default")?;
    let tools = advertised_tools(client_tools, delegates, dynamic_descriptors);
    let snapshot = ExecutableAgentSnapshot::builder(agent_id)
        .instructions(SYSTEM_PROMPT)
        .model(model)
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
        .build();
    Ok(snapshot)
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
            bound: awaken_ext_background_task::background_task_manifest().bound,
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
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>;
}

pub(crate) struct DeferredHandToolSource;

impl RuntimeToolSource for DeferredHandToolSource {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_tool_contract::tool::RawTool>> {
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
        call: &awaken_tool_contract::tool::ToolCall,
    ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError> {
        if self.tools.get(&call.tool_id).is_none() {
            return Err(awaken_tool_contract::tool::ToolError::DefinitiveFailure(
                format!("filesystem-free Session cannot execute `{}`", call.tool_id),
            ));
        }
        self.tools.invoke(call).await
    }
}

impl RuntimeToolSource for Box<dyn awaken_session_environment_contract::RootedToolSandbox> {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_tool_contract::tool::RawTool>> {
        self.rooted_tools()
    }
}

#[cfg(test)]
impl RuntimeToolSource for LocalSandbox {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_tool_contract::tool::RawTool>> {
        self.rooted_tools()
    }
}

impl RuntimeToolSource for crate::session_environment::SessionEnvironment {
    fn runtime_tools(&self) -> Vec<Arc<dyn awaken_tool_contract::tool::RawTool>> {
        self.agent_tools().rooted_tools()
    }
}

pub(crate) fn build_runtime<S: RuntimeToolSource + ?Sized>(
    llm: Arc<dyn LlmExecutor>,
    sandbox: &S,
    background_supervisor: Option<Arc<awaken_ext_background_task::BackgroundTaskSupervisor>>,
) -> Runtime {
    let mut runtime = Runtime::new()
        .with_llm(llm)
        .with_gate(server_gate())
        // Structure-only metrics at the model/tool chokepoints (#2). Binds to the
        // global meter installed by `observability::init()`; a no-op when none is.
        .with_metrics(Arc::new(awaken_observability::OtelMetricsRecorder::new()))
        // The tool state machine is available on every runtime; an agent activates
        // it via `plugin_ids` and configures its machines via `plugin_config`.
        .with_plugin(Arc::new(StateMachinePlugin::empty()));
    if let Some(supervisor) = background_supervisor {
        runtime = runtime.with_plugin(Arc::new(
            awaken_ext_background_task::BackgroundTaskPlugin::with_supervisor(
                Default::default(),
                supervisor,
            ),
        ));
    }
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
    background_supervisor: Option<Arc<awaken_ext_background_task::BackgroundTaskSupervisor>>,
) -> Runtime {
    let runtime = build_runtime(llm, sandbox, background_supervisor);
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
        )
        .expect("literal model reference");
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
        // Capability cause/effect table. C1 the authoritative registry contains
        // direct and server-routed providers; C2 every provider id is unique;
        // C3 a direct provider requires credentials. E1 the projected schema
        // contains every direct provider and no duplicate branch; E2 the free
        // default remains selectable; E3 paid authoring exposes only the Vault
        // source pin and hides provider wire mechanics. The provider crate owns
        // the server-route list,
        // so this adapter test deliberately derives expectations from the same
        // registry instead of copying its current branch count.
        //
        // | Rule | registry | credential | Effect |
        // | C1 | builtins | none | E1 + E2 |
        // | C1 + C2 + C3 | builtins | required | E1 + E3 |
        let providers = WebSearchProviderRegistry::builtins();
        let caps = platform_plugin_capabilities_with_web_search(&providers);
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
        let branch_ids = branches
            .iter()
            .map(|branch| {
                branch["properties"]["provider_id"]["const"]
                    .as_str()
                    .expect("provider branch id")
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(branch_ids.len(), branches.len(), "C2/E1 unique branches");
        for descriptor in providers.descriptors() {
            assert!(branch_ids.contains(descriptor.id.as_str()), "C1/E1");
        }
        assert!(branch_ids.contains("duckduckgo"), "C1/E2 free default");
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
        let brave = branches
            .iter()
            .find(|branch| branch["properties"]["provider_id"]["const"] == "brave")
            .expect("Brave provider branch");
        let credential = &brave["properties"]["credential"];
        assert_eq!(
            credential["required"],
            serde_json::json!(["id", "revision"]),
            "C3/E3 authoring exposes only an exact Vault selection"
        );
        assert!(
            credential.get("x-awaken-credential-application").is_none()
                && !credential.to_string().contains("X-Subscription-Token"),
            "C3/E3 provider injection mechanics never enter the model-facing schema"
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

    #[tokio::test]
    async fn an_auto_agent_toolset_defers_only_where_the_author_stated_nothing() {
        // Cause/effect graph: C1 the Agent toolset default is `auto`; C2 the tool
        // only observes; C3 the tool is a controlled modification; C4 the author
        // wrote an explicit per-tool override. E1 an observing call runs without a
        // person, E2 a modifying call goes to a person because the shipped
        // evaluator reaches no conclusion for it (not because a host default
        // forced ask — `an_auto_default_is_heard_for_every_member` proves the
        // evaluator is what decides), E3 an explicit override decides instead of
        // the evaluator.
        //
        // | Rule | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|
        // | T1 | T | F | F | E1 allow |
        // | T2 | F | T | F | E2 ask |
        // | T3 | F | T | always_allow | E3 allow — the author outranks the evaluator |
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };
        use awaken_runtime_contract::permission::{ToolPermissionPolicy, ToolPermissionVerdict};

        let call = |tool: &str| awaken_runtime_contract::permission::ToolCall {
            tool_id: tool.into(),
            call_id: "auto-host".into(),
            arguments: serde_json::json!({}),
        };
        let toolset = |overrides: Vec<ToolPolicyOverride>| ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::Auto,
            },
            overrides,
        };

        let plain = effective_policy_with_toolsets(
            None,
            &[],
            &[toolset(Vec::new())],
            platform_auto_evaluator(),
        );
        assert!(
            matches!(
                plain.evaluate(&call("read")).await.verdict,
                ToolPermissionVerdict::Allow
            ),
            "T1 an observing call runs unattended"
        );
        assert!(
            matches!(
                plain.evaluate(&call("bash")).await.verdict,
                ToolPermissionVerdict::RequireConfirmation { .. }
            ),
            "T2 a modifying call reaches a person, never a refusal"
        );

        let overridden = effective_policy_with_toolsets(
            None,
            &[],
            &[toolset(vec![ToolPolicyOverride::new(
                "bash",
                ToolExecutionPolicy {
                    enabled: true,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
            )])],
            platform_auto_evaluator(),
        );
        assert!(
            matches!(
                overridden.evaluate(&call("bash")).await.verdict,
                ToolPermissionVerdict::Allow
            ),
            "T3 an explicit authored policy outranks the evaluator"
        );
    }

    #[tokio::test]
    async fn an_auto_default_is_heard_for_every_member() {
        // Cause/effect graph: C1 the Agent toolset default is `auto` with no
        // per-tool override; C2 the injected evaluator answers `Indeterminate`
        // for everything; C3 it answers `Deny` for everything. E1 every member —
        // an observing one and a controlled modification alike — reaches the
        // evaluator: under C2 it asks, under C3 it is refused. Two host defaults
        // used to pre-empt this: the perception baseline emitted `allow` for
        // observing members, and ask-by-default rewrote controlled members to a
        // fixed ask. Either one makes `auto` decorative for that member.
        //
        // | Rule | member | evaluator | Effect |
        // |---|---|---|---|
        // | H1 | read (observes) | indeterminate | ask — baseline must not answer for it |
        // | H2 | bash (controlled) | indeterminate | ask — via the evaluator, not a forced rule |
        // | H3 | read (observes) | deny | deny — proves the evaluator, not a rule, decided |
        // | H4 | bash (controlled) | deny | deny |
        use awaken_ext_permission::{AutoDecision, AutoPermissionEvaluator};
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
        };
        use awaken_runtime_contract::permission::{
            ToolCall, ToolPermissionPolicy, ToolPermissionVerdict,
        };

        #[derive(Debug)]
        struct Fixed(AutoDecision);
        impl AutoPermissionEvaluator for Fixed {
            fn evaluate(&self, _call: &ToolCall) -> AutoDecision {
                self.0
            }
        }
        let auto_toolset = ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::Auto,
            },
            overrides: Vec::new(),
        };
        let call = |tool: &str| ToolCall {
            tool_id: tool.into(),
            call_id: "auto-heard".into(),
            arguments: serde_json::json!({}),
        };

        let asks = effective_policy_with_toolsets(
            None,
            &[],
            std::slice::from_ref(&auto_toolset),
            Arc::new(Fixed(AutoDecision::Indeterminate)),
        );
        for (rule, tool) in [("H1", "read"), ("H2", "bash")] {
            assert!(
                matches!(
                    asks.evaluate(&call(tool)).await.verdict,
                    ToolPermissionVerdict::RequireConfirmation { .. }
                ),
                "{rule} `{tool}` must reach the evaluator and ask"
            );
        }

        let denies = effective_policy_with_toolsets(
            None,
            &[],
            std::slice::from_ref(&auto_toolset),
            Arc::new(Fixed(AutoDecision::Deny)),
        );
        for (rule, tool) in [("H3", "read"), ("H4", "bash")] {
            assert!(
                matches!(
                    denies.evaluate(&call(tool)).await.verdict,
                    ToolPermissionVerdict::Deny { .. }
                ),
                "{rule} `{tool}` must be decided by the evaluator"
            );
        }
        // H5: exercise the same policy through the real Runtime. Auto-deny
        // commits an error result, never invokes the tool, and never creates a
        // reply ticket. The managed resolver's R10 rejects a confirmation when
        // no pending owner exists; the SDK A4 checks HTTP non-ask rejection.
        // This injectable deterministic evaluator is not a claim about the
        // shipped observing evaluator's ability to classify session content.
        use awaken_runtime_contract::execution::RunExecutor as _;
        struct OneRead;
        #[async_trait::async_trait]
        impl awaken_runtime_contract::llm::LlmExecutor for OneRead {
            async fn infer(
                &self,
                request: awaken_runtime_contract::llm::ChatRequest,
            ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
            {
                let output = if request.messages.iter().any(|message| {
                    message.role == awaken_agent_contract::agent::message::Role::Tool
                }) {
                    awaken_runtime_contract::llm::AssistantOutput::text("denial observed")
                } else {
                    awaken_runtime_contract::llm::AssistantOutput::from_tool_calls(vec![ToolCall {
                        tool_id: "read".into(),
                        call_id: "auto-denied-read".into(),
                        arguments: serde_json::json!({}),
                    }])
                };
                Ok(awaken_runtime_contract::llm::ChatResponse {
                    output,
                    usage: None,
                    stop_reason: None,
                })
            }
        }
        struct ReadCounter(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl awaken_tool_contract::tool::RawTool for ReadCounter {
            fn id(&self) -> &str {
                "read"
            }
            async fn invoke(
                &self,
                call: ToolCall,
            ) -> Result<awaken_tool_contract::tool::ToolOutput, awaken_tool_contract::tool::ToolError>
            {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(awaken_tool_contract::tool::ToolOutput::ok(
                    call.call_id,
                    "read",
                ))
            }
        }
        let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runtime = awaken_runtime::Runtime::new()
            .with_llm(Arc::new(OneRead))
            .with_tool(Arc::new(ReadCounter(effects.clone())));
        let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("auto-deny")
            .instructions("read once")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "stub", "native")
                    .expect("admitted literal test binding"),
            )
            .build();
        let (run_id, activation) = runtime.prepare(&snapshot, "auto-deny".into(), "read");
        let commit = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
            awaken_time::system_clock(),
        ));
        let outcome = runtime
            .execute(
                activation,
                awaken_runtime_contract::RuntimeRunContext::new()
                    .with_commit(commit.clone())
                    .with_tool_permission_policy(Arc::new(denies)),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            awaken_agent_contract::agent::run::RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::NaturalEnd
            ),
            "H5 denial is an error result, not an operator pause"
        );
        assert_eq!(
            effects.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "H5 no tool effect"
        );
        assert!(
            commit.resume_ticket_for(&run_id).is_none(),
            "H5 no confirmation authority"
        );
        assert!(
            commit
                .committed()
                .messages
                .iter()
                .any(|message| message.content.iter().any(|block| matches!(
                    block,
                    awaken_agent_contract::agent::content::ContentBlock::ToolResult {
                        is_error: true,
                        ..
                    }
                ))),
            "H5 committed error tool result"
        );
    }

    #[test]
    fn the_baseline_never_allows_a_controlled_modification_or_an_mcp_tool() {
        // Constraint/Invariant: the built-in baseline is a *perception* allowance.
        // It may never grant a controlled modification, and it may never grant an
        // MCP tool — an MCP tool's authority comes only from its `mcp_toolset`
        // policy, whose default is ask (Managed Agents wire parity). Without this
        // guard a new literal in `base_allow_rules` silently widens the default
        // for every Session that authored no policy.
        //
        // Cause/effect graph: C1 the tool is a controlled-modification member;
        // C2 the tool id is MCP-namespaced. E1 ask (never allow).
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | B1 | T | F | E1 ask |
        // | B2 | F | T | E1 ask |
        let base = effective_ruleset(None, &[]);
        for member in awaken_session_configuration_contract::agent_toolset_members() {
            if awaken_session_configuration_contract::is_controlled_modification_member(member) {
                assert_eq!(
                    base.decide(member, &serde_json::json!({})),
                    ToolPermissionBehavior::RequireConfirmation,
                    "B1 {member} must stay ask in the baseline"
                );
            }
        }
        for mcp_tool in ["mcp__calc__add", "mcp__github__create_issue"] {
            assert_eq!(
                base.decide(mcp_tool, &serde_json::json!({})),
                ToolPermissionBehavior::RequireConfirmation,
                "B2 {mcp_tool} must stay ask without a declared mcp_toolset"
            );
        }
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
        // | P2 | ordinary Managed Agent member | registered perception tool | allow |
        // | P3 | controlled member or unmatched extension | mutation/unknown | ask |
        // | P4 | authored allow | Bash(ls) | allow |
        // | P5 | authored allow+deny | Bash(rm) | deny |
        let base = effective_ruleset(None, &[]);
        assert_eq!(
            base.decide("read", &serde_json::json!({})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            base.decide(AGENT_RUN, &serde_json::json!({})),
            ToolPermissionBehavior::Allow
        );
        for (tool_id, ask) in builtin_hand_tools() {
            let official = awaken_session_configuration_contract::is_agent_toolset_member(&tool_id);
            let controlled =
                awaken_session_configuration_contract::is_controlled_modification_member(&tool_id);
            let expected = if official && !controlled {
                ToolPermissionBehavior::Allow
            } else {
                ToolPermissionBehavior::RequireConfirmation
            };
            assert_eq!(base.decide(&tool_id, &serde_json::json!({})), expected);
            assert_eq!(
                ask,
                !official || controlled,
                "P2/P3 advertised policy for {tool_id}"
            );
        }

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

    #[tokio::test]
    async fn toolset_policy_controls_execution_gate_behavior() {
        // Cause graph: normalized exact toolset policy -> permission rules at
        // runtime construction -> gate decision before RawTool invocation.
        // Visibility is tested at request assembly; this table proves execution
        // behavior and therefore prevents a data-only projection from passing.
        // Effects: disabled tools deny, explicit ask remains ask, an exact
        // always-allow may opt out outside the controlled preset, and an omitted
        // controlled member still asks. Constraint/Invariant: the frozen normalized
        // ToolsetPolicy is the only execution-gate input. Decision rule: cover
        // exact Agent allow, disabled, explicit ask, controlled fallback, MCP
        // inherited ask, and an exact MCP allow override partitions.
        //
        // Decision table:
        // | tool                         | enabled | permission   | gate result |
        // | read                         | false   | allow        | deny        |
        // | write                        | true    | ask          | ask         |
        // | bash (exact opt-out)         | true    | allow        | allow       |
        // | mcp__docs__search            | true    | allow        | allow       |
        // | mcp__docs__fetch (default)   | true    | ask          | ask         |
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
                    ToolPolicyOverride::new(
                        "bash",
                        policy(true, ToolPermissionRequirement::AlwaysAllow),
                    ),
                ],
            },
            ToolsetPolicy {
                source: ToolsetSource::Mcp {
                    server_name: "docs".into(),
                },
                default: policy(true, ToolPermissionRequirement::AlwaysAsk),
                overrides: vec![ToolPolicyOverride::new(
                    "search",
                    policy(true, ToolPermissionRequirement::AlwaysAllow),
                )],
            },
        ];
        let policy =
            effective_policy_with_toolsets(None, &[], &toolsets, platform_auto_evaluator());
        for (tool_id, expected) in [
            (
                "read",
                awaken_runtime_contract::permission::GateOutcomeKind::Block,
            ),
            (
                "write",
                awaken_runtime_contract::permission::GateOutcomeKind::RequireConfirmation,
            ),
            (
                "bash",
                awaken_runtime_contract::permission::GateOutcomeKind::Allow,
            ),
            (
                "mcp__docs__search",
                awaken_runtime_contract::permission::GateOutcomeKind::Allow,
            ),
            (
                "mcp__docs__fetch",
                awaken_runtime_contract::permission::GateOutcomeKind::RequireConfirmation,
            ),
        ] {
            let call = awaken_runtime_contract::llm::ToolCall {
                call_id: "typed-policy".into(),
                tool_id: tool_id.into(),
                arguments: serde_json::json!({}),
            };
            assert_eq!(
                policy
                    .evaluate(&call)
                    .await
                    .verdict
                    .kind()
                    .gate_outcome_kind(),
                expected,
                "{tool_id}"
            );
        }
    }

    #[tokio::test]
    async fn typed_controlled_policy_has_one_native_and_acp_verdict() {
        // Cause/effect graph: C1 an explicit Agent override enables an ordinary
        // member; C2 an MCP member inherits its source default; C3 the controlled
        // preset enables Bash/write/edit with exact ask; C4 an omitted Agent
        // member inherits the disabled Toolset default; C5 a Runtime-only Agent
        // override narrows the permissive baseline. E1 the Native gate outcome
        // and E2 the ACP policy verdict are projections of the same
        // EffectiveToolAuthorization; E3 ordinary perception is allowed while
        // every Bash command and typed file mutation asks.
        // Client-executed ResumeTicket closure is downstream of this decision and
        // deliberately is not a second permission evaluator.
        //
        // Decision table:
        // | rule | source | tool             | typed policy | Native | ACP |
        // | V1   | Agent  | read             | exact allow  | allow  | allow |
        // | V2   | MCP    | docs/search      | default ask  | ask    | ask |
        // | V3   | Agent  | bash/write/edit  | exact ask    | ask    | ask |
        // | V4   | Agent  | omitted glob     | default deny | deny   | deny |
        // | V5   | Agent  | runtime agent_run| exact ask    | ask    | ask |
        use awaken_runtime_contract::agent_bindings::{
            ResolvedConfiguration, ToolExecutionPolicy, ToolPermissionRequirement,
            ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
        };
        use awaken_runtime_contract::permission::GateOutcomeKind;

        let ask = ToolExecutionPolicy {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAsk,
        };
        let toolsets = vec![
            ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
                overrides: std::iter::once(ToolPolicyOverride::new(
                    "read",
                    ToolExecutionPolicy::default(),
                ))
                .chain(
                    ["bash", "write", "edit"]
                        .into_iter()
                        .map(|name| ToolPolicyOverride::new(name, ask)),
                )
                .chain(std::iter::once(ToolPolicyOverride::new("agent_run", ask)))
                .collect(),
            },
            ToolsetPolicy {
                source: ToolsetSource::Mcp {
                    server_name: "docs".into(),
                },
                default: ask,
                overrides: Vec::new(),
            },
        ];
        let authorization =
            effective_tool_authorization(&ResolvedConfiguration::default(), &[], &toolsets);
        let cases = [
            ("read", GateOutcomeKind::Allow, "V1"),
            (
                "mcp__docs__search",
                GateOutcomeKind::RequireConfirmation,
                "V2",
            ),
            ("bash", GateOutcomeKind::RequireConfirmation, "V3"),
            ("write", GateOutcomeKind::RequireConfirmation, "V3"),
            ("edit", GateOutcomeKind::RequireConfirmation, "V3"),
            ("glob", GateOutcomeKind::Block, "V4"),
            ("agent_run", GateOutcomeKind::RequireConfirmation, "V5"),
        ];
        for (tool_id, expected, rule) in cases {
            let call = awaken_runtime_contract::llm::ToolCall {
                call_id: format!("call-{tool_id}"),
                tool_id: tool_id.into(),
                arguments: serde_json::json!({}),
            };
            let native = authorization
                .gate
                .gate(&call, &awaken_agent_contract::agent::state::Store::new())
                .await;
            let acp = authorization.policy.evaluate(&call).await;
            assert_eq!(native.outcome.kind(), expected, "{rule}/E1 {tool_id}");
            assert_eq!(
                acp.verdict.kind().gate_outcome_kind(),
                expected,
                "{rule}/E2 {tool_id}"
            );
        }
    }

    #[tokio::test]
    async fn mcp_defaults_and_exact_overrides_have_one_native_and_acp_verdict() {
        // Cause graph: C1 default enabled/disabled; C2 default allow/ask;
        // C3 exact override absent/disabled/allow/ask; C4 server matches or not.
        // E1 resolved typed policy; E2 Native gate and ACP verdict agree.
        // Decision table is the Cartesian product below (2*2*4*2 = 32 rules):
        // matching source -> exact override replaces default, disabled denies;
        // another source -> ordinary unmatched ask, with no cross-server grant.
        // This includes the live Cowork counterexample: default disabled plus
        // an enabled attest override must allow; a fallback is not a deny rule.
        use awaken_runtime_contract::agent_bindings::{
            ResolvedConfiguration, ToolExecutionPolicy, ToolPermissionRequirement,
            ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
        };
        use awaken_runtime_contract::permission::GateOutcomeKind;
        let allow = ToolExecutionPolicy::default();
        let ask = ToolExecutionPolicy {
            permission: ToolPermissionRequirement::AlwaysAsk,
            ..allow
        };
        let deny = ToolExecutionPolicy {
            enabled: false,
            ..allow
        };
        for enabled in [false, true] {
            for permission in [
                ToolPermissionRequirement::AlwaysAllow,
                ToolPermissionRequirement::AlwaysAsk,
            ] {
                let default = ToolExecutionPolicy {
                    enabled,
                    permission,
                };
                for exact in [None, Some(deny), Some(allow), Some(ask)] {
                    // C5 the configured server name is plain, or carries the
                    // punctuation the id encoding must escape. Both spellings
                    // are one identity, so the matrix holds unchanged for each.
                    for configured in ["context", "context-mcp"] {
                        let toolset = ToolsetPolicy {
                            source: ToolsetSource::Mcp {
                                server_name: configured.into(),
                            },
                            default,
                            overrides: exact
                                .map(|policy| ToolPolicyOverride::new("attest", policy))
                                .into_iter()
                                .collect(),
                        };
                        let authorization = effective_tool_authorization(
                            &ResolvedConfiguration::default(),
                            &[],
                            &[toolset],
                        );
                        for server in [configured, "context_other"] {
                            let selected = if server == configured {
                                exact.unwrap_or(default)
                            } else {
                                ask
                            };
                            let expected = if !selected.enabled {
                                GateOutcomeKind::Block
                            } else if selected.permission == ToolPermissionRequirement::AlwaysAsk {
                                GateOutcomeKind::RequireConfirmation
                            } else {
                                GateOutcomeKind::Allow
                            };
                            let call = awaken_runtime_contract::llm::ToolCall {
                                call_id: "mcp-matrix".into(),
                                tool_id: awaken_agent_contract::McpToolId::new(server, "attest")
                                    .expect("canonical id")
                                    .as_str()
                                    .to_string(),
                                arguments: serde_json::json!({}),
                            };
                            let native = authorization
                                .gate
                                .gate(&call, &awaken_agent_contract::agent::state::Store::new())
                                .await;
                            let acp = authorization.policy.evaluate(&call).await;
                            assert_eq!(
                                native.outcome.kind(),
                                expected,
                                "Native: {default:?} {exact:?} {server}"
                            );
                            assert_eq!(
                                acp.verdict.kind().gate_outcome_kind(),
                                expected,
                                "ACP: {default:?} {exact:?} {server}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn legacy_no_toolset_uses_the_closed_controlled_modification_boundary() {
        // No-toolset cause/effect graph: C1 a legacy Session has no typed Agent
        // Toolset; C2 the registered closed member is perception or controlled
        // modification. Effects: E1 read/glob/grep allow; E2 Bash/write/edit
        // ask. Native and ACP consume the same compiled
        // authorization, so absence of typed authoring cannot restore the old
        // fail-open mutation baseline.
        //
        // | rule | membership | examples | Native / ACP |
        // | L1 | ordinary closed | read/glob/grep | allow |
        // | L2 | controlled closed | bash/write/edit | ask |
        use awaken_runtime_contract::agent_bindings::ResolvedConfiguration;
        use awaken_runtime_contract::permission::GateOutcomeKind;

        let authorization =
            effective_tool_authorization(&ResolvedConfiguration::default(), &[], &[]);
        let cases = [
            ("read", GateOutcomeKind::Allow, "L1"),
            ("glob", GateOutcomeKind::Allow, "L1"),
            ("grep", GateOutcomeKind::Allow, "L1"),
            ("bash", GateOutcomeKind::RequireConfirmation, "L2"),
            ("write", GateOutcomeKind::RequireConfirmation, "L2"),
            ("edit", GateOutcomeKind::RequireConfirmation, "L2"),
        ];
        for (tool_id, expected, rule) in cases {
            let call = awaken_runtime_contract::llm::ToolCall {
                call_id: format!("legacy-{tool_id}"),
                tool_id: tool_id.into(),
                arguments: serde_json::json!({}),
            };
            let native = authorization
                .gate
                .gate(&call, &awaken_agent_contract::agent::state::Store::new())
                .await;
            let acp = authorization.policy.evaluate(&call).await;
            assert_eq!(
                native.outcome.kind(),
                expected,
                "{rule}/E1-E3 Native {tool_id}"
            );
            assert_eq!(
                acp.verdict.kind().gate_outcome_kind(),
                expected,
                "{rule}/E1-E3 ACP {tool_id}"
            );
        }
    }

    #[tokio::test]
    async fn typed_policy_never_uses_bash_text_as_read_only_git_authority() {
        // Approval-granularity cause/effect graph: C1 an exact typed perception
        // tool (read/glob/grep) is selected; C2 an exact typed mutation tool
        // (write/edit) is selected; C3 Bash carries arbitrary command text that
        // looks read-only, commits, publishes, deletes, compounds a mutation, or
        // changes Git behavior through configuration/aliases.
        // Effects: E1 C1 is allowed; E2 C2 asks; E3 every C3 asks solely because
        // the canonical tool id is `bash`, independent of its opaque argument
        // string. Native and ACP consume the same verdict.
        //
        // | Rule | canonical tool | argument class | Native / ACP effect |
        // |---|---|---|---|
        // | G1 | read/glob/grep | typed fields | allow / allow |
        // | G2 | write/edit | typed fields | ask / ask |
        // | G3 | bash | apparent `git status` or `git diff` | ask / ask |
        // | G4 | bash | commit/push/delete/chaining/alias/config | ask / ask |
        //
        // Constraint: this typed preset supplies only an exact `bash` rule and
        // does not use the generic authored-pattern DSL as a Git classifier.
        // Read-only Git therefore cannot be auto-allowed while commit/push/delete
        // are distinguished; every exact Bash call asks and the user confirms
        // the complete command.
        use awaken_runtime_contract::agent_bindings::{
            ResolvedConfiguration, ToolExecutionPolicy, ToolPermissionRequirement,
            ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
        };
        use awaken_runtime_contract::permission::GateOutcomeKind;

        let allow = ToolExecutionPolicy::default();
        let ask = ToolExecutionPolicy {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAsk,
        };
        let toolsets = [ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: false,
                permission: ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: [
                ("read", allow),
                ("glob", allow),
                ("grep", allow),
                ("write", ask),
                ("edit", ask),
                ("bash", ask),
            ]
            .into_iter()
            .map(|(name, policy)| ToolPolicyOverride::new(name, policy))
            .collect(),
        }];
        let authorization =
            effective_tool_authorization(&ResolvedConfiguration::default(), &[], &toolsets);
        let cases = [
            (
                "read",
                serde_json::json!({"path": "/workspace/repository/.git/HEAD"}),
                GateOutcomeKind::Allow,
                "G1",
            ),
            (
                "glob",
                serde_json::json!({"pattern": "**/*.rs"}),
                GateOutcomeKind::Allow,
                "G1",
            ),
            (
                "grep",
                serde_json::json!({"pattern": "TODO"}),
                GateOutcomeKind::Allow,
                "G1",
            ),
            (
                "write",
                serde_json::json!({"path": "/workspace/repository/new.rs"}),
                GateOutcomeKind::RequireConfirmation,
                "G2",
            ),
            (
                "edit",
                serde_json::json!({"path": "/workspace/repository/src/lib.rs"}),
                GateOutcomeKind::RequireConfirmation,
                "G2",
            ),
            (
                "bash",
                serde_json::json!({"command": "git status --short"}),
                GateOutcomeKind::RequireConfirmation,
                "G3",
            ),
            (
                "bash",
                serde_json::json!({"command": "git diff --stat"}),
                GateOutcomeKind::RequireConfirmation,
                "G3",
            ),
            (
                "bash",
                serde_json::json!({"command": "git commit -am approved"}),
                GateOutcomeKind::RequireConfirmation,
                "G4",
            ),
            (
                "bash",
                serde_json::json!({"command": "git push origin HEAD"}),
                GateOutcomeKind::RequireConfirmation,
                "G4",
            ),
            (
                "bash",
                serde_json::json!({"command": "rm -f obsolete.txt"}),
                GateOutcomeKind::RequireConfirmation,
                "G4",
            ),
            (
                "bash",
                serde_json::json!({"command": "git status && git commit -am unexpected"}),
                GateOutcomeKind::RequireConfirmation,
                "G4",
            ),
            (
                "bash",
                serde_json::json!({"command": "git -c alias.status='!touch /workspace/pwned' status"}),
                GateOutcomeKind::RequireConfirmation,
                "G4",
            ),
        ];
        for (index, (tool_id, arguments, expected, rule)) in cases.into_iter().enumerate() {
            let call = awaken_runtime_contract::llm::ToolCall {
                call_id: format!("git-authority-{index}"),
                tool_id: tool_id.into(),
                arguments,
            };
            let native = authorization
                .gate
                .gate(&call, &awaken_agent_contract::agent::state::Store::new())
                .await;
            let acp = authorization.policy.evaluate(&call).await;
            assert_eq!(
                native.outcome.kind(),
                expected,
                "{rule}/E1-E3 Native {tool_id}"
            );
            assert_eq!(
                acp.verdict.kind().gate_outcome_kind(),
                expected,
                "{rule}/E1-E3 ACP {tool_id}"
            );
        }
    }

    #[tokio::test]
    async fn background_management_allowance_does_not_grant_the_wrapped_tool() {
        // B1 extension management is Thread-local -> baseline allows it;
        // B2 unknown effectful MCP target -> still asks; B3 explicit management
        // denial -> denies. PreparedToolExecutor tests and the sales MCP E2E
        // exercise the separate target gate before physical execution.
        use awaken_runtime_contract::permission::{ToolPermissionPolicy, ToolPermissionVerdict};
        let call = |tool: &str| awaken_runtime_contract::permission::ToolCall {
            tool_id: tool.into(),
            call_id: "background-policy".into(),
            arguments: serde_json::json!({}),
        };
        let baseline = server_policy(&[]);
        for id in awaken_ext_background_task::management_tool_ids() {
            assert!(
                matches!(
                    baseline.evaluate(&call(id)).await.verdict,
                    ToolPermissionVerdict::Allow
                ),
                "B1 {id}"
            );
        }
        assert!(
            matches!(
                baseline
                    .evaluate(&call("mcp__phone__StartCall"))
                    .await
                    .verdict,
                ToolPermissionVerdict::RequireConfirmation { .. }
            ),
            "B2"
        );
        let denied = effective_ruleset(
            Some(PermissionRuleset {
                rules: vec![PermissionRule::new(
                    ToolCallPattern::parse("run_in_background").unwrap(),
                    ToolPermissionBehavior::Deny,
                )],
                ..Default::default()
            }),
            &[],
        );
        let policy = RuleBasedToolPermissionPolicy::new(denied);
        assert!(
            matches!(
                policy.evaluate(&call("run_in_background")).await.verdict,
                ToolPermissionVerdict::Deny { .. }
            ),
            "B3"
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
        let runtime = build_runtime(Arc::new(NoLlm), &sandbox, None);
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
