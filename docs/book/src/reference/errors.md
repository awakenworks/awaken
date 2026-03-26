# Errors

All error types use `thiserror` derives and implement `std::error::Error` +
`Display`.

## StateError

Errors from state management operations. Defined in `awaken-contract`.

```rust,ignore
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

```rust,ignore
pub enum ToolError {
    InvalidArguments(String),
    ExecutionFailed(String),
    Denied(String),
    NotFound(String),
    Internal(String),
}
```

**Crate path:** `awaken::contract::tool::ToolError`

## BuildError

Errors from `AgentRuntimeBuilder::build()`.

```rust,ignore
pub enum BuildError {
    State(StateError),
    AgentRegistryConflict(String),
    StopPolicyConflict(String),
    ValidationFailed(String),
    DiscoveryFailed(DiscoveryError),     // requires feature "a2a"
}
```

**Crate path:** `awaken::BuildError`

`BuildError` converts from `StateError` via `From`.

## RuntimeError

Errors from agent runtime operations (resolving agents, starting runs).

```rust,ignore
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

Errors from the LLM execution layer.

```rust,ignore
pub enum InferenceExecutionError {
    Provider(String),
    RateLimited(String),
    Timeout(String),
    Cancelled,
}
```

**Crate path:** `awaken::contract::executor::InferenceExecutionError`

## StorageError

Errors returned by `ThreadStore`, `RunStore`, and `ThreadRunStore` operations.

```rust,ignore
pub enum StorageError {
    NotFound(String),
    AlreadyExists(String),
    VersionConflict { expected: u64, actual: u64 },
    Io(String),
    Serialization(String),
}
```

**Crate path:** `awaken::contract::storage::StorageError`

## ResolveError

Errors from the agent resolution pipeline (resolving `AgentSpec` to a runnable
`ResolvedAgent`).

```rust,ignore
pub enum ResolveError {
    AgentNotFound(String),
    ModelNotFound(String),
    ProviderNotFound(String),
    PluginNotFound(String),
    InvalidPluginConfig { plugin: String, key: String, message: String },
    RemoteAgentNotDirectlyRunnable(String),
    ToolIdConflict { tool_id: String, source_a: String, source_b: String },
    EnvBuild(StateError),
}
```

**Crate path:** `awaken::registry::resolve::ResolveError`

## UnknownKeyPolicy

Controls behavior when encountering an unknown state key during deserialization.

```rust,ignore
pub enum UnknownKeyPolicy {
    Error,
    Skip,
}
```

**Crate path:** `awaken::UnknownKeyPolicy`

## Related

- [Tool Trait Reference](./tool-trait.md)
