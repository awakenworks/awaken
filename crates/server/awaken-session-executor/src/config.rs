//! Assembling a runtime and its config, and reading a run's output.
//!
//! The "how to build a run" concern, kept out of the session substrate: the
//! permission policy, the advertised tool descriptors, the `RunnableConfig`, and
//! the per-thread `Runtime`. The session substrate (`host`) and the sub-agent
//! helper (`subagent`) both depend on this leaf rather than each other.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_ext_builtin_tools::{Toolset, builtin_tools, executable_hand_tools};
use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, RulePermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior,
};
use awaken_ext_state_machine::{STATE_MACHINE_PLUGIN_ID, StateMachinePlugin};
use awaken_runtime::{PermissionGate, Runtime};
use awaken_runtime_contract::capability::PluginCapability;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::{ContextPolicy, ModelBinding, ToolDescriptor};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_sandbox_local::LocalSandbox;

const SYSTEM_PROMPT: &str = "You are a helpful assistant working in a local repository.";

/// Concatenate the text of a content-block list.
pub fn block_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
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
    rules.push(allow("agent_run"));
    rules.push(allow("list_skills"));
    rules.push(allow("Skill"));
    rules.extend(extra_allowed.iter().map(|id| allow(id)));
    rules
}

fn server_policy(extra_allowed: &[String]) -> RulePermissionPolicy {
    RulePermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Ask,
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
        .unwrap_or((ToolPermissionBehavior::Ask, Mode::Default));
    if let Some(rs) = authored {
        rules.extend(rs.rules);
    }
    PermissionRuleset {
        default_behavior,
        mode,
        rules,
    }
}

/// The thread's authorization gate, built from [`effective_ruleset`].
pub(crate) fn server_gate_with(
    authored: Option<PermissionRuleset>,
    extra_allowed: &[String],
) -> Arc<dyn awaken_runtime_contract::permission::ToolGateHook> {
    Arc::new(PermissionGate::new(Arc::new(RulePermissionPolicy::new(
        effective_ruleset(authored, extra_allowed),
    ))))
}

