//! Official builtin tools.
//!
//! Concrete model-callable tool ids live here, not in `awaken-runtime`. The
//! extension owns both the descriptors and their in-process implementations
//! (ADR-0007): typed [`Tool`](awaken_runtime_contract::tool::Tool)s erased into
//! the runtime's `RawTool` registry.

mod erasure;
mod hand;

pub use erasure::{Erased, erase};
pub use hand::{
    BashArgs, BashTool, EditArgs, EditTool, GlobArgs, GlobTool, GrepArgs, GrepTool, ReadArgs,
    ReadTool, WriteArgs, WriteTool, executable_hand_tools,
};

use awaken_runtime_contract::resolved::ToolDescriptor;
use serde::{Deserialize, Serialize};

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

pub fn builtin_tools() -> Vec<BuiltinTool> {
    vec![
        hand_tool(
            "bash",
            "Run a shell command",
            path_arg("command", "shell command to run"),
        ),
        hand_tool(
            "read",
            "Read a file",
            path_arg("path", "absolute file path to read"),
        ),
        hand_tool("write", "Write a file", write_args()),
        hand_tool("edit", "Edit a file by replacing text", edit_args()),
        hand_tool(
            "glob",
            "Find files matching a glob",
            path_arg("pattern", "glob pattern"),
        ),
        hand_tool(
            "grep",
            "Search file contents",
            path_arg("pattern", "regular expression"),
        ),
        hand_tool("web_fetch", "Fetch a URL", path_arg("url", "URL to fetch")),
        hand_tool(
            "web_search",
            "Search the web",
            path_arg("query", "search query"),
        ),
        task_tool(
            "send_message",
            "Send a message",
            path_arg("content", "message body"),
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
                "agent_run",
                "Delegate a sub-run to another agent",
                agent_run_args(),
            ),
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
            "path": { "type": "string", "description": "absolute file path" },
            "content": { "type": "string", "description": "file content" },
        },
        "required": ["path", "content"],
    })
}

fn edit_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "old": { "type": "string" },
            "new": { "type": "string" },
        },
        "required": ["path", "old", "new"],
    })
}

fn agent_run_args() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "string", "description": "target agent id from the roster" },
            "input": { "type": "string", "description": "task for the sub-agent" },
        },
        "required": ["agent_id", "input"],
    })
}

#[cfg(test)]
mod tests {
    use super::{Toolset, builtin_tools};

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
}
