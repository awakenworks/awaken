//! Model-pool failover (#1): when a run's primary model fails cleanly, the engine
//! fails over to the next candidate binding in order. A single-model agent (no
//! candidates) is unchanged. Failover keys the circuit breaker per-model, so each
//! candidate tracks its own health.

use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{LlmRetryPolicy, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Error as LlmError, LlmExecutor, StopReason,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

/// Routes by `model_ref`: any model whose id is in `failing` returns a retryable
/// error; every other model returns a fixed success text. Records the ordered
/// sequence of model ids it was actually asked to run.
struct RouteLlm {
    failing: Vec<&'static str>,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl LlmExecutor for RouteLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let model = request.model_binding.model_ref.clone();
        self.seen.lock().unwrap().push(model.clone());
        if self.failing.contains(&model.as_str()) {
            Err(LlmError::Overloaded {
                message: format!("{model} is down"),
                retry_after: None,
            })
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text(format!("answered by {model}")),
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
            })
        }
    }
}

fn binding(model: &str) -> ModelBinding {
    ModelBinding {
        provider_identity_ref: "p".to_string(),
        model_ref: model.to_string(),
        backend_ref: "b".to_string(),
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

/// An activation whose primary model is `primary` with ordered pool `fallbacks`.
fn activation(primary: &str, fallbacks: &[&str]) -> RunActivation {
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
                model_binding: binding(primary),
                model_candidates: fallbacks.iter().map(|m| binding(m)).collect(),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
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

/// No backoff, no retries — failover, not per-model retry, is under test.
fn no_retries() -> LlmRetryPolicy {
    LlmRetryPolicy {
        max_retries: 0,
        backoff_base_ms: 0,
        overloaded_backoff_base_ms: 0,
    }
}

#[tokio::test]
async fn primary_failure_fails_over_to_the_next_candidate() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(RouteLlm {
            failing: vec!["primary"],
            seen: seen.clone(),
        }))
        .with_retry_policy(no_retries());
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation("primary", &["fallback"]), context)
        .await
        .expect("runs");

    // The run ended naturally on the fallback, not in a failure.
    assert!(
        matches!(outcome, Phase::Ended(EndCause::NaturalEnd)),
        "expected a natural end after failover, got {outcome:?}"
    );
    // Both models were tried, primary first then fallback — in candidate order.
    assert_eq!(&*seen.lock().unwrap(), &["primary", "fallback"]);
    let _ = &commit;
}

#[tokio::test]
async fn single_model_agent_does_not_fail_over() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(RouteLlm {
            failing: vec!["primary"],
            seen: seen.clone(),
        }))
        .with_retry_policy(no_retries());
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation("primary", &[]), context)
        .await
        .expect("runs");

    // No candidates → a failing primary is terminal, and only the primary was asked.
    assert!(
        matches!(outcome, Phase::Ended(EndCause::Error(_))),
        "expected a terminal failure, got {outcome:?}"
    );
    assert_eq!(&*seen.lock().unwrap(), &["primary"]);
}

#[tokio::test]
async fn all_candidates_failing_ends_in_terminal_failure_after_trying_each() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new()
        .with_llm(Arc::new(RouteLlm {
            failing: vec!["a", "b", "c"],
            seen: seen.clone(),
        }))
        .with_retry_policy(no_retries());
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation("a", &["b", "c"]), context)
        .await
        .expect("runs");

    assert!(
        matches!(outcome, Phase::Ended(EndCause::Error(_))),
        "expected a terminal failure, got {outcome:?}"
    );
    // Every candidate was tried, in order, exactly once.
    assert_eq!(&*seen.lock().unwrap(), &["a", "b", "c"]);
}
