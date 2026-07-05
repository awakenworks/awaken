//! A permanent inference failure commits a terminal Failed reason carrying the
//! error's classification code; a retryable one is retried with backoff and can
//! still succeed (G26). A `MaxTokens`-truncated text turn is continued in place
//! up to a per-step budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::event::Kind as StreamKind;
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamSink};
use awaken_runtime::{CircuitBreakerConfig, LlmRetryPolicy, Runtime};
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

/// A retry policy that keeps test suites fast without changing retry counts.
fn fast_retries(max_retries: usize) -> LlmRetryPolicy {
    LlmRetryPolicy {
        max_retries,
        backoff_base_ms: 1,
        overloaded_backoff_base_ms: 1,
    }
}

/// Always returns the same error.
struct FailingLlm {
    retryable: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for FailingLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.retryable {
            Err(LlmError::Overloaded {
                message: "overloaded".to_string(),
                retry_after: None,
            })
        } else {
            Err(LlmError::Unauthorized("bad api key".to_string()))
        }
    }
}

/// Fails permanently `fail_times`, then succeeds with text — exercises the
/// consecutive-failure tolerance, which absorbs post-retry failures at step
/// granularity.
struct PermanentThenOkLlm {
    fail_times: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for PermanentThenOkLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_times {
            Err(LlmError::Unauthorized("bad api key".to_string()))
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("recovered".to_string()),
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
            })
        }
    }
}

/// Fails retryably `fail_times`, then succeeds with text.
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
            Err(LlmError::Provider("503".to_string()))
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("recovered".to_string()),
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
            })
        }
    }
}

/// Emits `truncated_turns` text turns stopped by `MaxTokens`, then a final turn.
struct TruncatingLlm {
    truncated_turns: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for TruncatingLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.truncated_turns {
            Ok(ChatResponse {
                output: AssistantOutput::text(format!("chunk-{n}")),
                usage: None,
                stop_reason: Some(StopReason::MaxTokens),
            })
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("the end".to_string()),
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
            })
        }
    }
}

/// First turn: a complete tool call cut off by `MaxTokens`; then a text end.
struct TruncatedToolCallLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for TruncatedToolCallLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(ChatResponse {
                output: AssistantOutput::from_blocks(vec![ContentBlock::tool_use(
                    "c1",
                    "nonexistent-tool",
                    serde_json::json!({}),
                )]),
                usage: None,
                stop_reason: Some(StopReason::MaxTokens),
            })
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("done".to_string()),
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
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

#[tokio::test]
async fn permanent_inference_error_commits_a_terminal_failed_reason() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(FailingLlm {
        retryable: false,
        calls: calls.clone(),
    }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    // The fault carries the provider error's stable classification code, so a
    // host can categorize the unrecoverable failure without parsing messages.
    match &outcome {
        Phase::Ended(EndCause::Error(Failure::Inference { code, message })) => {
            assert_eq!(code, "unauthorized");
            assert!(message.contains("bad api key"));
        }
        other => panic!("expected a classified inference failure, got {other:?}"),
    }
    // A permanent error is not retried.
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let committed = commit.committed();
    assert!(matches!(
        committed.latest_run.unwrap().phase,
        Phase::Ended(EndCause::Error(Failure::Inference { .. }))
    ));
    // The terminal reason (with its code) is recorded in the phase event
    // payload. The last phase event is the terminal one — the first records
    // the transition into Running at the initial step boundary.
    let phase_event = committed
        .events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind,
                awaken_agent_contract::event::kind::Kind::RunPhaseChanged
            )
        })
        .expect("a phase event");
    let payload = phase_event.payload.to_string();
    assert!(payload.contains("bad api key"));
    assert!(payload.contains("unauthorized"));
}

#[tokio::test]
async fn retryable_error_is_retried_until_exhausted_then_failed_with_code() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(FailingLlm {
            retryable: true,
            calls: calls.clone(),
        }))
        .with_retry_policy(fast_retries(2));
    install(&runtime);

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    match &outcome {
        Phase::Ended(EndCause::Error(Failure::Inference { code, .. })) => {
            assert_eq!(code, "overloaded");
        }
        other => panic!("expected a classified inference failure, got {other:?}"),
    }
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
        .with_retry_policy(fast_retries(3));
    install(&runtime);

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    // One failure then one success.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn max_tokens_truncation_continues_in_place_and_recovers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(TruncatingLlm {
        truncated_turns: 1,
        calls: calls.clone(),
    }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    // The truncated turn plus one continuation round.
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // The committed transcript explains the recovery: the partial assistant
    // text, the continuation prompt, then the completed turn.
    let committed = commit.committed();
    let texts: Vec<String> = committed
        .messages
        .iter()
        .map(|m| {
            format!(
                "{:?}:{}",
                m.role,
                awaken_agent_contract::agent::content::extract_text(&m.content)
            )
        })
        .collect();
    assert!(
        texts.iter().any(|t| t == "Assistant:chunk-0"),
        "partial text is committed: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("User:Your response was cut off")),
        "continuation prompt is committed: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "Assistant:the end"),
        "final turn is committed: {texts:?}"
    );
}