fn hand_tool_descriptors() -> Vec<ToolDescriptor> {
    let registered: HashSet<String> = executable_hand_tools()
        .iter()
        .map(|t| t.id().to_string())
        .collect();
    builtin_tools()
        .into_iter()
        .filter(|t| t.toolset == Toolset::Hand && registered.contains(&t.descriptor.id))
        .map(|t| t.descriptor)
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
/// registered, so a call parks (gate `ask`) and the *client* supplies the result.
pub(crate) fn client_tool_descriptor(id: &str) -> ToolDescriptor {
    ToolDescriptor::pinned(
        "client",
        id,
        format!("Client-executed tool `{id}`; the caller runs it and returns the result."),
        serde_json::json!({ "type": "object" }),
    )
}

/// The `agent_run` delegation descriptor (advertised only when a roster is set).
fn delegation_descriptor() -> ToolDescriptor {
    builtin_tools()
        .into_iter()
        .find(|t| t.toolset == Toolset::Delegation)
        .map(|t| t.descriptor)
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

/// Ordered pool fallbacks for the server's agent, from `AWAKEN_MODEL_FALLBACKS`
/// (comma-separated model refs). Each is bound like the primary (`default`
/// provider/backend), so a run fails over to the next when its model is down (#1).
/// Empty when unset — a single-model server, unchanged behavior.
fn model_fallbacks() -> Vec<ModelBinding> {
    std::env::var("AWAKEN_MODEL_FALLBACKS")
        .ok()
        .into_iter()
        .flat_map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|m| ModelBinding::new("default", m, "default"))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn server_config(
    model_ref: &str,
    client_tools: &HashSet<String>,
    delegates: &HashSet<String>,
    plugin_ids: &[String],
    plugin_config: &std::collections::BTreeMap<String, serde_json::Value>,
    dynamic_descriptors: &[ToolDescriptor],
    context_policy: ContextPolicy,
) -> RunnableConfig {
    let tools = advertised_tools(client_tools, delegates, dynamic_descriptors);
    RunnableConfig::builder("assistant")
        .instructions(SYSTEM_PROMPT)
        .model(ModelBinding::new("default", model_ref, "default"))
        .model_candidates(model_fallbacks())
        .tools(tools)
        .max_steps(20)
        .plugins(plugin_ids.iter().cloned())
        .plugin_config(plugin_config.iter().map(|(k, v)| (k.clone(), v.clone())))
        .plugin_capabilities(platform_plugin_capabilities())
        .context_policy(context_policy)
        .build()
}

/// The plugins this server composes, advertised with their config schema so a
/// config frontend can discover and author each section. One place declares a
/// plugin's id and its schema, so registration and discovery cannot drift.
pub fn platform_plugin_capabilities() -> Vec<PluginCapability> {
    vec![
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
    ]
}

/// Every `plugin_config` section an author may set, WITH its JSON Schema: the
/// installable plugins PLUS the always-on `permission` policy. Permission is a
/// `plugin_config` section (not an installable plugin), so it is absent from
/// [`platform_plugin_capabilities`]; the assistant needs it advertised — with its
/// schema — or it cannot author a permission gate (it does not know the key/shape).
pub fn authorable_config_sections() -> Vec<PluginCapability> {
    let mut sections = platform_plugin_capabilities();
    sections.push(PluginCapability {
        id: PERMISSION_CONFIG_KEY.to_string(),
        schema_keys: vec![PERMISSION_CONFIG_KEY.to_string()],
        config_schema: Some(awaken_ext_permission::permission_config_schema()),
        bound: Default::default(),
    });
    sections
}

/// The server's base authorization gate (the declarative permission policy). A
/// composition-root helper so a caller can wrap it (e.g. to observe file paths for
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
pub(crate) fn build_runtime(llm: Arc<dyn LlmExecutor>, sandbox: &LocalSandbox) -> Runtime {
    let mut runtime = Runtime::new()
        .with_llm(llm)
        .with_gate(server_gate())
        // Structure-only metrics at the model/tool chokepoints (#2). Binds to the
        // global meter installed by `observability::init()`; a no-op when none is.
        .with_metrics(Arc::new(awaken_observability::OtelMetricsRecorder::new()))
        // The tool state machine is available on every runtime; an agent activates
        // it via `plugin_ids` and configures its machines via `plugin_config`.
        .with_plugin(Arc::new(StateMachinePlugin::empty()));
    // The full capability surface (ADR-0035 D8): hand tools plus provisioned skill
    // tools. Placement-agnostic — the kernel sees `RawTool`s, not "skills".
    for tool in sandbox.rooted_tools() {
        runtime = runtime.with_tool(tool);
    }
    runtime
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
            "m",
            &HashSet::new(),
            &HashSet::new(),
            &["state_machine".to_string()],
            &Default::default(),
            &[],
            ContextPolicy::KeepAll,
        );
        let mut spec = config.snapshot().resolved_spec.clone();
        spec.plugin_config
            .insert(STATE_MACHINE_PLUGIN_ID.to_string(), section);
        spec
    }

    #[test]
    fn server_config_advertises_the_state_machine_schema() {
        // A2/A4: the schema is discoverable in the installed catalog.
        let config = server_config(
            "m",
            &HashSet::new(),
            &HashSet::new(),
            &["state_machine".to_string()],
            &Default::default(),
            &[],
            ContextPolicy::KeepAll,
        );
        let sm = config
            .install()
            .capabilities
            .plugins
            .iter()
            .find(|p| p.id == STATE_MACHINE_PLUGIN_ID)
            .expect("state machine is advertised");
        assert!(sm.config_schema.is_some(), "config schema is discoverable");
    }

    #[test]
    fn platform_capabilities_expose_the_schema() {
        let caps = platform_plugin_capabilities();
        assert!(
            caps.iter()
                .any(|c| c.id == STATE_MACHINE_PLUGIN_ID && c.config_schema.is_some())
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
        // No authored policy → baseline: perception allowed, mutations asked.
        let base = effective_ruleset(None, &[]);
        assert_eq!(
            base.decide("read", &serde_json::json!({})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            base.decide("write", &serde_json::json!({})),
            ToolPermissionBehavior::Ask
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

    #[tokio::test]
    async fn build_runtime_registers_the_plugin_and_validates_config() {
        // A1: the composed runtime has the plugin (a valid section resolves).
        // A3: a malformed section fails closed at publish-time validation.
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = awaken_sandbox_local::LocalProvider::new(tmp.path())
            .create_sandbox(&crate::provisioning::subrun_sandbox_spec("t"))
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
