---
title: "Errors"
description: "All error types use thiserror derives and implement std::error::Error + Display."
---

All error types use `thiserror` derives and implement `std::error::Error` +
`Display`.

## StateError

Errors from state management operations. Defined in `awaken-runtime-contract`.

```rust
use awaken::Phase;

pub enum StateError {
    RevisionConflict { expected: u64, actual: u64 },
    MutationBaseRevisionMismatch { left: u64, right: u64 },
    PluginAlreadyInstalled { name: String },
    PluginNotInstalled { type_name: &'static str },
    KeyAlreadyRegistered { key: String },
    UnknownKey { key: String },
    KeyDecode { key: String, message: String },
    KeyEncode { key: String, message: String },
    HandlerAlreadyRegistered { key: String },
    EffectHandlerAlreadyRegistered { key: String },
    PhaseRunLoopExceeded { phase: Phase, max_rounds: usize },
    UnknownScheduledActionHandler { key: String },
    UnknownEffectHandler { key: String },
    ParallelMergeConflict { key: String },
    ToolAlreadyRegistered { tool_id: String },
    Cancelled,
}
```

**Crate path:** `awaken::StateError`

`StateError` implements `Clone` and `PartialEq`.

## ToolError

Errors returned from `Tool::validate_args` or `Tool::execute`. A `ToolError`
aborts the tool call entirely (as opposed to `ToolResult::error`, which sends
the failure back to the LLM).

```rust
pub enum ToolError {
    InvalidArguments(String),
    ExecutionFailed(String),
    /// Tool execution exceeded its deadline.
    Timeout(String),
    /// Tool execution cancelled (run cancelled, suspend cancel).
    Cancelled(String),
    Denied(String),
    NotFound(String),
    Internal(String),
}
```

**Crate path:** `awaken::contract::tool::ToolError`

## BuildError

Errors from `AgentRuntimeBuilder::build()`.

```rust
use awaken::StateError;

struct DiscoveryError;

pub enum BuildError {
    State(StateError),
    AgentRegistryConflict(String),
    ToolRegistryConflict(String),
    ModelRegistryConflict(String),
    ProviderRegistryConflict(String),
    PluginRegistryConflict(String),
    ValidationFailed(String),
    DiscoveryFailed(DiscoveryError),     // requires feature "a2a"
}
```

**Crate path:** `awaken::BuildError`

`BuildError` converts from `StateError` via `From`.

## RuntimeError

Errors from agent runtime operations (resolving agents, starting runs).

```rust
use awaken::StateError;

pub enum RuntimeError {
    State(StateError),
    ThreadAlreadyRunning { thread_id: String },
    AgentNotFound { agent_id: String },
    ResolveFailed { message: String },
}
```

**Crate path:** `awaken::RuntimeError`

`RuntimeError` converts from `StateError` via `From`. Implements `Clone` and
`PartialEq`.

## InferenceExecutionError

Errors from the LLM execution layer. Variants split into three recoverability
classes:

- **Transient** — retryable and counted toward the per-model circuit breaker.
- **Permanent** — not retryable and not counted toward the circuit breaker;
  these would have failed with the same error on any model.
- **Fail-fast** — the retry subsystem cannot or should not try again.

The enum is `#[non_exhaustive]`. Code outside the crate must handle a
`_ => …` arm and should prefer the `is_retryable()`,
`counts_toward_circuit_breaker()`, and `retry_after()` accessors over matching
specific variants.

```rust
use std::time::Duration;
use awaken::contract::executor::{InterruptCause, InterruptSnapshot};

#[non_exhaustive]
pub enum InferenceExecutionError {
    // Transient
    Provider(String),
    RateLimited { message: String, retry_after: Option<Duration> },
    Overloaded  { message: String, retry_after: Option<Duration> },
    Timeout(String),
    StreamInterrupted { cause: InterruptCause, snapshot: Box<InterruptSnapshot> },

    // Permanent
    ContextOverflow(String),
    InvalidRequest(String),
    Unauthorized(String),
    ModelNotFound(String),
    ContentFiltered(String),

    // Fail-fast
    AllModelsUnavailable,
    Cancelled,
}
```

| Class | Variants |
|---|---|
| Transient (retryable) | `Provider`, `RateLimited`, `Overloaded`, `Timeout`, `StreamInterrupted` |
| Permanent (not retryable) | `ContextOverflow`, `InvalidRequest`, `Unauthorized`, `ModelNotFound`, `ContentFiltered` |
| Fail-fast | `AllModelsUnavailable`, `Cancelled` |

`RateLimited` and `Overloaded` carry an optional `retry_after` parsed from the
provider's `Retry-After` header. The retry subsystem honors that hint before
falling back to exponential backoff.

`StreamInterrupted` carries an `InterruptCause`
(`ConnectionReset`, `IdleStall`, `GoAway`, or `Provider5xxMidStream(u16)`) and
an `InterruptSnapshot` capturing the partial assistant text, completed tool
calls, and the open tool whose arguments had not finished arriving. The loop
runner consumes the snapshot to choose one of four recovery plans; see
[Recover Streaming LLMs](/awaken/how-to/recover-streaming-llms/).

### Convenience accessors

```rust
fn is_retryable(&self) -> bool;
fn counts_toward_circuit_breaker(&self) -> bool;
fn retry_after(&self) -> Option<std::time::Duration>;

// Short constructors for common cases:
fn rate_limited(message: impl Into<String>) -> Self;
fn overloaded(message: impl Into<String>) -> Self;
```

**Crate path:** `awaken::contract::executor::InferenceExecutionError`

## StorageError

Errors returned by `ThreadStore`, `RunStore`, and `ThreadRunStore` operations.

```rust
pub enum StorageError {
    Validation(String),
    NotFound(String),
    AlreadyExists(String),
    VersionConflict { expected: u64, actual: u64 },
    Io(String),
    /// Commit may have persisted durably but follow-up promotion / cache work
    /// outcome is unknown to the caller (idempotent retry is safe).
    CommitUnknown(String),
    Serialization(String),
}
```

**Crate path:** `awaken::contract::storage::StorageError`

## ResolveError

Errors from the agent resolution pipeline (resolving `AgentSpec` to a runnable
`ResolvedAgent` or backend-backed execution plan).

```rust
use awaken::StateError;

pub enum ResolveError {
    AgentNotFound(String),
    ModelNotFound(String),
    ProviderNotFound(String),
    PluginNotFound(String),
    InvalidPluginConfig { plugin: String, key: String, message: String },
    UnsupportedRemoteBackend { agent_id: String, backend: String },
    InvalidRemoteEndpointConfig { agent_id: String, backend: String, message: String },
    RemoteAgentNotDirectlyRunnable(String),
    ToolIdConflict { tool_id: String, source_a: String, source_b: String },
    EnvBuild(StateError),
}
```

**Crate path:** `awaken::registry::resolve::ResolveError`

`RemoteAgentNotDirectlyRunnable` applies to direct local resolution through
`AgentResolver::resolve()`. Runtime run resolution uses `ResolvedRunPlan` and
can run endpoint-backed agents when a matching backend factory is registered.

## UnknownKeyPolicy

Controls behavior when encountering an unknown state key during deserialization.

```rust
pub enum UnknownKeyPolicy {
    Error,
    Skip,
}
```

**Crate path:** `awaken::UnknownKeyPolicy`

## Related

- [Tool Trait Reference](/awaken/reference/tool-trait/)
