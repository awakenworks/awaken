use super::policy::{is_scope_allowed, SCOPE_ALLOWED_AGENTS_KEY, SCOPE_EXCLUDED_AGENTS_KEY};
use super::{AgentOs, AgentRegistry};
use crate::contracts::runtime::tool_call::{
    Tool, ToolDescriptor, ToolResult, TOOL_SCOPE_PARENT_TOOL_CALL_ID_KEY,
};
use crate::contracts::thread::{Message, Role, ToolCall};
use crate::contracts::{AgentEvent, Suspension};
#[cfg(feature = "permission")]
use tirea_extension_permission::ToolPermissionBehavior;

#[cfg(not(feature = "permission"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ToolPermissionBehavior {
    Allow,
    Ask,
    Deny,
}
pub(super) use crate::loop_runtime::loop_runner::TOOL_SCOPE_CALLER_AGENT_ID_KEY as SCOPE_CALLER_AGENT_ID_KEY;
use crate::loop_runtime::loop_runner::{
    RunCancellationToken, TOOL_SCOPE_CALLER_MESSAGES_KEY, TOOL_SCOPE_CALLER_STATE_KEY,
    TOOL_SCOPE_CALLER_THREAD_ID_KEY,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use types::{SubAgent, SubAgentAction, SubAgentState, SubAgentStatus};

const SCOPE_CALLER_SESSION_ID_KEY: &str = TOOL_SCOPE_CALLER_THREAD_ID_KEY;
const SCOPE_CALLER_STATE_KEY: &str = TOOL_SCOPE_CALLER_STATE_KEY;
const SCOPE_CALLER_MESSAGES_KEY: &str = TOOL_SCOPE_CALLER_MESSAGES_KEY;
const SCOPE_PARENT_TOOL_CALL_ID_KEY: &str = TOOL_SCOPE_PARENT_TOOL_CALL_ID_KEY;
const SCOPE_RUN_ID_KEY: &str = "run_id";
pub(crate) const AGENT_TOOLS_PLUGIN_ID: &str = "agent_tools";
pub(crate) const AGENT_RECOVERY_PLUGIN_ID: &str = "agent_recovery";
pub(crate) const AGENT_RUN_TOOL_ID: &str = "agent_run";
pub(crate) const AGENT_STOP_TOOL_ID: &str = "agent_stop";
pub(crate) const AGENT_OUTPUT_TOOL_ID: &str = "agent_output";
pub(crate) const AGENT_RECOVERY_INTERACTION_ACTION: &str = "recover_agent_run";
pub(crate) const AGENT_RECOVERY_INTERACTION_PREFIX: &str = "agent_recovery_";

fn collect_descendant_run_ids(
    children_by_parent: &HashMap<String, Vec<String>>,
    root_run_id: &str,
    include_root: bool,
) -> Vec<String> {
    let mut queue = VecDeque::from([root_run_id.to_string()]);
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if include_root || id != root_run_id {
            out.push(id.clone());
        }
        if let Some(children) = children_by_parent.get(&id) {
            for child_id in children {
                queue.push_back(child_id.clone());
            }
        }
    }
    out
}

mod manager;
mod plugins;
mod state;
mod tools;
mod types;

#[cfg(test)]
use manager::SubAgentCompletion;
use manager::{execute_sub_agent, SubAgentExecutionRequest};
pub(crate) use manager::{SubAgentHandleTable, SubAgentSummary};
pub(super) use plugins::{AgentRecoveryPlugin, AgentToolsPlugin};
use state::*;
pub(super) use tools::{AgentOutputTool, AgentRunTool, AgentStopTool};

#[cfg(test)]
mod tests;
