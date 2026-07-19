//! Delegation (ADR-0044): a tool call whose id matches the executor's `tool_id` is
//! routed to the `DelegationExecutor` instead of the tool registry. An `Ended` step folds
//! the delegate's token usage into the parent thread and feeds its reply back; a
//! `Awaiting` step awaits the parent on a `Delegation` ticket carrying the opaque
//! handle; an `Err` feeds a model-visible error. A awaiting delegation resumes
//! through the resolver, which may finish, re-await, or fail (RD2/RD3/RD4).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::awaiting::AwaitReason;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::delegation::{
    DelegationExecutionError, DelegationExecutor, DelegationRequest, DelegationResume,
    DelegationStep,
};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ThreadUsage, TokenUsage, ToolCall,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

const FINGERPRINT: &str = "catalog-a";
const SNAPSHOT_ID: &str = "snapshot-1";
const DELEGATE_TOOL: &str = "agent_run";
const CALL_ID: &str = "d1";

/// The parent agent: delegates once (a call to `agent_run`), then ends with text —
/// so the delegate's folded result is observable in the next committed turn.
struct DelegateThenText {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for DelegateThenText {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: CALL_ID.to_string(),
                tool_id: DELEGATE_TOOL.to_string(),
                arguments: serde_json::json!({ "agent": "sub", "input": "hi" }),
            }])
        } else {
            AssistantOutput::text("parent done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// The delegate step a mock resolver should yield.
#[derive(Clone)]
enum Step {
    /// Finished with this reply and (optionally) this many tokens on `delegate-m`.
    Done(String, u64),
    /// Awaiting needing more input, carrying this opaque handle.
    Awaiting(serde_json::Value),
    /// Could not run.
    Fail(String),
}

fn step_to_result(step: &Step) -> Result<DelegationStep, DelegationExecutionError> {
    match step {
        Step::Done(text, tokens) => {
            let mut usage = ThreadUsage::default();
            if *tokens > 0 {
                usage.record(
                    "delegate-m",
                    TokenUsage {
                        prompt_tokens: *tokens,
                        completion_tokens: *tokens,
                        ..Default::default()
                    },
                );
            }
            Ok(DelegationStep::Ended {
                text: text.clone(),
                usage,
            })
        }
        Step::Awaiting(continuation) => Ok(DelegationStep::Awaiting {
            continuation: continuation.clone(),
        }),
        Step::Fail(err) => Err(DelegationExecutionError::new(err.clone())),
    }
}

/// A delegation resolver whose `run` and `resume` yield configured [`Step`]s, and
/// which records the `(handle, input)` each resume was called with.
struct MockResolver {
    run_step: Step,
    resume_step: Step,
    started: Mutex<Vec<DelegationOrigin>>,
    resumed_with: Mutex<Vec<(serde_json::Value, String)>>,
}

impl MockResolver {
    fn new(run_step: Step, resume_step: Step) -> Self {
        Self {
            run_step,
            resume_step,
            started: Mutex::new(Vec::new()),
            resumed_with: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl DelegationExecutor for MockResolver {
    fn tool_id(&self) -> &str {
        DELEGATE_TOOL
    }
    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.started.lock().unwrap().push(request.origin);
        step_to_result(&self.run_step)
    }
    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        self.resumed_with
            .lock()
            .unwrap()
            .push((request.continuation, request.input));
        step_to_result(&self.resume_step)
    }
}

fn snapshot() -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
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
            tool_descriptors: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: ContextPolicy::KeepAll,
            tool_presentation: Default::default(),
        },
        fingerprint,
    }
}

fn runtime(resolver: Arc<MockResolver>) -> Runtime {
    let runtime = Runtime::new()
        .with_llm(Arc::new(DelegateThenText {
            calls: AtomicUsize::new(0),
        }))
        .with_delegation_executor(resolver);
    let fingerprint = CatalogFingerprint(FINGERPRINT.to_string());
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
    runtime.register_snapshot(snapshot());
    runtime
}

fn activation() -> RunActivation {
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        model_ref_override: None,
    }
}

/// A resume command correlated to the `Delegation` ticket (its call id is the
/// correlation id), carrying the given user input.
fn resume_command(input: &str) -> ResumeCommand {
    ResumeCommand {
        correlation_id: CALL_ID.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAPSHOT_ID.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FINGERPRINT.to_string()),
        result: ResumeResult::Input(input.to_string()),
        now_ms: 0,
    }
}

// --- CE-4: dispatch of a delegation tool call ---

#[tokio::test]
async fn delegation_done_folds_delegate_usage_and_feeds_the_reply_back() {
    // T2: an allowed delegation that finishes feeds the delegate's reply back as the
    // tool result AND folds the delegate's own token spend into the parent thread's
    // running tally, so a session's usage counts delegated work.
    let resolver = Arc::new(MockResolver::new(
        Step::Done("delegate replied".to_string(), 5),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(resolver);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    // The delegate's reply came back as the tool result the parent then saw.
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("delegate replied")),
        "the delegate's reply is fed back as the tool result"
    );
    // The delegate's usage was folded into the parent thread's committed tally.
    let usage = ThreadUsage::from_committed_state(&committed.state);
    assert!(!usage.is_empty(), "the delegate's usage was recorded");
    assert_eq!(
        usage.total().prompt_tokens,
        5,
        "the parent thread's tally counts the delegate's tokens"
    );
}