#[tokio::test]
async fn max_tokens_budget_exhausted_lets_the_partial_turn_stand() {
    let calls = Arc::new(AtomicUsize::new(0));
    // Every turn truncates; the per-step budget (2) bounds the continuations.
    let runtime = Runtime::new()
        .with_llm(Arc::new(TruncatingLlm {
            truncated_turns: usize::MAX,
            calls: calls.clone(),
        }))
        .with_max_continuation_retries(2);
    install(&runtime);

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    // The still-truncated turn stands as a text-only natural end.
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    // 1 initial turn + 2 continuation rounds.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn failed_run_emits_run_failed_on_the_live_stream_before_run_finished() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(FailingLlm {
        retryable: false,
        calls,
    }));
    install(&runtime);

    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new().with_stream_sink(sink.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert!(matches!(outcome, Phase::Ended(EndCause::Error(_))));

    let kinds: Vec<StreamKind> = sink.events().into_iter().map(|e| e.kind).collect();
    let failed_at = kinds
        .iter()
        .position(|k| {
            matches!(
                k,
                StreamKind::RunFailed { code, message }
                    if code == "unauthorized" && message.contains("bad api key")
            )
        })
        .unwrap_or_else(|| panic!("a RunFailed event with the fault code: {kinds:?}"));
    let finished_at = kinds
        .iter()
        .position(|k| matches!(k, StreamKind::RunFinished))
        .expect("a terminal RunFinished");
    assert!(
        failed_at < finished_at,
        "RunFailed precedes the RunFinished close signal"
    );
}

#[tokio::test]
async fn consecutive_failure_tolerance_absorbs_a_failed_step_until_success() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(PermanentThenOkLlm {
            fail_times: 2,
            calls: calls.clone(),
        }))
        .with_max_consecutive_inference_failures(3);
    install(&runtime);

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    // Two failed steps are absorbed (below the threshold of 3); the third
    // step succeeds and the run ends naturally.
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn consecutive_failure_tolerance_exhausted_ends_with_the_last_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(FailingLlm {
            retryable: false,
            calls: calls.clone(),
        }))
        .with_max_consecutive_inference_failures(2);
    install(&runtime);

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    // The second consecutive failure is terminal, classified by its code.
    match &outcome {
        Phase::Ended(EndCause::Error(Failure::Inference { code, .. })) => {
            assert_eq!(code, "unauthorized");
        }
        other => panic!("expected a classified inference failure, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn circuit_breaker_opens_after_counted_failures_and_fails_fast() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(FailingLlm {
            retryable: true,
            calls: calls.clone(),
        }))
        .with_retry_policy(fast_retries(0))
        .with_circuit_breaker(CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: std::time::Duration::from_secs(600),
            half_open_max_probes: 1,
        });
    install(&runtime);

    // Two runs, each one counted failure: the circuit opens at the threshold.
    for _ in 0..2 {
        let outcome = runtime
            .execute(activation(), RuntimeRunContext::new())
            .await
            .expect("runs");
        assert!(matches!(outcome, Phase::Ended(EndCause::Error(_))));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // The third run fails fast: the provider is never called while open.
    let outcome = runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");
    match &outcome {
        Phase::Ended(EndCause::Error(Failure::Inference { code, message })) => {
            assert_eq!(code, "provider_error");
            assert!(message.contains("circuit breaker"), "{message}");
        }
        other => panic!("expected a fail-fast provider error, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2, "no call while open");
}

#[tokio::test]
async fn permanent_failures_do_not_trip_the_circuit_breaker() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new()
        .with_llm(Arc::new(FailingLlm {
            retryable: false,
            calls: calls.clone(),
        }))
        .with_circuit_breaker(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: std::time::Duration::from_secs(600),
            half_open_max_probes: 1,
        });
    install(&runtime);

    // A permanent error says nothing about provider health: even with a
    // threshold of 1, every run still reaches the provider.
    for _ in 0..3 {
        let outcome = runtime
            .execute(activation(), RuntimeRunContext::new())
            .await
            .expect("runs");
        assert!(matches!(
            outcome,
            Phase::Ended(EndCause::Error(Failure::Inference { .. }))
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn max_tokens_with_tool_calls_skips_continuation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(TruncatedToolCallLlm {
        calls: calls.clone(),
    }));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
    // The truncated-but-complete tool call executes normally (its result feeds
    // the next step); no continuation prompt is injected.
    let committed = commit.committed();
    assert!(
        !committed.messages.iter().any(|m| {
            awaken_agent_contract::agent::content::extract_text(&m.content)
                .starts_with("Your response was cut off")
        }),
        "no continuation prompt expected"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
