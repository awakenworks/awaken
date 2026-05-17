---
title: "Events"
description: "The agent loop emits AgentEvent values as it executes. Events are streamed to clients via SSE and consumed by protocol encoders."
---

The agent loop emits `AgentEvent` values as it executes. Events are streamed to
clients via SSE and consumed by protocol encoders.

## AgentEvent

All variants are tagged with `event_type` in their JSON serialization
(`#[serde(tag = "event_type", rename_all = "snake_case")]`).

```rust
pub enum AgentEvent {
    RunStart {
        thread_id: String,
        run_id: String,
        parent_run_id: Option<String>,    // omitted when None
        identity: Option<RunIdentity>,    // omitted when None
    },

    RunFinish {
        thread_id: String,
        run_id: String,
        identity: Option<RunIdentity>,    // omitted when None
        result: Option<Value>,            // omitted when None
        termination: TerminationReason,
    },

    TextDelta { delta: String },

    ReasoningDelta { delta: String },

    ReasoningEncryptedValue { encrypted_value: String },

    ToolCallStart { id: String, name: String },

    ToolCallDelta { id: String, args_delta: String },

    ToolCallReady {
        id: String,
        name: String,
        arguments: Value,
    },

    ToolCallDone {
        id: String,
        message_id: String,
        result: ToolResult,
        outcome: ToolCallOutcome,
    },

    ToolCallStreamDelta {
        id: String,
        name: String,
        delta: String,
    },

    ToolCallResumed { target_id: String, result: Value },

    /// A tool call that started streaming was cancelled before its argument
    /// JSON closed (mid-stream interruption recovery). Consumers should drop
    /// any partial deltas they buffered for this `id`.
    ToolCallCancel {
        id: String,
        name: String,
        reason: String,                   // e.g. "connection reset", "idle stall"
    },

    /// The current assistant turn was restarted after a mid-stream
    /// interruption that could not be recovered via continuation. Consumers
    /// should discard all previously-emitted deltas in this turn.
    StreamReset { reason: String },

    MessagesSnapshot { messages: Vec<Value> },

    ActivitySnapshot {
        message_id: String,
        activity_type: String,
        content: Value,
        replace: Option<bool>,            // omitted when None
    },

    ActivityDelta {
        message_id: String,
        activity_type: String,
        patch: Vec<Value>,
    },

    StepStart { message_id: String },

    StepEnd,

    InferenceComplete {
        model: String,
        usage: Option<TokenUsage>,        // omitted when None
        duration_ms: u64,
    },

    StateSnapshot { snapshot: Value },

    StateDelta { delta: Vec<Value> },

    Error {
        message: String,
        code: Option<String>,             // omitted when None
    },
}
```

**Crate path:** `awaken::contract::event::AgentEvent`

### Stream-recovery semantics

`ToolCallCancel` and `StreamReset` are advisory drop signals emitted during
mid-stream recovery. Consumers discard partial deltas for the named tool call
(or for the whole turn) and keep reading; the recovered deltas follow on the
normal `TextDelta` / `ToolCallDelta` channels. See
[Recover Streaming LLMs](/how-to/recover-streaming-llms/) for the four
recovery plans and `StreamCheckpointStore` wiring.

### Helper

```rust
impl AgentEvent {
    /// Extract the response text from a RunFinish result value.
    pub fn extract_response(result: &Option<Value>) -> String
}
```

## StreamEvent

Wire-format envelope that wraps an `AgentEvent` with sequencing metadata.
Sent over SSE as JSON.

```rust
pub struct StreamEvent {
    /// Monotonically increasing sequence number within a run.
    pub seq: u64,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// The wrapped agent event (flattened via #[serde(flatten)]).
    pub event: AgentEvent,
}
```

### Constructor

```rust
fn new(seq: u64, timestamp: impl Into<String>, event: AgentEvent) -> Self
```

## RunInput

Input to start or resume a run.

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunInput {
    /// A new user message to process.
    UserMessage { text: String },
    /// Resume a suspended run with a decision.
    ResumeDecision {
        tool_call_id: String,
        action: ResumeDecisionAction,
        payload: Value,       // omitted when null
    },
}
```

## RunOutput

Type alias for the event stream returned by a run:

```rust
pub type RunOutput = futures::stream::BoxStream<'static, AgentEvent>;
```

## TerminationReason

Why a run terminated. Serialized as `{ "type": "...", "value": ... }`.

```rust
pub struct StoppedReason {
    pub code: String,
    pub detail: Option<String>,
}

pub enum TerminationReason {
    NaturalEnd,
    BehaviorRequested,
    Stopped(StoppedReason),
    Cancelled,
    Blocked(String),
    Suspended,
    Error(String),
}
```

## ToolCallOutcome

```rust
pub enum ToolCallOutcome {
    Succeeded,
    Failed,
    Suspended,
}
```

## TokenUsage

```rust
pub struct TokenUsage {
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub total_tokens: Option<i32>,
    pub cache_read_tokens: Option<i32>,
    pub cache_creation_tokens: Option<i32>,
    pub thinking_tokens: Option<i32>,
}
```

All fields are omitted from JSON when `None`. `TokenUsage::default()` produces
all `None` values.

## Related

- [Run Lifecycle and Phases](/explanation/run-lifecycle-and-phases/)
