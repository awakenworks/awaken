//! Host adapter for Memory recall selection through the ordinary auxiliary-Agent path.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_builtin_tools::{AuxiliaryAgentInput, invoke_auxiliary_agent};
use awaken_ext_memory::{RecallSelector, parse_indices, select_input};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::judge::AuxAgentTool;
use crate::store::HostCommit;

/// A [`RecallSelector`] backed by the `memory-selector` sub-agent: a single-step,
/// tool-free, plugin-free run driven through the same ordinary Agent-backed tool
/// used by the compactor. Its configuration activates no plugins, so recall
/// cannot recursively invoke itself; housekeeping usage remains outside the
/// user Session's accounting projection.
pub(crate) struct AgentSelector {
    agent_tool: Arc<dyn RawTool>,
    agent_id: String,
}

impl AgentSelector {
    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
        execution: Arc<HostCommit>,
    ) -> Self {
        let agent_id = snapshot.root_agent_id.0.clone();
        let catalog = Arc::new(AgentCatalog::new().with_agent(snapshot));
        let base = std::env::temp_dir()
            .join("awaken-coordinator")
            .join(format!("{}-mem-select", std::process::id()));
        Self {
            agent_tool: Arc::new(AuxAgentTool {
                llm,
                provider: LocalProvider::new(base),
                catalog,
                execution,
            }),
            agent_id,
        }
    }
}

#[async_trait]
impl RecallSelector for AgentSelector {
    async fn select(&self, query: &str, manifest: &[(usize, String)], max: usize) -> Vec<usize> {
        let input = select_input(query, manifest, max);
        // A runner error degrades to "select nothing" rather than failing the
        // foreground Run. The port surfaces only reply content.
        let reply = invoke_auxiliary_agent(
            self.agent_tool.as_ref(),
            "memory-selector-agent-run",
            AuxiliaryAgentInput {
                agent_id: self.agent_id.clone(),
                seed: vec![Message {
                    id: MessageId("mem-select".into()),
                    role: Role::User,
                    content: vec![ContentBlock::text(input)],
                }],
            },
            None,
        )
        .await
        .ok()
        .filter(|output| !output.is_error)
        .map(|output| output.content)
        .unwrap_or_default();
        parse_indices(&extract_text(&reply), manifest.len(), max)
    }
}
