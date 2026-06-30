//! A permanent inference failure commits a terminal Failed reason; a transient
//! one is retried with backoff and can still succeed (G26).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Error as LlmError, LlmExecutor,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// Always returns the same error.
struct FailingLlm {
    transient: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for FailingLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.transient {
            Err(LlmError::Transient("overloaded".to_string()))
        } else {
            Err(LlmError::Inference("bad api key".to_string()))
        }
    }
}

/// Fails transiently `fail_times`, then succeeds with text.
struct FlakyLlm {
    fail_times: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for FlakyLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_times {
            Err(LlmError::Transient("503".to_string()))
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("recovered".to_string()),
                usage: None,
            })
        }
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
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_instance_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        trace: Default::default(),
    }
}

#[tokio::test]
async fn permanent_inference_error_commits_a_terminal_failed_reason() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(FailingLlm {
        transient: false,
        calls: calls.clone(),
    }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert!(matches!(
        outcome,
        Phase::Ended(EndCause::Error(Failure::Inference(_)))
    ));
    // A permanent error is not retried.
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let committed = commit.committed();
    assert!(matches!(
        committed.latest_run.unwrap().phase,
        Phase::Ended(EndCause::Error(Failure::Inference(_)))
    ));
    // The terminal reason is recorded in the phase event payload.
    let phase_event = committed
        .events
        .iter()
        .find(|e| {
            matches!(
                e.kind,
                awaken_agent_contract::event::kind::Kind::RunPhaseChanged
            )
        })
        .expect("a phase event");
    assert!(phase_event.payload.to_string().contains("bad api key"));
}

#[tokio::test]
async fn transient_error_is_retried_until_exhausted_then_failed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(FailingLlm {
            transient: true,
            calls: calls.clone(),
        }))
        .with_infer_retries(2);
    install(&runtime);

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert!(matches!(
        outcome,
        Phase::Ended(EndCause::Error(Failure::Inference(_)))
    ));
    // 1 initial attempt + 2 retries = 3 calls.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn transient_error_then_success_recovers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(FlakyLlm {
            fail_times: 1,
            calls: calls.clone(),
        }))
        .with_infer_retries(3);
    install(&runtime);

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    // One failure then one success.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
