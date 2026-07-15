//! Per-step durability: each completed step's messages/state/audit commit at
//! the step boundary under a `Running` fact, so a crash loses at most the step
//! in flight and readers see committed progress mid-run. The terminal step
//! still commits atomically with the final phase (and any ticket) through the
//! single finish boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
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
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

/// (step, committed message count, committed phase) captured inside a step.
type Observations = Arc<std::sync::Mutex<Vec<(usize, usize, Option<Phase>)>>>;

/// Emits `tool_steps` tool-call turns, then a final text turn.
struct ToolStepsThenEnd {
    tool_steps: usize,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ToolStepsThenEnd {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n < self.tool_steps {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: format!("call-{n}"),
                tool_id: "probe".to_string(),
                arguments: serde_json::json!({ "step": n }),
            }])
        } else {
            AssistantOutput::text("done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// Observes committed truth from inside a step: proof that the previous step
/// is durable (and the run is `Running`) while the run is still mid-flight.
struct ProbeTool {
    commit: Arc<MemoryCommitCoordinator>,
    observations: Observations,
    staged: Vec<StateCommand>,
}

#[async_trait::async_trait]
impl RawTool for ProbeTool {
    fn id(&self) -> &str {
        "probe"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let step = call.arguments["step"].as_u64().unwrap_or(0) as usize;
        let committed = self.commit.committed();
        self.observations.lock().unwrap().push((
            step,
            committed.messages.len(),
            committed.latest_run.map(|r| r.phase),
        ));
        let mut output = ToolOutput::ok(&call.call_id, "probed");
        output.state = self.staged.clone();
        Ok(output)
    }
}

fn install(runtime: &Runtime) {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: vec![ToolDescriptor::pinned(
                    "test",
                    "probe",
                    "Probe committed truth",
                    serde_json::json!({"type": "object"}),
                )],
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        model_access: Default::default(),
    }
}

fn runtime_with(
    tool_steps: usize,
    commit: &Arc<MemoryCommitCoordinator>,
    observations: &Observations,
    staged: Vec<StateCommand>,
) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(ToolStepsThenEnd {
            tool_steps,
            calls: AtomicUsize::new(0),
        }))
        .with_tool(Arc::new(ProbeTool {
            commit: commit.clone(),
            observations: observations.clone(),
            staged,
        }));
    install(&runtime);
    runtime
}

#[tokio::test]
async fn each_continuing_step_commits_under_a_running_fact() {
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runtime = runtime_with(2, &commit, &observations, Vec::new());

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

    // The input commits at the first step boundary (the run is durable as
    // Running before any inference), the two tool steps commit at theirs,
    // and the terminal text step commits through finish: four commits.
    assert_eq!(commit.commit_count(), 4);

    let committed = commit.committed();
    // The durable phase history walked Running → Running → Ended.
    let phases: Vec<Phase> = committed
        .run_facts
        .iter()
        .map(|f| f.phase.clone())
        .collect();
    assert_eq!(
        phases,
        vec![
            Phase::Running,
            Phase::Running,
            Phase::Running,
            Phase::Ended(EndCause::NaturalEnd)
        ]
    );
    // The transition into Running is recorded exactly once, before the
    // terminal phase event.
    let phase_events: Vec<String> = committed
        .events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::RunPhaseChanged))
        .map(|e| e.payload["phase"].to_string())
        .collect();
    assert_eq!(phase_events.len(), 2, "one Running, one terminal");
    assert!(phase_events[0].contains("Running"));
    assert!(phase_events[1].contains("Ended"));
}

#[tokio::test]
async fn committed_progress_is_visible_mid_run() {
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runtime = runtime_with(2, &commit, &observations, Vec::new());

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    runtime.execute(activation(), context).await.expect("runs");

    let observations = observations.lock().unwrap();
    // Already during step 0 the run's input is durable and the run reads as
    // Running: execution never begins without a durable trace.
    let (_, visible_at_0, ref phase_at_0) = observations[0];
    assert_eq!(
        visible_at_0, 1,
        "the input is durable during the first step"
    );
    assert_eq!(*phase_at_0, Some(Phase::Running));
    // By the time step 1's tool runs, step 0 (assistant turn + tool result)
    // is durable too.
    let (_, visible_at_1, ref phase_at_1) = observations[1];
    assert!(
        visible_at_1 >= 3,
        "step 0's messages are durable during step 1 (saw {visible_at_1})"
    );
    assert_eq!(*phase_at_1, Some(Phase::Running));
}

#[tokio::test]
async fn cross_step_state_conflict_ends_the_run_and_keeps_committed_steps() {
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    // Every tool step stages the same exclusive set: step 0's commits fine,
    // step 1's collides with it in the cumulative batch.
    let exclusive = vec![StateCommand::set(
        Scope::Run,
        MergePolicy::Exclusive,
        "lock",
        serde_json::json!(1),
    )];
    let runtime = runtime_with(3, &commit, &observations, exclusive);

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(
        outcome,
        Phase::Ended(EndCause::Error(Failure::StateConflict))
    );
    let committed = commit.committed();
    // Step 0 was valid when it committed and stays committed; the conflicting
    // tail was dropped, so exactly one copy of the exclusive set is durable.
    assert_eq!(committed.state.len(), 1);
    assert_eq!(
        committed.latest_run.unwrap().phase,
        Phase::Ended(EndCause::Error(Failure::StateConflict))
    );
}

#[tokio::test]
async fn text_only_run_commits_input_then_terminal() {
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runtime = runtime_with(0, &commit, &observations, Vec::new());

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    // A text-only run: the input commits at the first step boundary (durable
    // Running before inference), then the terminal turn commits via finish.
    assert_eq!(commit.commit_count(), 2);
}
