//! Official builtin tool descriptors.
//!
//! Concrete model-callable tool ids live here, not in `awaken-runtime`.

use awaken_runtime_contract::resolved::ToolDescriptor;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Toolset {
    Hand,
    Task,
    Delegation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltinTool {
    pub toolset: Toolset,
    pub descriptor: ToolDescriptor,
}

pub fn builtin_tools() -> Vec<BuiltinTool> {
    vec![
        hand_tool("bash"),
        hand_tool("read"),
        hand_tool("write"),
        hand_tool("edit"),
        hand_tool("glob"),
        hand_tool("grep"),
        hand_tool("web_fetch"),
        hand_tool("web_search"),
        task_tool("send_message"),
        task_tool("cancel_task"),
        task_tool("recover_failed_messages"),
        BuiltinTool {
            toolset: Toolset::Delegation,
            descriptor: ToolDescriptor {
                id: "agent_run".to_string(),
                content_hash: "builtin:delegation:agent_run:v1".to_string(),
            },
        },
    ]
}

fn hand_tool(id: &str) -> BuiltinTool {
    BuiltinTool {
        toolset: Toolset::Hand,
        descriptor: ToolDescriptor {
            id: id.to_string(),
            content_hash: format!("builtin:hand:{id}:v1"),
        },
    }
}

fn task_tool(id: &str) -> BuiltinTool {
    BuiltinTool {
        toolset: Toolset::Task,
        descriptor: ToolDescriptor {
            id: id.to_string(),
            content_hash: format!("builtin:task:{id}:v1"),
        },
    }
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
}
