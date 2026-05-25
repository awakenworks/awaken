# LLM Stream Error Handling & Mid-Stream Recovery — Design

## Problem

The inference pipeline (`awaken-runtime::engine::executor::GenaiExecutor`,
`awaken-runtime::engine::retry::RetryingExecutor`, and
`awaken-runtime::loop_runner::inference::execute_streaming`) retries whole
requests that fail *before* streaming starts, and already recovers from
`stop_reason == MaxTokens` via `awaken-runtime::context::truncation`. It does
not, however, recover from failures that occur **after streaming has begun**.
The current classification also conflates permanent errors (context overflow,
auth, invalid request) with transient ones, causing retry loops that make
outages worse.

Concretely, the following failure modes are handled poorly or not at all:

1. Mid-stream disconnects (`ECONNRESET`, HTTP/2 GOAWAY, proxy 5xx after
   headers): error is surfaced immediately, already-emitted deltas are
   discarded, accumulated state is thrown away.
2. Idle stalls (TCP half-open, provider hang): only bounded by the 120 s total
   request timeout; a stalled stream blocks a turn for the full window.
3. `400 "prompt is too long"` / `413 Payload Too Large`: classified as
   `Provider` and retried, amplifying the failure.
4. `529 overloaded_error` (Anthropic) is indistinguishable from generic 5xx,
   retried at the same cadence rather than backing off harder or failing over.
5. `Retry-After` header is ignored; retries use pure exponential backoff.
6. Partial tool_use JSON on mid-stream disconnect is dropped with no path to
   tell the model which tools succeeded and which were interrupted.
7. All-circuit-open state surfaces as a generic provider error.

## Goals

In scope for this spec:

- Split `InferenceExecutionError` into retryable / permanent variants, with
  per-variant policy (backoff base, retry count, circuit-breaker accounting).
- Honor `Retry-After` headers for `RateLimited` and `Overloaded`.
- Detect mid-stream interruptions and idle-stalls, capture accumulated state,
  and resume via one of four plans (R1–R4 below) that reuse existing loop
  runner machinery rather than inventing a new resume protocol.
- For parallel tool_use interruptions, deliver complete tool calls to the
  existing `StopReason::ToolUse` path and inject a user-visible note for the
  cancelled partial tool, so the model can decide whether to retry it.
- Add failure-injection tests covering each recovery path and each
  classification change.

### Explicit non-goals

- Cross-process / cross-crash resume of in-flight streams. Depends on
  `NatsBufferedThreadStore` (design WIP, not yet on `main`); tracked as a
  separate follow-up change once that store lands.
- Tool argument "repair prompting" for non-truncated malformed JSON. Tracked
  as a follow-up change.
- `ContentFiltered` surfacing. A variant is reserved in the enum but
  `map_error` keeps current behavior; the follow-up change wires provider
  classification and telemetry.
- Changes to the `genai` dependency or provider-specific SDK behavior.

## Architecture

### Error taxonomy

`awaken-contract::contract::executor::InferenceExecutionError` becomes:

```rust
#[derive(Debug, Error)]
pub enum InferenceExecutionError {
    // Transient — retryable. Counts toward circuit breaker.
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
    },

    #[error("provider overloaded: {0}")]
    Overloaded(String),

    #[error("upstream timeout: {0}")]
    Timeout(String),

    #[error("transient provider error: {0}")]
    Provider(String),

    #[error("stream interrupted ({cause})")]
    StreamInterrupted {
        cause: InterruptCause,
        snapshot: Box<InterruptSnapshot>,
    },

    // Permanent — NOT retryable. Does NOT count toward circuit breaker.
    #[error("context overflow: {0}")]
    ContextOverflow(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("content filtered: {0}")]
    ContentFiltered(String),

    // Fail-fast when the retry subsystem has nothing left.
    #[error("all models unavailable (circuit breakers open)")]
    AllModelsUnavailable,

    // Lifecycle.
    #[error("cancelled")]
    Cancelled,
}

pub enum InterruptCause {
    ConnectionReset,
    IdleStall,
    GoAway,
    Provider5xxMidStream(u16),
}
```

`RetryingExecutor::is_retryable` returns `true` for `RateLimited | Overloaded
| Timeout | Provider | StreamInterrupted`; `false` for everything else.
`CircuitBreaker::record_failure` is only called for the retryable set.
`ContextOverflow`, `InvalidRequest`, `Unauthorized`, `ModelNotFound`,
`AllModelsUnavailable`, `ContentFiltered`, `Cancelled` bypass the breaker.

