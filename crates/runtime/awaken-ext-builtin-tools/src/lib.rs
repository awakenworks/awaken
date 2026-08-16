//! Official builtin tools.
//!
//! Concrete model-callable tool ids live here, not in `awaken-runtime`. The
//! extension owns both the descriptors and their in-process implementations
//! (ADR-0007): typed [`Tool`](awaken_runtime_contract::tool::Tool)s erased into
//! the runtime's `RawTool` registry.

mod agent;
mod erasure;
mod hand;
mod task;
mod web;

pub use erasure::{Erased, erase};
pub use hand::{
    BashArgs, BashTool, DeleteArgs, DeleteTool, EditArgs, EditTool, GlobArgs, GlobTool, GrepArgs,
    GrepTool, MoveArgs, MoveTool, ReadArgs, ReadTool, WriteArgs, WriteTool, executable_hand_tools,
};
pub use task::{
    CancelTaskArgs, CancelTaskTool, MessageRecovery, MessageSendRequest, MessageSender,
    RecoverFailedMessagesArgs, RecoverFailedMessagesTool, SendMessageArgs, SendMessageTool,
    TaskCanceller, task_tools,
};
pub use web::{
    BRAVE_PROVIDER_ID, BraveSearchProvider, DUCKDUCKGO_PROVIDER_ID, DuckDuckGoProvider,
    WEB_SEARCH_PLUGIN_ID, WEB_SEARCH_TOOL_ID, WebFetchArgs, WebFetchTool, WebSearchArgs,
    WebSearchConfig, WebSearchCredentialRequirement, WebSearchCredentialResolver, WebSearchPlugin,
    WebSearchProvider, WebSearchProviderDescriptor, WebSearchProviderRegistry,
    WebSearchRegistryError, WebSearchRequest, WebSearchResult, WebSearchTool, web_hand_tools,
    web_search_descriptor,
};

/// The one complete static Hand registry used by every SessionEnvironment.
/// `web_search` is deliberately absent because its configured plugin is the
/// sole execution owner; all other built-in Sandbox tools live here.
pub fn all_hand_tools() -> Vec<std::sync::Arc<dyn awaken_runtime_contract::tool::RawTool>> {
    executable_hand_tools()
        .into_iter()
        .chain(web_hand_tools())
        .collect()
}

use awaken_runtime_contract::resolved::{ToolDescriptor, ToolKind};
use awaken_runtime_contract::tool::{ToolRecoveryMode, ToolRecoveryPolicy};
use serde::{Deserialize, Serialize};

/// The delegation tool id. The model-visible descriptor and the runtime resolver
/// that backs it (`RunDelegationService::tool_id`) must agree on this one value.
pub const AGENT_RUN: &str = "agent_run";
/// Internal ordinary-tool identity for housekeeping Agent capabilities. It is
/// deliberately distinct from the model-visible delegation contract.
pub const AUXILIARY_AGENT: &str = "auxiliary_agent";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Toolset {
    Hand,
    Task,
    Delegation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuiltinTool {
    pub toolset: Toolset,
    pub descriptor: ToolDescriptor,
}

/// Recovery modes frozen on the selected canonical Hand descriptors. This is
/// the one Hand-catalog membership rule shared by publication admission and
/// Worker placement; callers remain responsible for their own policy decision.
pub fn selected_hand_recovery_modes(
    descriptors: &[ToolDescriptor],
) -> std::collections::BTreeSet<ToolRecoveryMode> {
    let hand_ids = builtin_tools()
        .into_iter()
        .filter(|tool| tool.toolset == Toolset::Hand)
        .map(|tool| tool.descriptor.id)
        .collect::<std::collections::BTreeSet<_>>();
    descriptors
        .iter()
        .filter(|tool| hand_ids.contains(&tool.id))
        .map(|tool| tool.recovery_policy.mode)
        .collect()
}

pub fn builtin_tools() -> Vec<BuiltinTool> {
    vec![
        hand_tool("bash", "Run a shell command", bash_args()),
        hand_tool("read", "Read a file", read_args()),
        hand_tool("write", "Write a file", write_args()),
        hand_tool("edit", "Edit a file by replacing text", edit_args()),
        hand_tool("move", "Move or rename a file", move_args()),
        hand_tool(
            "delete",
            "Delete one file",
            path_arg("path", "absolute file path to delete"),
        ),
        hand_tool("glob", "Find files matching a glob", glob_args()),
        hand_tool("grep", "Search file contents", grep_args()),
        hand_tool("web_fetch", "Fetch a URL", path_arg("url", "URL to fetch")),
        task_tool_with_recovery(
            "send_message",
            "Send a message to another thread",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "target_thread": { "type": "string", "description": "id of the thread to message" },
                    "content": { "type": "string", "description": "message body" },
                    "idempotency_key": { "type": "string", "description": "optional caller key, scoped to the sending run" },
                },
                "required": ["target_thread", "content"],
            }),
            ToolRecoveryPolicy::durable_request(),
        ),
        task_tool(
            "cancel_task",
            "Cancel a background task",
            path_arg("task_id", "task identifier"),
        ),
        task_tool(
            "recover_failed_messages",
            "Recover failed messages (operator-scoped)",
            no_args(),
        ),
        BuiltinTool {
            toolset: Toolset::Delegation,
            descriptor: ToolDescriptor::pinned(
                "builtin:delegation",
                AGENT_RUN,
                "Delegate a Run to another Agent",
                agent_run_args(),
            )
            .with_kind(ToolKind::AgentDelegation)
            .with_recovery(ToolRecoveryPolicy::durable_request()),
        },
    ]
}

