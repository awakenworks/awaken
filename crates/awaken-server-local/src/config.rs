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
use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_sandbox_local::Environment;

const SYSTEM_PROMPT: &str = "You are a helpful assistant working in a local repository.";

/// Concatenate the text of a content-block list.
pub(crate) fn block_text(content: &[ContentBlock]) -> String {
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

/// read/glob/grep allowed, mutations asked (ADR-0030). With `approval_mode:
/// human_approval` an asked tool parks for a confirmation.
fn server_policy() -> RulePermissionPolicy {
    let allow = |name: &str| {
        PermissionRule::new(
            ToolCallPattern::parse(name).expect("static pattern"),
            ToolPermissionBehavior::Allow,
        )
    };
    RulePermissionPolicy::new(PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Ask,
        mode: Mode::Default,
        // `agent_run` is allowed: the kernel executes it via the injected
        // delegation resolver (a sub-agent, native or remote), not the tool
        // registry — delegation is a runtime concern.
        rules: vec![
            allow("read"),
            allow("glob"),
            allow("grep"),
            allow("agent_run"),
            // Skill discovery/activation grant perception (they list metadata and
            // return instructions), not authorization; allow them without a
            // confirmation prompt. Any tool a skill then invokes is still gated.
            allow("list_skills"),
            allow("Skill"),
        ],
    })
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

/// A client-executed tool descriptor: model-visible, but no `RawTool` is
/// registered, so a call parks (gate `ask`) and the *client* supplies the result.
fn client_tool_descriptor(id: &str) -> ToolDescriptor {
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

pub(crate) fn server_config(
    model_ref: &str,
    client_tools: &HashSet<String>,
    delegates: &HashSet<String>,
    plugin_ids: &[String],
    skill_descriptors: &[ToolDescriptor],
) -> RunnableConfig {
    let mut tools = hand_tool_descriptors();
    tools.extend(client_tools.iter().map(|id| client_tool_descriptor(id)));
    // The single `Skill` tool descriptor (ADR-0036), when skills are offered: its
    // description carries the activatable-skill catalog, and the runtime registers
    // the matching `Skill` RawTool. Never a per-skill tool.
    tools.extend(skill_descriptors.iter().cloned());
    if !delegates.is_empty() {
        tools.push(delegation_descriptor());
    }
    RunnableConfig::builder("assistant")
        .instructions(SYSTEM_PROMPT)
        .model(ModelBinding::new("default", model_ref, "default"))
        .tools(tools)
        .max_steps(20)
        .plugins(plugin_ids.iter().cloned())
        .plugin_capabilities(platform_plugin_capabilities())
        .build()
}

/// The plugins this server composes, advertised with their config schema so a
/// config frontend can discover and author each section. One place declares a
/// plugin's id and its schema, so registration and discovery cannot drift.
pub(crate) fn platform_plugin_capabilities() -> Vec<PluginCapability> {
    vec![PluginCapability {
        id: STATE_MACHINE_PLUGIN_ID.to_string(),
        schema_keys: vec![STATE_MACHINE_PLUGIN_ID.to_string()],
        config_schema: Some(awaken_ext_state_machine::config_schema()),
    }]
}

/// The server's base authorization gate (the declarative permission policy). A
/// composition-root helper so a caller can wrap it (e.g. to observe file paths for
/// conditional skills) and re-inject it.
pub(crate) fn server_gate() -> Arc<dyn awaken_runtime_contract::permission::ToolGateHook> {
    Arc::new(PermissionGate::new(Arc::new(server_policy())))
}

/// A per-thread runtime whose hand tools come from `env` (placement-agnostic). No
/// `agent_run` executor is registered: a delegate call is advertised by the config
/// but the kernel runs it via the injected resolver, not the tool registry.
pub(crate) fn build_runtime(llm: Arc<dyn LlmExecutor>, env: &Environment) -> Runtime {
    let mut runtime = Runtime::new()
        .with_llm(llm)
        .with_gate(server_gate())
        // The tool state machine is available on every runtime; an agent activates
        // it via `plugin_ids` and configures its machines via `plugin_config`.
        .with_plugin(Arc::new(StateMachinePlugin::empty()));
    // The full capability surface (ADR-0035 D8): hand tools plus provisioned skill
    // tools. Placement-agnostic — the kernel sees `RawTool`s, not "skills".
    for tool in env.tools() {
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
            &[],
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
            &[],
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
    fn build_runtime_registers_the_plugin_and_validates_config() {
        // A1: the composed runtime has the plugin (a valid section resolves).
        // A3: a malformed section fails closed at publish-time validation.
        let runtime = build_runtime(Arc::new(NoLlm), &Environment::new("t", Vec::new()));
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