### `GenaiExecutor::map_error` classification

- Status 429 → `RateLimited { retry_after: parse_retry_after(headers) }`.
- Status 529 → `Overloaded`. Status 503 also maps to `Overloaded` (Anthropic
  and many proxies emit 503 under overload).
- Status 408, 504, client-side read timeout → `Timeout`.
- Status 500, 502 → `Provider`.
- Status 400:
  - Body or message contains any of `"prompt is too long"`,
    `"context_length_exceeded"`, `"reduce the length"`, `"input is too long"`
    (case-insensitive) → `ContextOverflow`.
  - Otherwise → `InvalidRequest`.
- Status 413 → `ContextOverflow`.
- Status 401, 403 → `Unauthorized`.
- Status 404 → `ModelNotFound`.
- Status 422 → `InvalidRequest`.
- No status + string-match fallback preserves today's heuristics but routes
  `"overloaded"` to `Overloaded` (was `Provider`).

`parse_retry_after` accepts either an integer seconds value or an HTTP-date,
per RFC 9110 §10.2.3. Unparseable values fall back to `None`.

### Backoff policy

Two defaults and one rule change in `RetryingExecutor`:

- Existing `backoff_base_ms` (default 500, cap 8000) stays for `Timeout`,
  `Provider`, and `StreamInterrupted`.
- New `overloaded_backoff_base_ms` (default 2000, same cap) for `Overloaded`,
  to give the provider more room.
- For `RateLimited { retry_after: Some(d) }` and `Overloaded` carrying a
  `Retry-After` header, the wait is `max(retry_after, computed_backoff)`.
- `AllModelsUnavailable` is returned immediately when a higher-level router can
  prove no model candidate is available.

### Stream-level recovery: where it lives

Mid-stream retry logic lives in
`awaken-runtime::loop_runner::inference::execute_streaming`, not in
`RetryingExecutor`. Rationale:

| Responsibility                                     | Executor layer | loop_runner |
| -------------------------------------------------- | :------------: | :---------: |
| Mutate the `messages` field of `InferenceRequest` for continuation |   ✗   |   ✓   |
| Emit `ToolCallCancel` / `StreamReset` to `EventSink` |     ✗        |      ✓      |
| Inspect `ChatOptions.reasoning` for idle threshold |       ✗        |      ✓      |
| Synthesize `StopReason::ToolUse` to upstream caller|       ✗        |      ✓      |

`RetryingExecutor` keeps its current semantics: retry the *call* to
`execute_stream` on errors raised before the stream yields. Mid-stream errors
propagate up to `execute_streaming`, which owns recovery.

### `StreamCollector` snapshot

`awaken-runtime::engine::streaming::StreamCollector` gains:

```rust
pub struct InterruptSnapshot {
    pub text: Option<String>,
    pub completed_tool_calls: Vec<ToolCall>,
    pub in_flight_tool: Option<InFlightTool>,
    pub bytes_received: usize,
    pub last_delta_at: Instant,
}

pub struct InFlightTool {
    pub id: String,
    pub name: String,
    pub partial_args: String,
}

impl StreamCollector {
    pub fn last_delta_at(&self) -> Instant;
    pub fn interrupt_snapshot(&self) -> InterruptSnapshot;
}
```

`last_delta_at` is updated inside `process()` after every yielded `Text`,
`Reasoning`, or `ToolCallDelta`. A tool call moves from `in_flight_tool` to
`completed_tool_calls` when its accumulated JSON parses successfully. Only
one tool can be `in_flight` at a time per Anthropic / OpenAI streaming
semantics; if a second `ToolCallStart` arrives while one is in flight, the
previous one is finalized (and if JSON is invalid, dropped).

### Recovery plans