#[tokio::test]
async fn a_child_run_delegates_with_the_next_depth_and_ordinary_runtime_path() {
    let resolver = Arc::new(MockResolver::new(
        Step::Done("grandchild replied".to_string(), 0),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(resolver.clone());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let parent_origin = DelegationOrigin::root(RunId("root-run".into()), "root-call");
    let context = RuntimeRunContext::new()
        .with_commit(commit)
        .for_delegated_child(parent_origin);

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let started = resolver.started.lock().unwrap();
    assert_eq!(started.len(), 1);
    let expected = DelegationOrigin::nested(RunId("run-1".into()), CALL_ID, 1).unwrap();
    assert_eq!(started[0], expected, "the child continues its Run lineage");
}

#[tokio::test]
async fn delegation_awaiting_awaits_the_parent_on_a_delegation_ticket() {
    // T3: a delegate that needs more input awaits the parent on a Delegation ticket
    // carrying the opaque resume handle.
    let handle = serde_json::json!({ "task": "remote-42" });
    let resolver = Arc::new(MockResolver::new(
        Step::Awaiting(handle.clone()),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(resolver);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Awaiting);

    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("a delegation ticket is committed");
    assert_eq!(ticket.reason, AwaitReason::Delegation);
    assert_eq!(ticket.call_id.as_deref(), Some(CALL_ID));
    assert_eq!(
        ticket
            .pending_tool
            .as_ref()
            .and_then(|p| p.resume_handle.clone()),
        Some(handle),
        "the ticket carries the delegate's opaque handle"
    );
}

#[tokio::test]
async fn delegation_error_feeds_a_model_visible_error_result() {
    // T4: a delegate that cannot run yields a model-visible error result; the run
    // continues rather than aborting.
    let resolver = Arc::new(MockResolver::new(
        Step::Fail("sub-agent unavailable".to_string()),
        Step::Fail("unused".to_string()),
    ));
    let runtime = runtime(resolver);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());

    let state = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("sub-agent unavailable")),
        "the delegate error is a model-visible tool result"
    );
}

// --- CE-10: resuming an awaiting delegation ---

/// Await the parent on a Delegation ticket (run step = Awaiting) and return the wired
/// runtime + commit coordinator ready to resume.
async fn await_on_delegation(
    resolver: Arc<MockResolver>,
) -> (Runtime, Arc<MemoryCommitCoordinator>) {
    let runtime = runtime(resolver);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(activation(), context)
        .await
        .expect("initial run awaits");
    assert_eq!(
        state,
        RunState::Awaiting,
        "the parent awaiting on delegation"
    );
    (runtime, commit)
}

#[tokio::test]
async fn resuming_a_delegation_done_folds_the_reply_and_completes() {
    // RD2: resuming an awaiting delegation runs the resolver one more step; a Done folds
    // the reply back as the delegate tool's result and the parent drives on.
    let resolver = Arc::new(MockResolver::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Done("delegate finished".to_string(), 0),
    ));
    let (runtime, commit) = await_on_delegation(resolver.clone()).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .resume(resume_command("more please"), commit.as_ref(), context)
        .await
        .expect("resume runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // The resolver was resumed with the awaiting handle and the user's input.
    let resumed = resolver.resumed_with.lock().unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].0, serde_json::json!({ "task": "remote-42" }));
    assert_eq!(resumed[0].1, "more please");
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("delegate finished")),
        "the resumed delegate reply is folded back as the tool result"
    );
    // The ticket is cleared once the delegation completes.
    assert!(
        commit
            .resume_ticket_for(&RunId("run-1".to_string()))
            .is_none()
    );
}

#[tokio::test]
async fn resuming_a_delegation_that_awaits_again_uses_the_new_handle() {
    // RD3: a resumed delegate that still needs input re-awaits the parent on a fresh
    // Delegation ticket carrying the NEW handle.
    let new_handle = serde_json::json!({ "task": "remote-99" });
    let resolver = Arc::new(MockResolver::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Awaiting(new_handle.clone()),
    ));
    let (runtime, commit) = await_on_delegation(resolver).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .resume(resume_command("still working?"), commit.as_ref(), context)
        .await
        .expect("resume runs");

    assert_eq!(state, RunState::Awaiting, "the delegate re-awaiting");
    let ticket = commit
        .resume_ticket_for(&RunId("run-1".to_string()))
        .expect("a fresh delegation ticket is committed");
    assert_eq!(ticket.reason, AwaitReason::Delegation);
    assert_eq!(
        ticket
            .pending_tool
            .as_ref()
            .and_then(|p| p.resume_handle.clone()),
        Some(new_handle),
        "the re-await carries the delegate's new handle"
    );
}

#[tokio::test]
async fn resuming_a_delegation_error_feeds_an_error_result_and_completes() {
    // RD4: a resumed delegate that fails yields a model-visible error result; the
    // parent drives on rather than aborting.
    let resolver = Arc::new(MockResolver::new(
        Step::Awaiting(serde_json::json!({ "task": "remote-42" })),
        Step::Fail("remote task lost".to_string()),
    ));
    let (runtime, commit) = await_on_delegation(resolver).await;

    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .resume(resume_command("status?"), commit.as_ref(), context)
        .await
        .expect("resume runs");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("remote task lost")),
        "the resumed delegate error is a model-visible tool result"
    );
}
