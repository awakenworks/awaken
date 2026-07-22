use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::pause::PauseSignal;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::terminal::{
    CommittedTerminalRun, RunTerminalObserver, RunTerminalObserverError,
};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

struct TextLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

#[derive(Default)]
struct RecordingObserver {
    events: Mutex<Vec<CommittedTerminalRun>>,
    fail: bool,
}

#[async_trait::async_trait]
impl RunTerminalObserver for RecordingObserver {
    fn observer_id(&self) -> &str {
        "recording-observer"
    }

    async fn observe(
        &self,
        terminal: &CommittedTerminalRun,
    ) -> Result<(), RunTerminalObserverError> {
        self.events.lock().push(terminal.clone());
        if self.fail {
            Err(RunTerminalObserverError("expected failure".to_string()))
        } else {
            Ok(())
        }
    }
}

struct RejectingCommit;

#[async_trait::async_trait]
impl CommitCoordinator for RejectingCommit {
    async fn commit(&self, _commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        Err(CommitError::Rejected("expected rejection".to_string()))
    }
}

fn snapshot() -> ExecutableAgentSnapshot {
    let fingerprint = CatalogFingerprint("terminal-observer-catalog".to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId("terminal-observer-snapshot".to_string()),
        metadata: Default::default(),
        root_agent_id: AgentId("terminal-observer-agent".to_string()),
        resolved_spec: ResolvedSpec {
            model_candidates: Vec::new(),
            catalog_fingerprint: fingerprint.clone(),
            instructions: String::new(),
            max_steps: 4,
            delegation_limits: Default::default(),
            model_binding: ModelBinding {
                provider_identity_ref: "provider".to_string(),
                model_ref: "model".to_string(),
                backend_ref: "backend".to_string(),
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

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("terminal-run".to_string()),
        ThreadId("terminal-thread".to_string()),
        snapshot(),
        vec![Message {
            id: MessageId("terminal-input".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hello")],
        }],
    )
}

#[tokio::test]
async fn terminal_observer_runs_only_after_a_successful_terminal_commit() {
    let observer = Arc::new(RecordingObserver::default());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let token = CancellationToken::new();
    token.cancel();
    let context = RuntimeRunContext::new()
        .with_commit(commit)
        .with_terminal_observer(observer.clone())
        .with_cancellation(token);

    let state = Runtime::new()
        .execute(activation(), context)
        .await
        .expect("terminal run");

    assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    assert_eq!(
        observer.events.lock().as_slice(),
        &[CommittedTerminalRun {
            run_id: RunId("terminal-run".to_string()),
            thread_id: ThreadId("terminal-thread".to_string()),
            cause: EndCause::Cancelled,
        }]
    );
}

#[tokio::test]
async fn awaiting_is_not_delivered_to_terminal_observers() {
    let observer = Arc::new(RecordingObserver::default());
    let pause = PauseSignal::new();
    pause.request();
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(MemoryCommitCoordinator::new()))
        .with_terminal_observer(observer.clone())
        .with_pause(pause);

    let state = Runtime::new()
        .with_llm(Arc::new(TextLlm {
            calls: AtomicUsize::new(0),
        }))
        .execute(activation(), context)
        .await
        .expect("awaiting run");

    assert_eq!(state, RunState::Awaiting);
    assert!(observer.events.lock().is_empty());
}

#[tokio::test]
async fn commit_failure_prevents_terminal_delivery() {
    let observer = Arc::new(RecordingObserver::default());
    let token = CancellationToken::new();
    token.cancel();
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(RejectingCommit))
        .with_terminal_observer(observer.clone())
        .with_cancellation(token);

    let error = Runtime::new()
        .execute(activation(), context)
        .await
        .expect_err("commit must fail");

    assert!(error.to_string().contains("expected rejection"));
    assert!(observer.events.lock().is_empty());
}

#[tokio::test]
async fn an_uncommitted_terminal_result_is_not_delivered() {
    let observer = Arc::new(RecordingObserver::default());
    let token = CancellationToken::new();
    token.cancel();
    let context = RuntimeRunContext::new()
        .with_terminal_observer(observer.clone())
        .with_cancellation(token);

    let state = Runtime::new()
        .execute(activation(), context)
        .await
        .expect("non-durable embedded run");

    assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    assert!(
        observer.events.lock().is_empty(),
        "the seam observes committed truth, not a transient return value"
    );
}

#[tokio::test]
async fn observer_failure_cannot_change_the_committed_run_result() {
    let observer = Arc::new(RecordingObserver {
        events: Mutex::new(Vec::new()),
        fail: true,
    });
    let token = CancellationToken::new();
    token.cancel();
    let context = RuntimeRunContext::new()
        .with_commit(Arc::new(MemoryCommitCoordinator::new()))
        .with_terminal_observer(observer.clone())
        .with_cancellation(token);

    let state = Runtime::new()
        .execute(activation(), context)
        .await
        .expect("observer errors are isolated");

    assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    assert_eq!(observer.events.lock().len(), 1);
}

#[tokio::test]
async fn stable_id_reentry_redelivers_without_reexecuting_inference() {
    let observer = Arc::new(RecordingObserver::default());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let llm = Arc::new(TextLlm {
        calls: AtomicUsize::new(0),
    });
    let runtime = Runtime::new().with_llm(llm.clone());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit)
        .with_terminal_observer(observer.clone());
    let run_id = RunId("stable-terminal-run".to_string());

    for _ in 0..2 {
        let state = runtime
            .run_to_completion_with_id(
                &snapshot(),
                run_id.clone(),
                "stable-terminal-thread",
                "hello",
                context.clone(),
                |_| unreachable!("this run does not await"),
            )
            .await
            .expect("stable run");
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    }

    assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        observer.events.lock().len(),
        2,
        "redelivery is intentional; the observer suppresses duplicate effects"
    );
}