```rust
pub enum RecoveryPlan {
    ContinueText {
        assistant_prefix: String,
    },
    SynthesizeToolUse {
        completed: Vec<ToolCall>,
        cancelled_tool_hint: Option<InFlightTool>,
    },
    TruncateBeforeTool {
        assistant_prefix: String,
        cancelled_tool_id: String,
        cancelled_tool_name: String,
    },
    WholeRestart,
}

impl InterruptSnapshot {
    pub fn plan(&self) -> RecoveryPlan {
        match (self.text.as_deref(), self.completed_tool_calls.as_slice(),
               self.in_flight_tool.as_ref()) {
            // R1: text only
            (Some(t), [], None) if !t.is_empty() =>
                RecoveryPlan::ContinueText { assistant_prefix: t.into() },

            // R2: ≥1 completed tool, partial tool (if any) becomes a hint
            (_, completed @ [_, ..], in_flight) =>
                RecoveryPlan::SynthesizeToolUse {
                    completed: completed.to_vec(),
                    cancelled_tool_hint: in_flight.cloned(),
                },

            // R3: only one partial tool, but we have text before it
            (Some(t), [], Some(p)) if !t.is_empty() =>
                RecoveryPlan::TruncateBeforeTool {
                    assistant_prefix: t.into(),
                    cancelled_tool_id: p.id.clone(),
                    cancelled_tool_name: p.name.clone(),
                },

            // R4: only a partial tool, no text, no completed tools
            _ => RecoveryPlan::WholeRestart,
        }
    }
}
```

### `execute_streaming` loop

```rust
pub async fn execute_streaming(
    agent: &ActiveAgent,
    mut req: InferenceRequest,
    sink: &mut dyn EventSink,
    cancel: &CancellationToken,
    policy: &StreamRetryPolicy,
) -> Result<StreamResult, InferenceExecutionError> {
    let idle_timeout = idle_timeout_for(&req, policy.stream_idle_timeout);
    let mut attempt: u32 = 0;

    loop {
        let executor = &agent.llm_executor;
        let stream = executor.execute_stream(req.clone()).await?;
        let mut collector = StreamCollector::new();

        match run_stream_with_idle_timeout(
            stream, &mut collector, sink, cancel, idle_timeout,
        ).await {
            Ok(()) => return Ok(collector.finish()),

            Err(DriveError::Cancelled) => return Err(Cancelled),

            Err(DriveError::Interrupted(cause))
                if attempt >= policy.max_stream_retries =>
            {
                return Err(StreamInterrupted {
                    cause,
                    snapshot: Box::new(collector.interrupt_snapshot()),
                });
            }

            Err(DriveError::Interrupted(cause)) => {
                let snapshot = collector.interrupt_snapshot();
                match snapshot.plan() {
                    RecoveryPlan::ContinueText { assistant_prefix } => {
                        push_continuation(&mut req, assistant_prefix);
                    }
                    RecoveryPlan::SynthesizeToolUse { completed, cancelled_tool_hint } => {
                        return Ok(StreamResult::synthesized_tool_use(
                            collector.text(),
                            completed,
                            cancelled_tool_hint,
                        ));
                    }
                    RecoveryPlan::TruncateBeforeTool {
                        assistant_prefix, cancelled_tool_id, cancelled_tool_name: _,
                    } => {
                        sink.emit(StreamEvent::ToolCallCancel {
                            id: cancelled_tool_id,
                        });
                        push_continuation(&mut req, assistant_prefix);
                    }
                    RecoveryPlan::WholeRestart => {
                        sink.emit(StreamEvent::StreamReset { reason: cause });
                    }
                }

                backoff_for_stream(&cause, attempt, policy).await;
                attempt += 1;
            }
        }
    }
}

fn push_continuation(req: &mut InferenceRequest, assistant_prefix: String) {
    let msgs = req.messages_mut();
    msgs.push(Message::assistant_text(assistant_prefix));
    msgs.push(Message::user_text(CONTINUATION_PROMPT));
}
```

`run_stream_with_idle_timeout` wraps each `stream.next()` call in
`tokio::time::timeout(idle_timeout, …)`. On timeout it returns
`Interrupted(IdleStall)`. On transport errors it returns
`Interrupted(ConnectionReset | GoAway | Provider5xxMidStream(n))` based on
the underlying error source.

`StreamResult::synthesized_tool_use` constructs a `StreamResult` with
`stop_reason = StopReason::ToolUse`, the completed tool calls, any pre-tool
text, and — if `cancelled_tool_hint` is `Some` — stores the hint in a new
`pending_tool_cancel_hint: Option<ToolCancelHint>` field on the returned
`StreamResult` (and copied onto the run's turn state so the next
`user`-role message can carry it).

### Model-aware idle threshold

```rust
fn idle_timeout_for(req: &InferenceRequest, base: Duration) -> Duration {
    let model = req.upstream_model.as_str();
    let name_hits_thinking =
        model.contains("thinking")
        || model.contains("reasoning")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4");
    let options_hits_thinking = req.overrides.thinking_enabled();
    if name_hits_thinking || options_hits_thinking { base * 2 } else { base }
}
```

