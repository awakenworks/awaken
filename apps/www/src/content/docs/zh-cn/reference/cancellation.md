---
title: "取消"
description: "Awaken 使用协作式取消机制来中断 agent run、流式输出和长时间运行的操作。"
---

Awaken 使用协作式取消机制来中断 agent run、流式输出和长时间运行的操作。

## CancellationToken

`CancellationToken` 是一个可克隆的句柄，底层由共享的 `AtomicBool` 与 `tokio::sync::Notify` 组成。任何一个 clone 调用 `cancel()`，其他 clone 都会立刻观察到取消状态。

```rust
use awaken::CancellationToken;

let token = CancellationToken::new();
```

### 方法

```rust
pub fn new() -> Self
pub fn cancel(&self)
pub fn is_cancelled(&self) -> bool
pub async fn cancelled(&self)
```

### Trait

- `Clone`
- `Default`

## 同步轮询

在同步代码或紧密循环里，可以使用 `is_cancelled()`：

```rust
let token = CancellationToken::new();

while !token.is_cancelled() {
    // do work
}
```

## 配合 tokio::select! 的异步等待

```rust
let token = CancellationToken::new();

tokio::select! {
    result = some_async_work() => {
        // work completed before cancellation
    }
    _ = token.cancelled() => {
        // cancellation was signalled
    }
}
```

## 协作式语义

取消不会强杀任务。`cancel()` 只会设置标志位并唤醒等待者，具体执行逻辑仍需要主动检查 `is_cancelled()` 或在 `select!` 中监听 `cancelled()`。

关键特性：

- 幂等：重复调用 `cancel()` 是安全的。
- 共享：所有 clone 看到的是同一份状态。
- 可见性：原子标志使用强顺序，跨线程立即可见。
- 立即唤醒：`cancel()` 会通知所有等待者。

## 运行时里的用法

运行时会给每个 run 传入 `CancellationToken`，用于：

- 中断流式推理
- 在 phase 之间提前停止 run
- 把 HTTP / SSE / mailbox 层的取消请求传播到核心运行时

```rust
let token = CancellationToken::new();
let clone = token.clone();

tokio::select! {
    _ = async {
        while let Some(chunk) = stream.next().await {
            // process chunk
        }
    } => {}
    _ = clone.cancelled() => {
        // stop processing, clean up
    }
}
```

## 新消息到来时的自动取消

同一 thread 上已经有活动 run 时，如果又来了新的输入，mailbox 会先取消旧 run，再启动新 run。这样可以避免 `ThreadAlreadyRunning`，也避免挂起 run 与新输入互相干扰。

大致顺序是：

1. `Mailbox::submit()` 发现该 thread 已有活动 run。
2. 调用 `cancel_and_wait_by_thread()` 触发取消并等待线程槽位释放。
3. 旧 run 以 `TerminationReason::Cancelled` 结束。
4. 新 run 启动前清理不成对的 tool call 历史，避免污染上下文。

## 关键文件

- `crates/awaken-runtime/src/cancellation.rs`
- `crates/awaken-runtime/src/runtime/agent_runtime/active_registry.rs`
- `crates/awaken-runtime/src/runtime/agent_runtime/runner.rs`
