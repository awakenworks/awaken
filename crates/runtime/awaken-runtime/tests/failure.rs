//! A permanent inference failure commits a terminal Failed reason carrying the
//! error's classification code; a retryable one is retried with backoff and can
//! still succeed (G26). A `MaxTokens`-truncated text response is continued in place
//! up to a per-step budget.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_runtime::{CircuitBreakerConfig, LlmRetryPolicy, Runtime};
use awaken_runtime_contract::CommittedThreadView;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, Error as LlmError, LlmExecutor, StopReason,
    ThreadUsage, TokenUsage,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamSink};

/// A retry policy that keeps test suites fast without changing retry counts.
fn fast_retries(max_retries: usize) -> LlmRetryPolicy {
    LlmRetryPolicy {
        max_retries,
        backoff_base_ms: 1,
        overloaded_backoff_base_ms: 1,
        attempt_timeout: std::time::Duration::from_secs(5),
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
                stop_reason: Some(StopReason::NaturalEnd),
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
                stop_reason: Some(StopReason::NaturalEnd),
            })
        }
    }
}

/// Emits `truncated_steps` text responses stopped by `MaxTokens`, then a final response.
struct TruncatingLlm {
    truncated_steps: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for TruncatingLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.truncated_steps {
            Ok(ChatResponse {
                output: AssistantOutput::text(format!("chunk-{n}")),
                usage: Some(TokenUsage {
                    prompt_tokens: 2,
                    completion_tokens: 1,
                    ..Default::default()
                }),
                stop_reason: Some(StopReason::MaxTokens),
            })
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("the end".to_string()),
                usage: Some(TokenUsage {
                    prompt_tokens: 3,
                    completion_tokens: 2,
                    ..Default::default()
                }),
                // Provider compatibility: absence is the legacy natural-end
                // representation. An explicit but unknown provider reason is
                // rejected by the adapter before reaching this neutral port.
                stop_reason: None,
            })
        }
    }
}

/// First response exhausts the output budget in private reasoning; the next
/// bounded continuation produces a public terminal answer.
struct ReasoningOnlyTruncatingLlm {
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait::async_trait]
impl LlmExecutor for ReasoningOnlyTruncatingLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.requests.lock().unwrap().push(request);
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(ChatResponse {
                output: AssistantOutput::from_blocks(vec![ContentBlock::thinking(
                    "private plan consumed the first allowance",
                )]),
                usage: Some(TokenUsage {
                    prompt_tokens: 11,
                    completion_tokens: 4,
                    ..Default::default()
                }),
                stop_reason: Some(StopReason::MaxTokens),
            })
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("bounded continuation completed"),
                usage: Some(TokenUsage {
                    prompt_tokens: 13,
                    completion_tokens: 3,
                    ..Default::default()
                }),
                stop_reason: Some(StopReason::NaturalEnd),
            })
        }
    }
}

/// First response: a complete tool call cut off by `MaxTokens`; then a text end.
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
                output: AssistantOutput::from_blocks(vec![
                    ContentBlock::text("partial explanation"),
                    ContentBlock::tool_use("c1", "nonexistent-tool", serde_json::json!({})),
                ]),
                usage: None,
                stop_reason: Some(StopReason::MaxTokens),
            })
        } else {
            Ok(ChatResponse {
                output: AssistantOutput::text("done".to_string()),
                usage: None,
                stop_reason: Some(StopReason::NaturalEnd),
            })
        }
    }
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "p".to_string(),
                        model_ref: "m".to_string(),
                        backend_ref: "b".to_string(),
                    },
                ),
                tool_descriptors: Vec::new(),
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
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