fn hand_tool(id: &str, description: &str, parameters: serde_json::Value) -> BuiltinTool {
    BuiltinTool {
        toolset: Toolset::Hand,
        descriptor: ToolDescriptor::pinned("builtin:hand", id, description, parameters),
    }
}

fn task_tool(id: &str, description: &str, parameters: serde_json::Value) -> BuiltinTool {
    BuiltinTool {
        toolset: Toolset::Task,
        descriptor: ToolDescriptor::pinned("builtin:task", id, description, parameters),
    }
}

fn task_tool_with_recovery(
    id: &str,
    description: &str,
    parameters: serde_json::Value,
    recovery: ToolRecoveryPolicy,
) -> BuiltinTool {
    let mut tool = task_tool(id, description, parameters);
    tool.descriptor = tool.descriptor.with_recovery(recovery);
    tool
}

/// A one-required-string-parameter JSON Schema.
fn path_arg(name: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { name: { "type": "string", "description": description } },
        "required": [name],
    })
}

fn no_args() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

fn write_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string", "description": "path of the file to write" },
            "content": { "type": "string", "description": "file content" },
        },
        "required": ["file_path", "content"],
    })
}

fn edit_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string" },
            "old_string": { "type": "string" },
            "new_string": { "type": "string" },
            "replace_all": { "type": "boolean" },
        },
        "required": ["file_path", "old_string", "new_string"],
    })
}

fn grep_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "regular expression" },
            "path": { "type": "string", "description": "optional directory root to search under" },
        },
        "required": ["pattern"],
    })
}

fn bash_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "shell command to execute" },
            "restart": { "type": "boolean", "description": "restart the runner-side bash session" },
            "timeout_ms": { "type": "integer", "minimum": 0 },
        },
    })
}

fn read_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string", "description": "path of the file to read" },
            "view_range": {
                "type": "array",
                "items": { "type": "integer" },
                "minItems": 2,
                "maxItems": 2
            },
        },
        "required": ["file_path"],
    })
}

fn glob_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "doublestar glob pattern" },
            "path": { "type": "string", "description": "optional directory root" },
        },
        "required": ["pattern"],
    })
}

fn move_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "description": "absolute source file path" },
            "destination": { "type": "string", "description": "absolute destination file path" },
        },
        "required": ["source", "destination"],
    })
}

fn agent_run_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "string", "description": "target agent id from the roster" },
            "input": { "type": "string", "description": "task for the target Agent" },
        },
        "required": ["agent_id", "input"],
    })
}

pub use agent::{AuxiliaryAgentInput, invoke_auxiliary_agent};

#[cfg(test)]
mod tests {
    use super::{Toolset, builtin_tools, selected_hand_recovery_modes};
    use awaken_runtime_contract::tool::{ToolRecoveryMode, ToolRecoveryPolicy};

    #[test]
    fn delegation_uses_one_stable_agent_run_tool_id() {
        let delegation_tools: Vec<_> = builtin_tools()
            .into_iter()
            .filter(|tool| tool.toolset == Toolset::Delegation)
            .collect();

        assert_eq!(delegation_tools.len(), 1);
        assert_eq!(delegation_tools[0].descriptor.id, "agent_run");
    }

    #[test]
    fn every_builtin_carries_a_schema_and_a_schema_derived_hash() {
        for tool in builtin_tools() {
            let d = &tool.descriptor;
            assert!(!d.description.is_empty(), "{} needs a description", d.id);
            assert_eq!(
                d.parameters["type"], "object",
                "{} needs an object schema",
                d.id
            );
            // The hash is derived from the schema surface, so it ends in a hex digest.
            assert!(d.content_hash.contains(&d.id), "{} hash names the id", d.id);
            assert_ne!(d.content_hash, format!("builtin:hand:{}:v1", d.id));
        }
    }

    #[test]
    fn unique_tool_ids() {
        let ids: Vec<_> = builtin_tools()
            .into_iter()
            .map(|t| t.descriptor.id)
            .collect();
        let mut deduped = ids.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(ids.len(), deduped.len(), "builtin tool ids must be unique");
    }

    #[test]
    fn selected_hand_recovery_uses_the_canonical_catalog_membership() {
        // Cause/effect decision table: C1=selected descriptor belongs to Hand;
        // C2=selected descriptor belongs to Task; C3=its policy is durable.
        // R1 C1+C3 => DurableRequest is projected; R2 C2+C3 => no Hand mode.
        // This single projection prevents Cloud and placement from maintaining
        // parallel concrete tool-id filters.
        let builtins = builtin_tools();
        let hand = builtins
            .iter()
            .find(|tool| tool.toolset == Toolset::Hand)
            .unwrap()
            .descriptor
            .clone()
            .with_recovery(ToolRecoveryPolicy::durable_request());
        let task = builtins
            .iter()
            .find(|tool| tool.toolset == Toolset::Task)
            .unwrap()
            .descriptor
            .clone()
            .with_recovery(ToolRecoveryPolicy::durable_request());
        assert_eq!(
            selected_hand_recovery_modes(&[hand]),
            [ToolRecoveryMode::DurableRequest].into_iter().collect(),
            "R1"
        );
        assert!(selected_hand_recovery_modes(&[task]).is_empty(), "R2");
    }
}
