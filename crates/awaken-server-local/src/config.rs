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
use awaken_runtime::{PermissionGate, Runtime};
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
) -> RunnableConfig {
    let mut tools = hand_tool_descriptors();
    tools.extend(client_tools.iter().map(|id| client_tool_descriptor(id)));
    if !delegates.is_empty() {
        tools.push(delegation_descriptor());
    }
    RunnableConfig::builder("assistant")
        .instructions(SYSTEM_PROMPT)
        .model(ModelBinding::new("default", model_ref, "default"))
        .tools(tools)
        .max_steps(20)
        .plugins(plugin_ids.iter().cloned())
        .build()
}

/// A per-thread runtime whose hand tools come from `env` (placement-agnostic). No
/// `agent_run` executor is registered: a delegate call is advertised by the config
/// but the kernel runs it via the injected resolver, not the tool registry.
pub(crate) fn build_runtime(llm: Arc<dyn LlmExecutor>, env: &Environment) -> Runtime {
    let gate = PermissionGate::new(Arc::new(server_policy()));
    let mut runtime = Runtime::new().with_llm(llm).with_gate(Arc::new(gate));
    for tool in env.hand_tools() {
        runtime = runtime.with_tool(tool);
    }
    runtime
}