#[tokio::test]
async fn permanent_inference_error_commits_a_terminal_failed_reason() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(FailingLlm {
        retryable: false,
        calls: calls.clone(),
    }));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    // The fault carries the provider error's stable classification code, so a
    // host can categorize the unrecoverable failure without parsing messages.
    match &outcome {
        RunState::Ended(EndCause::Error(Failure::Inference { code, message })) => {
            assert_eq!(code, "unauthorized");
            assert!(message.contains("bad api key"));
        }
        other => panic!("expected a classified inference failure, got {other:?}"),
    }
    // A permanent error is not retried.
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let committed = commit.committed();
    assert!(matches!(
        committed.latest_run.unwrap().state,
        RunState::Ended(EndCause::Error(Failure::Inference { .. }))
    ));
    // The terminal reason (with its code) is recorded in the state event
    // payload. The last state event is the terminal one — the first records
    // the transition into Running at the initial step boundary.
    let state_event = committed
        .events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.kind,
                awaken_agent_contract::audit::kind::Kind::RunStateChanged
            )
        })
        .expect("a state event");
    let payload = state_event.payload.to_string();
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

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    match &outcome {
        RunState::Ended(EndCause::Error(Failure::Inference { code, .. })) => {
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

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    // One failure then one success.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// MaxTokens live-coordinate cause/effect table: C1 response 0 truncates with
/// usage and commits a partial; C2 response 1 completes with an absent legacy
/// stop reason and more usage; E1 both
/// keep the same `(run,thread,step)`; E2 C2 advances only `response`; E3 the
/// transcript still commits partial/prompt/final; E4 both request tallies are
/// committed exactly once and attributed to the selected model. Rules
/// M1=C1=>response 0+E3, M2=C1+C2=>response [0,1]+E1+E2+E3+E4.
/// Constraints/invariants: continuation remains in one Run/Thread/Step and only
/// the response coordinate advances; transcript order is append-only; `None`
/// preserves the legacy natural-end contract while an adapter-classified unknown
/// reason never reaches the loop.
#[tokio::test]
async fn max_tokens_truncation_continues_in_place_and_recovers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(TruncatingLlm {
        truncated_steps: 1,
        calls: calls.clone(),
    }));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let stream = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(stream.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    // The truncated response plus one continuation Step.
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // The committed transcript explains the recovery: the partial assistant
    // text, the continuation prompt, then the completed response.
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
        "final response is committed: {texts:?}"
    );
    let coordinates = stream
        .observations()
        .into_iter()
        .filter_map(|observation| {
            matches!(
                observation.event.kind,
                AgentEvent::Delta(Delta::TextDelta { .. })
            )
            .then_some(observation.assistant_response)
        })
        .collect::<Vec<_>>();
    assert_eq!(coordinates.len(), 2, "M2 one delta per model response");
    assert_eq!(
        coordinates
            .iter()
            .map(|coordinate| coordinate
                .as_ref()
                .expect("M1/M2 Runtime coordinate")
                .response)
            .collect::<Vec<_>>(),
        vec![0, 1],
        "M2/E2"
    );
    assert!(
        coordinates.iter().all(|coordinate| {
            coordinate.as_ref().is_some_and(|coordinate| {
                coordinate.thread_id == ThreadId("thread-1".into()) && coordinate.step == 0
            })
        }),
        "M1/M2 E1"
    );
    let usage =
        ThreadUsage::from_committed_state(&commit.committed_state(&ThreadId("thread-1".into())));
    assert_eq!(
        usage.by_model["m"],
        TokenUsage {
            prompt_tokens: 5,
            completion_tokens: 3,
            ..Default::default()
        },
        "M2/E4: truncated and final request usage is accumulated exactly once"
    );
}

#[tokio::test]
async fn reasoning_only_max_tokens_uses_the_same_bounded_continuation_owner() {
    // Reasoning-only truncation cause/effect table. C1 a provider returns one
    // non-empty Thinking block, MaxTokens, usage, and no public text/ToolUse;
    // C2 the Step has continuation budget; C3 the next response is a natural
    // public answer. Effects: E1 no identical provider retry occurs; E2 the
    // existing in-place continuation commits the private partial plus its one
    // smaller-pieces prompt; E3 the second logical request completes the same
    // Run/Thread/Step; E4 both usages are counted once and reasoning never leaks
    // into answer text. Rule Q1=C1+C2+C3=>E1+E2+E3+E4. Neighbor rules: text
    // truncation is M1/M2 above; exhausted budget is X1 below; MaxTokens with a
    // typed tool is T1 below; NaturalEnd reasoning-only is rejected at the
    // provider usability seam. The provider transcript mapper owns omission of
    // a standalone reasoning-only history row, so Runtime adds no second mapper.
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::new().with_llm(Arc::new(ReasoningOnlyTruncatingLlm {
        calls: calls.clone(),
        requests: requests.clone(),
    }));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let outcome = runtime
        .execute(
            activation(),
            RuntimeRunContext::new().with_commit(commit.clone()),
        )
        .await
        .expect("Q1 executes");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "Q1/E3");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "Q1/E1");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "Q1/E1");
    assert!(
        requests[1].messages.iter().any(|message| {
            message.role == Role::Assistant
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Thinking { .. }))
        }),
        "Q1/E2 private partial remains neutral transcript truth"
    );
    assert!(
        requests[1].messages.iter().any(|message| {
            message.role == Role::User
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("smaller pieces")))
        }),
        "Q1/E2 one bounded continuation prompt"
    );
    let committed = commit.committed();
    assert!(
        committed.messages.iter().any(|message| {
            message.role == Role::Assistant
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Thinking { .. }))
        }),
        "Q1/E2"
    );
    assert_eq!(
        committed
            .messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| awaken_agent_contract::agent::content::extract_text(&message.content))
            .collect::<Vec<_>>(),
        vec!["".to_string(), "bounded continuation completed".to_string()],
        "Q1/E4 reasoning is never answer text"
    );
    assert_eq!(
        ThreadUsage::from_committed_state(&commit.committed_state(&ThreadId("thread-1".into())))
            .by_model["m"],
        TokenUsage {
            prompt_tokens: 24,
            completion_tokens: 7,
            ..Default::default()
        },
        "Q1/E4"
    );
}

