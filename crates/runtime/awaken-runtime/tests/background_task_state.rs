//! BackgroundTask integration at the Runtime commit boundary.
//!
//! The extension has no service/repository seam. These tests therefore exercise
//! the production agent loop and inspect the ordinary committed Thread state.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Scope, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_background_task::{
    BACKGROUND_TASK_PLUGIN_ID, BACKGROUND_TASK_STATE_PREFIX, BackgroundTask, BackgroundTaskConfig,
    BackgroundTaskLifecycle, BackgroundTaskPlugin,
};
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolConcurrency, ToolOutput, ToolRecoveryCapability, ToolRecoveryPolicy, ToolResource,
    ToolResourceAccess,
};
use awaken_store_inmem::MemoryCommitCoordinator;

struct Bash;

#[async_trait]
impl RawTool for Bash {
    fn id(&self) -> &str {
        "bash"
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::ReplaySafe
    }

    fn concurrency(&self, _arguments: &serde_json::Value) -> ToolConcurrency {
        ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
            "sandbox", "workdir",
        ))])
    }

    async fn invoke(
        &self,
        call: ToolCall,
    ) -> Result<ToolOutput, awaken_runtime_contract::tool::ToolError> {
        Ok(ToolOutput::ok(call.call_id, "unused by submission test"))
    }
}

struct SubmitThenEnd {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmExecutor for SubmitThenEnd {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            let ids = request
                .tools
                .iter()
                .map(|tool| tool.id.as_str())
                .collect::<BTreeSet<_>>();
            assert!(ids.contains("run_in_background"), "C1/E1");
            assert!(ids.contains("list_background_tasks"), "C1/E1");
        }
        Ok(ChatResponse {
            output: if first {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "submit-call".into(),
                    tool_id: "run_in_background".into(),
                    arguments: serde_json::json!({
                        "tool": "bash",
                        "arguments": {"command": "sleep 1"}
                    }),
                }])
            } else {
                AssistantOutput::text("done")
            },
            usage: None,
            stop_reason: None,
        })
    }
}

fn activation(plugin_ids: Vec<String>) -> RunActivation {
    let fingerprint = CatalogFingerprint("background-catalog".into());
    RunActivation {
        run_id: RunId("run-1".into()),
        thread_id: ThreadId("thread-1".into()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 4,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "provider".into(),
                        model_ref: "model".into(),
                        backend_ref: "backend".into(),
                    },
                ),
                tool_descriptors: vec![
                    ToolDescriptor::pinned(
                        "test",
                        "bash",
                        "Run a command.",
                        serde_json::json!({"type":"object"}),
                    )
                    .with_recovery(ToolRecoveryPolicy::replay_safe()),
                ],
                plugin_ids,
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("input-1".into()),
            role: Role::User,
            content: vec![ContentBlock::text("run it")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

#[tokio::test]
async fn runtime_commits_submission_through_the_ordinary_thread_state_boundary() {
    // Causal graph:
    // C1 configured plugin -> four tools are projected;
    // C2 model submits -> RawTool returns one typed State command;
    // C3 Runtime completes its normal ToolBatch/ThreadCommit -> the task is
    // replayable from committed Thread state. There is no background service,
    // repository mock, database handle, or Runtime special operation in the graph.
    let plugin = Arc::new(BackgroundTaskPlugin::new(BackgroundTaskConfig {
        tools: BTreeSet::from(["bash".into()]),
    }));
    let runtime = Runtime::new()
        .with_llm(Arc::new(SubmitThenEnd {
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(plugin)
        .with_tool(Arc::new(Bash));
    let commits = Arc::new(MemoryCommitCoordinator::new());
    let outcome = runtime
        .execute(
            activation(vec![BACKGROUND_TASK_PLUGIN_ID.into()]),
            RuntimeRunContext::new().with_commit(commits.clone()),
        )
        .await
        .expect("the state-only plugin must run through the ordinary loop");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "C3/E1");

    let committed = commits.committed();
    let state = Store::rebuild(&committed.state);
    let tasks = state
        .scan_prefix(Scope::Thread, BACKGROUND_TASK_STATE_PREFIX)
        .map(|(_, value)| {
            serde_json::from_value::<BackgroundTask>(value.clone())
                .expect("committed task state must retain its typed shape")
        })
        .collect::<Vec<_>>();
    assert_eq!(tasks.len(), 1, "C2/E1: exactly one durable submission");
    assert!(matches!(
        tasks[0].lifecycle,
        BackgroundTaskLifecycle::Running { .. }
    ));
    assert_eq!(tasks[0].origin.thread_id, ThreadId("thread-1".into()));
    let attempt = tasks[0]
        .attempt()
        .expect("submission freezes execution facts");
    assert_eq!(attempt.policy.recovery, ToolRecoveryPolicy::replay_safe());
    assert_eq!(
        attempt.policy.concurrency,
        ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
            "sandbox", "workdir"
        ))])
    );
}

#[test]
fn an_unconfigured_plugin_is_inert() {
    // Boundary partition: registration is not enablement. Empty external
    // configuration contributes no tool and no state namespace, so a product
    // may register the extension globally without advertising unavailable work.
    use awaken_runtime_contract::plugin::Plugin;

    let contributions = BackgroundTaskPlugin::new(Default::default()).resolve();
    assert!(contributions.dynamic_tools.is_empty());
    assert!(contributions.state_keys.is_empty());
}
