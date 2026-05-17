---
title: "Recover Streaming LLMs"
description: "Use this when transient provider failures during a streaming inference call must not surface as run errors. The runtime retries whole requests that fail *before* streaming starts; this page is about…"
---

Use this when transient provider failures during a streaming inference call
must not surface as run errors. The runtime retries whole requests that fail
*before* streaming starts; this page is about the harder case — failures that
arrive *after* the model already started producing tokens.

## What the runtime handles for you

The streaming inference path detects four mid-stream interruption causes
through `InferenceExecutionError::StreamInterrupted` and the
`InterruptCause` enum:

- `ConnectionReset` (TCP/HTTP/2 connection dropped after headers)
- `IdleStall` (no bytes received within the idle window)
- `GoAway` (HTTP/2 GOAWAY frame mid-response)
- `Provider5xxMidStream(u16)` (provider returned a 5xx after streaming began)

When any of these fires, the loop runner consults
`InterruptSnapshot::plan()` and picks one of four recovery plans. The naming
in code is `R1..R4`:

| Plan | When it fires | What the runtime does |
|---|---|---|
| **R1 — `ContinueText`** | Only text accumulated, no tool calls in flight | Retries with the accumulated text as an assistant prefix and a continuation prompt; the model picks up where it left off |
| **R2 — `SynthesizeToolUse`** | At least one tool call had complete argument JSON | Synthesizes a `StopReason::ToolUse` terminal state so the loop runner executes the completed tools; any unfinished tool is captured as a hint and surfaced to the model on the next user message |
| **R3 — `TruncateBeforeTool`** | Text plus a single unclosed tool call | Truncates to the text prefix, emits `AgentEvent::ToolCallCancel` so consumers drop the partial argument delta, then continues |
| **R4 — `WholeRestart`** | Nothing salvageable (no text, no completed tools) | Restarts the assistant turn from scratch; emits `AgentEvent::StreamReset` so consumers discard already-emitted deltas |

`Retry-After` is honored: when the provider returns `429` or `529` with a
`Retry-After` header, `InferenceExecutionError::RateLimited` and
`Overloaded` carry the parsed `Duration` and the retry subsystem waits at
least that long before retrying.

## What clients see

SSE consumers receive normal `TextDelta` and `ToolCallDelta` events during the
recovered turn. Two new events tell consumers what to drop:

- `ToolCallCancel { id, name, reason }` — drop any buffered partial delta for
  this tool call.
- `StreamReset { reason }` — discard *all* deltas for the current assistant
  turn; new deltas follow.

Both events are advisory. They never appear in the durable thread log;
clients that re-render from `MessagesSnapshot` do not need to special-case
them.

## Cross-process resume

A single-process retry loop is enough when the same server stays up through
the interruption. Cross-process resume — picking up from where a previous
*process* left off — uses the `StreamCheckpointStore` contract.

```rust
use std::sync::Arc;
use awaken::contract::stream_checkpoint::{
    InMemoryStreamCheckpointStore, StreamCheckpoint, StreamCheckpointStore,
};

#[async_trait::async_trait]
trait StreamCheckpointStore: Send + Sync {
    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), _>;
    async fn get(&self, run_id: &str) -> Result<Option<StreamCheckpoint>, _>;
    async fn delete(&self, run_id: &str) -> Result<(), _>;
}
```

While a stream runs, the loop runner periodically writes the accumulated
`partial_text`, `completed_tool_calls`, and the open `in_flight_tool` to the
store under the run's `run_id`. When a fresh process picks up that run, the
checkpoint is read at the start of `execute_streaming` and translated into the
same R1 prefix-injection that the in-process retry loop uses.

The checkpoint is **not** a full conversation log — committed messages are
still owned by `ThreadRunStore`. The checkpoint only captures the in-flight
delta accumulator, which is why the contract is small (`put`, `get`,
`delete`).

### Attaching a store to an agent

The store lives on `ResolvedAgent::stream_checkpoint_store`, populated through
the builder method:

```rust
use awaken::contract::stream_checkpoint::{
    InMemoryStreamCheckpointStore, StreamCheckpointStore,
};
use std::sync::Arc;

let store: Arc<dyn StreamCheckpointStore> =
    Arc::new(InMemoryStreamCheckpointStore::new());

let resolved = resolved.with_stream_checkpoint_store(store);
```

The default resolver pipeline leaves the field as `None`. To make every
resolution carry the store, wrap your `AgentResolver` so it decorates the
returned `ResolvedAgent` with `with_stream_checkpoint_store(store.clone())`
before handing it to the runtime. `AgentRuntimeBuilder` does not yet expose a
direct shortcut for this; track the open builder integration in
[GitHub issues](https://github.com/AwakenWorks/awaken/issues) if you need it.

The shipped `InMemoryStreamCheckpointStore` is fine for tests and for
single-process operation. For true cross-process resume, implement the trait
on a shared backend (NATS JetStream KV, Redis, a filesystem path, etc.). Each
`put` should idempotently upsert the checkpoint for `run_id`; `delete` runs
after the turn commits.

## What this does not do

- It does not retry permanent errors. `ContextOverflow`, `InvalidRequest`,
  `Unauthorized`, `ModelNotFound`, and `ContentFiltered` short-circuit the
  retry subsystem and propagate to the caller.
- It does not repair malformed-but-not-truncated tool call JSON. That is a
  separate concern; the recovery snapshot only classifies arguments as
  "completed" when they parse as JSON.
- It does not fold back into the durable message log on its own. The
  re-emitted deltas in the recovered turn produce the same final assistant
  message that a fresh run would have produced; checkpoint cleanup happens
  after that message is committed.

## Related

- [Errors](/reference/errors/) for the full `InferenceExecutionError`
  taxonomy and accessors.
- [Events](/reference/events/) for `ToolCallCancel` / `StreamReset`
  semantics.
- [Optimize the Context Window](/how-to/optimize-context-window/) for the
  separate truncation-recovery path used when the model itself stops with
  `MaxTokens`.
