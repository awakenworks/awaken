---
title: "错误"
description: "Awaken 的错误类型统一基于 thiserror，并实现 std::error::Error 与 Display。"
---

Awaken 的错误类型统一基于 `thiserror`，并实现 `std::error::Error` 与 `Display`。

## StateError

状态系统相关错误，定义在 `awaken-runtime-contract`。

```rust
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

```rust
pub enum ToolError {
    InvalidArguments(String),
    ExecutionFailed(String),
    /// 工具执行超过截止时间。
    Timeout(String),
    /// 工具执行被取消(run 取消、suspend cancel)。
    Cancelled(String),
    Denied(String),
    NotFound(String),
    Internal(String),
}
```

## BuildError

`AgentRuntimeBuilder::build()` 阶段的错误。

```rust
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

```rust
pub enum RuntimeError {
    State(StateError),
    ThreadAlreadyRunning { thread_id: String },
    AgentNotFound { agent_id: String },
    ResolveFailed { message: String },
}
```

## InferenceExecutionError

LLM 执行层错误。变体按可恢复性分为三类：

- **Transient（可重试）**：会被重试子系统再次发起，且计入每模型的熔断器计数。
- **Permanent（不可重试）**：换模型也会失败，不计入熔断器。
- **Fail-fast**：重试子系统已经无法（或不应该）再尝试。

枚举为 `#[non_exhaustive]`。crate 外部的代码必须包含 `_ => …` 分支，并优先使
用 `is_retryable()`、`counts_toward_circuit_breaker()`、`retry_after()` 三个
访问器，而不是直接对具体变体做模式匹配。

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

| 类别 | 变体 |
|---|---|
| Transient（可重试） | `Provider`、`RateLimited`、`Overloaded`、`Timeout`、`StreamInterrupted` |
| Permanent（不可重试） | `ContextOverflow`、`InvalidRequest`、`Unauthorized`、`ModelNotFound`、`ContentFiltered` |
| Fail-fast | `AllModelsUnavailable`、`Cancelled` |

`RateLimited` 与 `Overloaded` 携带从 provider `Retry-After` 头解析得到的可选
`retry_after`；retry 子系统会优先尊重该提示，再回退到指数退避。

`StreamInterrupted` 携带 `InterruptCause`（`ConnectionReset`、`IdleStall`、
`GoAway`、`Provider5xxMidStream(u16)`）以及 `InterruptSnapshot`，里面记录了
中断时的 partial 文本、已完成的 tool call，以及参数尚未完成的 in-flight tool。
loop runner 据此选择四种恢复方案之一，详见
[流式 LLM 错误恢复](/awaken/zh-cn/how-to/recover-streaming-llms/)。

### 便捷访问器

```rust
fn is_retryable(&self) -> bool;
fn counts_toward_circuit_breaker(&self) -> bool;
fn retry_after(&self) -> Option<std::time::Duration>;

// 常用短构造：
fn rate_limited(message: impl Into<String>) -> Self;
fn overloaded(message: impl Into<String>) -> Self;
```

**Crate 路径：** `awaken::contract::executor::InferenceExecutionError`

## StorageError

`ThreadStore`、`RunStore`、`ThreadRunStore` 返回的错误。

```rust
pub enum StorageError {
    Validation(String),
    NotFound(String),
    AlreadyExists(String),
    VersionConflict { expected: u64, actual: u64 },
    Io(String),
    /// commit 可能已经持久化,但后续 promotion / cache 工作结果对调用方未知
    /// (幂等重试是安全的)。
    CommitUnknown(String),
    Serialization(String),
}
```

## ResolveError

agent 解析管线中的错误。

```rust
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

`RemoteAgentNotDirectlyRunnable` 只适用于通过 `AgentResolver::resolve()` 进行的直接本地解析。runtime run resolution 使用 `ResolvedRunPlan`；只要注册了匹配的 backend factory，就可以运行 endpoint-backed agent。

## UnknownKeyPolicy

反序列化未知状态键时的策略。

```rust
pub enum UnknownKeyPolicy {
    Error,
    Skip,
}
```

## 相关

- [Tool Trait](/awaken/zh-cn/reference/tool-trait/)