/// Cause/effect design: C1 every model response is text-only and stopped by
/// MaxTokens; C2 the continuation budget is two; C3 a commit coordinator is
/// present. Effects: E1 exactly one initial request plus two continuations; E2
/// the last partial is committed; E3 exhaustion is an explicit inference fault,
/// never NaturalEnd; E4 no prompt authorizes a request beyond the budget.
/// Decision rule X1=C1+C2+C3=>E1+E2+E3+E4; recovery before exhaustion is covered
/// by `max_tokens_truncation_continues_in_place_and_recovers`.
/// Constraints/invariants: the budget is exact and bounded; durable partial
/// evidence survives the terminal failure; incomplete output is never success.
#[tokio::test]
async fn max_tokens_budget_exhaustion_commits_partial_and_fails_explicitly() {
    let calls = Arc::new(AtomicUsize::new(0));
    // Every response truncates; the per-Step budget (2) bounds the continuations.
    let runtime = Runtime::new()
        .with_llm(Arc::new(TruncatingLlm {
            truncated_steps: usize::MAX,
            calls: calls.clone(),
        }))
        .with_max_continuation_retries(2);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert!(
        matches!(
            outcome,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. }))
                if code == "max_tokens_exhausted"
        ),
        "X1/E3: incomplete output must fail explicitly, got {outcome:?}"
    );
    // 1 initial response + 2 continuation Steps.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let texts = commit
        .committed()
        .messages
        .iter()
        .map(|message| awaken_agent_contract::agent::content::extract_text(&message.content))
        .collect::<Vec<_>>();
    assert!(texts.iter().any(|text| text == "chunk-2"), "X1/E2");
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.starts_with("Your response was cut off"))
            .count(),
        2,
        "X1/E4: only requests within the budget receive continuation prompts"
    );
    let usage =
        ThreadUsage::from_committed_state(&commit.committed_state(&ThreadId("thread-1".into())));
    assert_eq!(
        usage.by_model["m"],
        TokenUsage {
            prompt_tokens: 6,
            completion_tokens: 3,
            ..Default::default()
        },
        "X1/E2: all three truncated request tallies survive terminal failure"
    );
}

#[tokio::test]
async fn failed_run_emits_run_failed_on_the_live_stream_before_run_finished() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(FailingLlm {
        retryable: false,
        calls,
    }));

    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new().with_stream_sink(sink.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert!(matches!(outcome, RunState::Ended(EndCause::Error(_))));

    let kinds: Vec<AgentEvent> = sink.events().into_iter().map(|e| e.kind).collect();
    let failed_at = kinds
        .iter()
        .position(|k| {
            matches!(
                k,
                AgentEvent::Fact(Fact::RunFailed { code, message })
                    if code == "unauthorized" && message.contains("bad api key")
            )
        })
        .unwrap_or_else(|| panic!("a RunFailed event with the fault code: {kinds:?}"));
    let finished_at = kinds
        .iter()
        .position(|k| matches!(k, AgentEvent::Fact(Fact::RunFinished { .. })))
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

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    // Two failed steps are absorbed (below the threshold of 3); the third
    // step succeeds and the run ends naturally.
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
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

    let context = RuntimeRunContext::new();
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    // The second consecutive failure is terminal, classified by its code.
    match &outcome {
        RunState::Ended(EndCause::Error(Failure::Inference { code, .. })) => {
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

    // Two runs, each one counted failure: the circuit opens at the threshold.
    for _ in 0..2 {
        let outcome = runtime
            .execute(activation(), RuntimeRunContext::new())
            .await
            .expect("runs");
        assert!(matches!(outcome, RunState::Ended(EndCause::Error(_))));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // The third run fails fast: the provider is never called while open.
    let outcome = runtime
        .execute(activation(), RuntimeRunContext::new())
        .await
        .expect("runs");
    match &outcome {
        RunState::Ended(EndCause::Error(Failure::Inference { code, message })) => {
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

    // A permanent error says nothing about provider health: even with a
    // threshold of 1, every run still reaches the provider.
    for _ in 0..3 {
        let outcome = runtime
            .execute(activation(), RuntimeRunContext::new())
            .await
            .expect("runs");
        assert!(matches!(
            outcome,
            RunState::Ended(EndCause::Error(Failure::Inference { .. }))
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn max_tokens_with_tool_calls_skips_continuation() {
    // Cause/effect design: C1 MaxTokens is reported with both partial text and a
    // complete ToolUse; C2 the tool is executable. Effects: E1 no text-continuation prompt is
    // injected; E2 the tool call follows the ordinary tool path; E3 the next
    // model response may end naturally. Rule T1=C1+C2=>E1+E2+E3.
    // Constraint: stop-reason recovery must never override tool-call causality.
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::new().with_llm(Arc::new(TruncatedToolCallLlm {
        calls: calls.clone(),
    }));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
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
