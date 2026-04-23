# 错误

Awaken 的错误类型统一基于 `thiserror`，并实现 `std::error::Error` 与 `Display`。

## StateError

状态系统相关错误，定义在 `awaken-contract`。

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

## ToolError

由 `Tool::validate_args` 或 `Tool::execute` 返回的错误。和 `ToolResult::error(...)` 不同，`ToolError` 会直接中止该次 tool call。

```rust,ignore
pub enum ToolError {
    InvalidArguments(String),
    ExecutionFailed(String),
    Denied(String),
    NotFound(String),
    Internal(String),
}
```

## BuildError

`AgentRuntimeBuilder::build()` 阶段的错误。

```rust,ignore
pub enum BuildError {
    State(StateError),
    AgentRegistryConflict(String),
    ToolRegistryConflict(String),
    ModelRegistryConflict(String),
    ProviderRegistryConflict(String),
    PluginRegistryConflict(String),
    ValidationFailed(String),
    DiscoveryFailed(DiscoveryError),
}
```

## RuntimeError

运行时执行错误，例如 agent 无法解析、同一 thread 重入运行等。

```rust,ignore
pub enum RuntimeError {
    State(StateError),
    ThreadAlreadyRunning { thread_id: String },
    AgentNotFound { agent_id: String },
    ResolveFailed { message: String },
}
```

## InferenceExecutionError

LLM 执行层错误。

```rust,ignore
pub enum InferenceExecutionError {
    Provider(String),
    RateLimited(String),
    Timeout(String),
    Cancelled,
}
```

## StorageError

`ThreadStore`、`RunStore`、`ThreadRunStore` 返回的错误。

```rust,ignore
pub enum StorageError {
    Validation(String),
    NotFound(String),
    AlreadyExists(String),
    VersionConflict { expected: u64, actual: u64 },
    Io(String),
    Serialization(String),
}
```

## ResolveError

agent 解析管线中的错误。

```rust,ignore
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

`RemoteAgentNotDirectlyRunnable` 只适用于通过 `AgentResolver::resolve()` 进行的直接本地解析。runtime root execution 使用 `ExecutionResolver`，只要注册了匹配的 backend factory，就可以运行 endpoint-backed agent。

## UnknownKeyPolicy

反序列化未知状态键时的策略。

```rust,ignore
pub enum UnknownKeyPolicy {
    Error,
    Skip,
}
```

## 相关

- [Tool Trait](./tool-trait.md)
