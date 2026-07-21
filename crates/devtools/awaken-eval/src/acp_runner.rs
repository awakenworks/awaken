//! Shared tool-free text runner for live auxiliary-agent evaluation over ACP.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_run_executor_acp::{AcpLaunch, AcpRunExecutor, Codec, SubprocessChannelSource};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::permission::{ToolCall, ToolPermissionPolicy, ToolPermissionVerdict};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

struct DenyAllTools;

#[async_trait]
impl ToolPermissionPolicy for DenyAllTools {
    async fn evaluate(&self, _call: &ToolCall) -> ToolPermissionVerdict {
        ToolPermissionVerdict::Deny {
            reason: "evaluation run is tool-free".into(),
        }
    }
}

/// Production ACP adapter configured for isolated, read-only auxiliary runs.
pub struct ToolFreeAcpRunner {
    executor: AcpRunExecutor,
}

impl ToolFreeAcpRunner {
    #[must_use]
    pub fn new(argv: Vec<String>, env: Vec<(String, String)>) -> Self {
        let launch = AcpLaunch::custom(argv, env);
        let source = Arc::new(SubprocessChannelSource::new(launch).with_codec(Codec::Acp));
        Self {
            executor: AcpRunExecutor::new(source).with_session_mode("read-only"),
        }
    }

    /// Run one tool-free Agent using its real system instructions and user input.
    pub async fn run(
        &self,
        namespace: &str,
        sequence: usize,
        instructions: &str,
        input: String,
        max_steps: usize,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let fingerprint = CatalogFingerprint(format!("{namespace}-acp-v1"));
        let run_id = RunId(format!("{namespace}-acp-run-{sequence}"));
        let thread_id = ThreadId(format!("{namespace}-acp-thread-{sequence}"));
        let activation = RunActivation {
            run_id,
            thread_id: thread_id.clone(),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId(format!("{namespace}-acp-v1")),
                metadata: Default::default(),
                root_agent_id: AgentId(format!("{namespace}-agent")),
                resolved_spec: ResolvedSpec {
                    catalog_fingerprint: fingerprint.clone(),
                    instructions: instructions.into(),
                    max_steps,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("eval", "weakest", "acp"),
                    model_candidates: Vec::new(),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: ContextPolicy::KeepAll,
                    tool_presentation: Default::default(),
                },
                fingerprint,
            },
            input: vec![Message {
                id: MessageId(format!("{namespace}-acp-input-{sequence}")),
                role: Role::User,
                content: vec![ContentBlock::text(input)],
            }],
            delegation_origin: None,
            model_ref_override: None,
        };
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let context = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_tool_permission_policy(Arc::new(DenyAllTools));
        let state = self.executor.execute(activation, context).await?;
        if state != RunState::Ended(EndCause::NaturalEnd) {
            return Err(format!("ACP evaluation run ended in {state:?}").into());
        }
        commit
            .committed_messages(&thread_id)
            .into_iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| message.text_content())
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| "ACP evaluation returned no assistant output".into())
    }
}
