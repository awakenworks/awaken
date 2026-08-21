//! Official builtin tools.
//!
//! Concrete model-callable tool ids live here, not in `awaken-runtime`. The
//! extension owns both the descriptors and their in-process implementations
//! (ADR-0007): typed [`Tool`]s erased into
//! the runtime's `RawTool` registry.

mod agent;
mod erasure;
mod hand;
mod task;
mod web;

pub use erasure::{Erased, erase};
pub use hand::{
    BashArgs, BashTool, DeleteArgs, DeleteTool, EditArgs, EditTool, GlobArgs, GlobTool, GrepArgs,
    GrepTool, HandToolContext, MoveArgs, MoveTool, ReadArgs, ReadTool, WriteArgs, WriteTool,
    executable_hand_tools, executable_hand_tools_in,
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
    all_hand_tools_in(HandToolContext::default())
}

pub fn all_hand_tools_in(
    context: HandToolContext,
) -> Vec<std::sync::Arc<dyn awaken_runtime_contract::tool::RawTool>> {
    executable_hand_tools_in(context)
        .into_iter()
        .chain(web_hand_tools())
        .collect()
}

use awaken_runtime_contract::resolved::{ToolDescriptor, ToolKind};
use awaken_runtime_contract::tool::{Tool, ToolRecoveryMode, ToolRecoveryPolicy};
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
        .map(|tool| tool.recovery_policy.mode())
        .collect()
}

pub fn builtin_tools() -> Vec<BuiltinTool> {
    vec![
        hand_tool::<BashTool>(),
        hand_tool::<ReadTool>(),
        hand_tool::<WriteTool>(),
        hand_tool::<EditTool>(),
        hand_tool::<MoveTool>(),
        hand_tool::<DeleteTool>(),
        hand_tool::<GlobTool>(),
        hand_tool::<GrepTool>(),
        hand_tool::<WebFetchTool>(),
        task_tool_with_recovery::<SendMessageTool>(ToolRecoveryPolicy::durable_request()),
        task_tool::<CancelTaskTool>(),
        task_tool::<RecoverFailedMessagesTool>(),
        BuiltinTool {
            toolset: Toolset::Delegation,
            descriptor: ToolDescriptor::for_args::<AgentRunArgs>(
                "builtin:delegation",
                AGENT_RUN,
                "Delegate a Run to another Agent",
            )
            .with_kind(ToolKind::AgentDelegation)
            .with_recovery(ToolRecoveryPolicy::durable_request()),
        },
    ]
}

fn hand_tool<T: Tool>() -> BuiltinTool {
    BuiltinTool {
        toolset: Toolset::Hand,
        descriptor: ToolDescriptor::for_tool::<T>("builtin:hand"),
    }
}

fn task_tool<T: Tool>() -> BuiltinTool {
    BuiltinTool {
        toolset: Toolset::Task,
        descriptor: ToolDescriptor::for_tool::<T>("builtin:task"),
    }
}

fn task_tool_with_recovery<T: Tool>(recovery: ToolRecoveryPolicy) -> BuiltinTool {
    let mut tool = task_tool::<T>();
    tool.descriptor = tool.descriptor.with_recovery(recovery);
    tool
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRunArgs {
    /// Target Agent ID from the roster.
    pub agent_id: String,
    /// Task for the target Agent.
    pub input: String,
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
            assert_eq!(
                d.parameters["additionalProperties"], false,
                "{} must reject undeclared arguments",
                d.id
            );
            assert!(
                d.parameters.get("$schema").is_none()
                    && d.parameters.get("$defs").is_none()
                    && d.parameters.get("definitions").is_none(),
                "{} must use the portable, inline model-tool dialect",
                d.id
            );
            // The hash is derived from the schema surface, so it ends in a hex digest.
            assert!(
                d.content_hash().contains(&d.id),
                "{} hash names the id",
                d.id
            );
            assert_ne!(d.content_hash(), format!("builtin:hand:{}:v1", d.id));
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