`InferenceOverrides` gains a new `thinking_enabled: Option<bool>` field (
serde default `None`, which behaves as `false`). The field is populated by the
caller (agent config, tool-use setup) when the request uses extended thinking
or any reasoning-effort knob. `thinking_enabled()` returns
`self.thinking_enabled.unwrap_or(false)`. The field is additive on the
`InferenceOverrides` struct; existing configs deserialize unchanged.

### Parallel tool_use: the `cancelled_tool_hint` injection

The R2 recovery path returns a `StreamResult` to the caller without any
further LLM round trip. Loop runner proceeds as it does for any
`StopReason::ToolUse`: the completed tool calls are executed via
`execute_ready_tool_calls`, and their results are assembled into a `user`
message containing `tool_result` content blocks.

A new optional payload on that `user` message — a trailing `text` content
block — is emitted iff `pending_tool_cancel_hint` is set:

```text
Note: your parallel call to tool `<name>` was interrupted mid-stream due to a
transient upstream error. The other tool calls completed normally. You may
re-issue the call if still needed.
```

The hint is consumed (cleared) when the next `assistant` turn is drafted.
No changes to the API contract with upstream providers: the injected block
is a standard `text` content block, legal on `user` messages in both
Anthropic and OpenAI schemas.

### Configuration

New fields on `awaken-contract::contract::executor::LlmRetryPolicy`:

```rust
pub struct LlmRetryPolicy {
    // existing fields ...
    pub max_stream_retries: u32,              // default 2, independent of max_retries
    pub stream_idle_timeout_secs: u64,        // default 60
    pub overloaded_backoff_base_ms: u64,      // default 2000
}
```

Defaults live in `Default for LlmRetryPolicy`. Agent-level overrides are
already plumbed through `AgentConfig`.

### Telemetry

Counters, registered through the existing `metrics` pattern used by
`circuit_breaker.rs`:

- `llm_stream_interrupted_total{cause, recovery_plan}`
- `llm_stream_idle_stall_total{model}`
- `llm_context_overflow_total{model}`
- `llm_overloaded_total{model}`
- `llm_retry_after_respected_total`
- `llm_all_models_unavailable_total`

Existing logs in `map_error` gain the classified variant in their structured
fields.

## Testing

All new tests live adjacent to the code they cover and use the existing mock
executor patterns from `awaken-runtime::engine::mock` and
`awaken-runtime::engine::retry` tests.

### Failure-injection harness

A new helper, `MockStreamInjector`, feeds scripted `ChatStreamEvent` sequences
with inline faults:

```rust
pub enum Injected {
    Event(ChatStreamEvent),
    Fault(InjectFault),
    IdleFor(Duration),
}

pub enum InjectFault {
    ConnectionReset,
    GoAway,
    Provider5xxMidStream(u16),
    EndWithoutStopReason,
}

pub struct MockStreamInjector {
    scripts: Vec<Vec<Injected>>,   // one script per attempt
    attempt: AtomicUsize,
}
```

Each call to `execute_stream` consumes the script at index
`min(attempt, scripts.len() - 1)` and increments `attempt`. If fewer scripts
than attempts are provided, the last script repeats, which models a steady
failure mode across retries. `IdleFor` is rendered via
`tokio::time::advance` under `tokio::test(start_paused = true)` to exercise
the idle-timeout path without wall-clock waits.

`Message::assistant_text` and `Message::user_text` are convenience
constructors on `awaken-contract::contract::message::Message` that wrap a
single-element `content` vector with a `Text` block. They are added as part
of this change if not already present.

### Unit tests (per module)

`crates/awaken-runtime/src/engine/executor.rs` — classification:

- `map_error_429_populates_retry_after_from_header`
- `map_error_529_maps_to_overloaded`
- `map_error_503_maps_to_overloaded`
- `map_error_400_prompt_too_long_maps_to_context_overflow`
- `map_error_400_schema_maps_to_invalid_request`
- `map_error_413_maps_to_context_overflow`
- `map_error_401_maps_to_unauthorized`
- `map_error_404_maps_to_model_not_found`

`crates/awaken-runtime/src/engine/retry.rs` — backoff & breaker:

- `retry_after_header_overrides_exponential_backoff`
- `overloaded_uses_longer_base_backoff`
- `context_overflow_bypasses_retry_and_breaker`
- `invalid_request_bypasses_retry_and_breaker`
- `all_models_open_returns_all_models_unavailable_without_wait`

`crates/awaken-runtime/src/engine/streaming.rs` — snapshot:

- `interrupt_snapshot_captures_text_only_state` → R1 plan
- `interrupt_snapshot_captures_completed_and_in_flight_tool` → R2 plan
- `interrupt_snapshot_truncates_before_in_flight_when_no_completed_tool` → R3
- `interrupt_snapshot_wholerestart_when_partial_tool_no_text` → R4
- `last_delta_at_updates_on_every_delta`

`crates/awaken-runtime/src/loop_runner/inference.rs` — stream loop:

- `mid_stream_connection_reset_with_text_retries_via_continuation` (R1)
- `mid_stream_reset_with_two_complete_tools_and_one_partial_synthesizes_tool_use_and_queues_hint` (R2)
- `mid_stream_reset_with_text_then_partial_tool_truncates_and_emits_cancel_event` (R3)
- `mid_stream_reset_with_only_partial_tool_whole_restart_emits_reset_event` (R4)
- `idle_stall_triggers_recovery_at_configured_threshold`
- `thinking_model_idle_threshold_doubled`
- `stream_retry_budget_exhausted_returns_last_snapshot`
- `cancellation_during_backoff_aborts_loop`

### Integration test

`crates/awaken/tests/llm_stream_recovery.rs` — end-to-end through the full
loop runner with `MockStreamInjector`:

- `parallel_tools_interrupted_midway_model_gets_hint_and_final_answer`
  Scripts attempt 1 to emit two complete `tool_use` blocks plus a partial
  third, then `ConnectionReset`. Tools A and B execute via mock handlers.
  Attempt 2's `user` message contains the injected hint text block. Attempt
  2 returns a clean final answer. Assert: A and B were called once each,
  the partial tool was never executed, the final `assistant` content matches
  the scripted answer.

- `context_overflow_triggers_compaction_then_succeeds`
  First `execute_stream` call returns `ContextOverflow`. Loop runner invokes
  `compact_with_llm`, replaces the message tail with the summary, and
  re-dispatches. Assert: no breaker failure recorded; second call
  succeeds.

- `stream_retries_bounded_and_final_snapshot_surfaced`
  Injector fails mid-stream on every attempt. After `max_stream_retries + 1`
  attempts, the run terminates with `StreamInterrupted` whose snapshot
  carries the last attempt's accumulated state.

## Migration and compatibility

- `InferenceExecutionError` additions are source-incompatible with external
  consumers that `match` exhaustively on the enum. The only in-tree
  exhaustive matches are in `RetryingExecutor`, `map_error`, and tests;
  these are updated in the same change. The enum is not `#[non_exhaustive]`
  today but becomes so as part of this spec so future variants (wired
  `ContentFiltered` handling, resume-id for cross-process recovery) are
  additive without breaking downstream code.
- `LlmRetryPolicy` gains fields with `Default` values; serde defaults cover
  deserialization of older configs.
- `StreamEvent` variants `ToolCallCancel` and `StreamReset` are additive.
  Existing `EventSink` implementations must pattern-match exhaustively; the
  trait adds default no-op methods for the new variants to keep implementors
  compiling without mandatory changes.

## Risks

- **R2 hint wording drift.** The injected hint is natural-language; models
  may react to its phrasing differently. The text is centralized as a
  `const` so it can be tuned from telemetry.
- **Idle threshold false positives on thinking models.** `2×` is a coarse
  rule. If we observe stalls on `claude-opus-4-7` with extended thinking
  beyond 120 s, the policy can be made per-model via
  `LlmRetryPolicy::per_model_overrides`.
- **Alignment with `codex/max-tokens-text-continuation` branch.** That
  in-flight branch touches the same continuation path. The implementation
  plan must rebase on top of it (or merge it first) to avoid divergent
  continuation prompts.
- **Breaker accounting change.** Moving permanent errors off the breaker
  changes observed failure counts; dashboards that read the breaker metric
  will see lower counts after rollout. Acceptable — those errors should
  never have been counted in the first place.

## Follow-up changes (separate specs)

| Change                                                          | Depends on              |
| --------------------------------------------------------------- | ----------------------- |
| Malformed-JSON tool args repair; `stop_reason`-missing recovery | This spec               |
| `ContentFiltered` surfacing and policy telemetry                | This spec               |
| Cross-process stream resume via `NatsBufferedThreadStore` WAL   | NATS store store design |
