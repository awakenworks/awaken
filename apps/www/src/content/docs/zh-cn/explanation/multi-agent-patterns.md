---
title: "多智能体模式"
description: "Awaken 支持多种 agent 组合方式，包括本地委托、远程 A2A agent、后台 agent、agent 间通信、sub-agent 执行以及 handoff。"
---

Awaken 支持多种 agent 组合方式，包括本地委托、远程 A2A agent、后台 agent、agent 间通信、sub-agent 执行以及 handoff。

## 目的

本页用于在写代码前选择组合模型。好的模式应让 ownership、返回路径、state 流动和 cancellation 都是显式的；这比把所有专家能力都塞进同一段隐藏上下文更安全。

| 模式 | 目的 | 为什么这样更好 |
|---|---|---|
| 把 delegate agent 暴露成 tool | 运行一个专家并把边界清晰的结果返回给父 agent | 父 agent 看到普通 tool result，仍保留最终控制权。 |
| 代码式 sub-agent | 由自定义 Rust 决定 seed/export、streaming 和 status 策略 | state 流动是类型化、可审计的，而不是隐式发生。 |
| 后台任务或后台 agent | 让长任务跨 step 边界继续运行 | loop 可以等待、取消、恢复并消费 inbox 事件。 |
| `send_message` 通信 | 让独立 agent 互相发消息 | 路由明确：实时 child inbox 或持久 mailbox。 |
| Handoff | 让另一个 agent 接管当前 thread | 对话和 state 保持连续，不需要另起并行 run。 |

## 通过 AgentSpec.delegates 进行 agent 委托

agent 可以通过 `delegates` 声明它允许委托的子 agent：

```json
{
  "id": "orchestrator",
  "model_id": "gpt-4o",
  "system_prompt": "You coordinate tasks across specialized agents.",
  "delegates": ["researcher", "writer", "reviewer"]
}
```

解析时,运行时会为每个 delegate 生成一个 `AgentTool`,LLM 看见的是普通工具,例如 `agent_run_researcher`。

`AgentTool` 持有 `Arc<dyn AgentResolver>`，**不**预选 backend。LLM 调用该工具时，`execute()` 在 **call time** 调 `resolver.resolve_execution(&agent_id)`，resolver 按每次调用决定：

- **本地 agent**(无 `endpoint`)解析为本地 `ResolvedAgent`,在同一 runtime 内联执行。
- **远程 agent**(有 `endpoint`)解析为 `ResolvedBackendAgent`,经配置的 `ExecutionBackend`(当前是 A2A)执行 —— 发 `message:send`,然后轮询返回的 task 直到完成。

因为解析推迟到 call time,通过配置 API 改 delegate 的 `AgentSpec`(例如翻转其 `endpoint`)在下一次工具调用立刻生效,无需重建父 agent。

## 通过 A2A 使用远程 agent

如果 `AgentSpec.endpoint` 存在，该 delegate 会被当作远程 A2A agent：

```json
{
  "id": "remote-analyst",
  "model_id": "unused-for-remote",
  "system_prompt": "",
  "endpoint": {
    "backend": "a2a",
    "base_url": "https://analyst.example.com/v1/a2a",
    "auth": { "type": "bearer", "token": "token-abc" },
    "target": "analyst",
    "timeout_ms": 300000,
    "options": {
      "poll_interval_ms": 1000
    }
  }
}
```

`A2aBackend` 会发送 `message:send`，读取返回的 `task.id` 并轮询任务状态，再把最终结果包装成 `BackendRunResult` / `ToolResult` 返回给父 agent。远端超时、失败、等待输入、等待认证和取消会保留在 `BackendRunStatus` 中。

## 在工具里以代码方式调用 Sub-Agent

当你写一个自定义 `Tool` 需要委托给另一个 agent，特别是需要严格控制父 ↔ 子 state 流动时，使用 `awaken_runtime::child_agent` 的 [`run_child_agent`](/awaken/zh-cn/how-to/invoke-sub-agent-from-tool/)。它与自动生成的 `AgentTool` 是同级关系——两者都构建一个 `BackendDelegateRunRequest` 并调用同一个 `execute_resolved_delegate_execution` 分发;而 `run_streaming_subagent` 是对 `run_child_agent` 的薄封装。

`run_child_agent` 通过 `initial_state_seed: Option<PersistedState>` 接受父→子的初始状态注入，并通过 `BackendRunResult.state`（一个 `PersistedState`）把子的终态返回给父工具去解码并以 `StateCommand` 形式塞进 `ToolOutput`。子→父 state 走的是普通工具就在用的 `ToolOutput.command` 通路，没有单独的"sub-agent 导出"机制。

state seeding **只对 Local backend 生效**。它不是 `BackendProfile` 里的一个
capability flag：只要解析出的 `ExecutionPlan` 不是本地执行，
`validate_delegate_execution_request` 就会拒绝
`BackendDelegateRunRequest.state_seed`。A2A 和自定义远程 backend 没有统一的
seed wire 协议，因此带 seed 的 delegate 请求会以 `ExecutionBackendError` 失败，
而不是静默丢弃 seed。只要 backend 返回了结果，子的 `BackendRunResult.state`
仍然可读，用于回写父侧。

Backend 实现者应使用 `BackendProfile` 表达 continuation、persistence、waits、
transcript shape 和 output shape 等 typed capabilities。父→子 state seed 是这个
profile 之外的本地执行规则。

## 后台任务与后台 Agent

当工作不应阻塞当前 model step 时，使用后台任务：轮询外部 job、监听事件流、执行长分析，或等待人/系统输入。注册 `BackgroundTaskPlugin`，并在 tool 中调用 `BackgroundTaskManager::spawn(...)`。manager 会把任务元数据持久化到 `BackgroundTaskStateKey`，提供 cancellation，并把完成/自定义事件送回拥有该 thread 的 inbox。

当长任务本身就是一个需要持续可寻址的 agent loop 时，使用后台 agent。`BackgroundTaskManager::spawn_agent_with_context(...)` 会创建带 inbox 的 `sub_agent` task，父 agent 可以在子 agent 运行期间继续发送消息。相比同步 delegation，这更适合 child 需要多轮、后续数据或父侧已继续推进后仍需取消的场景。

后台工作不是业务 state 传递的替代品。任务状态是生命周期元数据；父子双方需要共享结构化业务 state 时，仍应使用类型化 `StateKey` seed/export 策略。

## Agent 之间的通信

当 agent 需要通信但不应该合并执行上下文时，暴露 `SendMessageTool`。这个工具使用统一 schema，并按 recipient 自动选择 transport：

- `child`：按 name 或 task ID 发到实时后台 child task inbox。
- `parent`：通过 host 的 durable message sink 发到父 thread。
- `agent`：通过同一个 durable sink 发到另一个 thread/agent。

这比共享可变内存更好，因为每条消息都有明确 recipient、sender、receipt 和失败模式。低延迟的进程内协作用 live child messaging；跨 thread、跨进程或跨 worker 的通信用 mailbox-backed durable messaging。

## Sub-Agent 模式

### 串行委托

```text
Orchestrator -> researcher -> result
             -> writer     -> result
             -> reviewer   -> result
```

父 agent 按顺序调用子 agent，并根据前一步结果决定下一步。

### 并行委托

如果 LLM 在同一轮推理里一次返回多个 delegate tool call，它们会使用和普通工具相同的 `ToolExecutor`。内置 resolver 默认安装 `SequentialToolExecutor`，因此委托默认逐个执行。如果这些 delegate call 相互独立，并且需要并发执行，可以通过自定义 resolver 或 `ResolvedAgent::with_tool_executor(...)` 安装 `ParallelToolExecutor`。

### 嵌套委托

```text
orchestrator
  -> team_lead (delegates: [dev_a, dev_b])
       -> dev_a
       -> dev_b
```

每一层都独立通过 `AgentResolver` 解析。理论上没有硬深度限制，但每层都会增加 token 和延迟成本。

## Agent Handoff

handoff 会在同一 run 内把控制权切换给另一个 agent，而不是把它当成子任务调用。

机制：

1. 插件或 handoff 扩展把新 agent ID 写入活动 agent 状态键
2. loop runner 在下一个步骤边界检测到变化
3. 重新通过 `AgentResolver` 解析 agent
4. 在同一个 thread、同一条消息历史上继续执行

### Handoff 与 Delegation 的区别

| 方面 | Delegation | Handoff |
|--------|-----------|---------|
| 控制流 | 父 agent 调子 agent，拿回结果 | 控制权直接切到新 agent |
| Thread 连续性 | 子 agent 可以有独立上下文 | 同一 thread、同一消息历史 |
| 返回路径 | 结果回到父 agent | 不返回，后续由新 agent 接管 |
| 适用场景 | 任务拆解、专长子任务 | 角色切换、升级处理、路由 |

## ExecutionBackend Trait

root execution 和 delegation 都基于 canonical `ExecutionBackend`：

```rust
pub trait ExecutionBackend: Send + Sync {
    fn capabilities(&self) -> BackendProfile;

    async fn abort(&self, request: BackendAbortRequest<'_>)
        -> Result<(), ExecutionBackendError>;

    async fn execute_root(
        &self,
        request: BackendRootRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError>;

    async fn execute_delegate(
        &self,
        request: BackendDelegateRunRequest<'_>,
    ) -> Result<BackendRunResult, ExecutionBackendError>;
}
```

`BackendRunResult` 保留 agent ID、状态、终止原因、可选响应文本、结构化输出、run ID、inbox、run-scoped 持久化状态，以及可选的 thread-scoped 持久化状态。`BackendRunStatus` 包含 `Completed`、`WaitingInput`、`WaitingAuth`、`Suspended`、`Failed`、`Cancelled` 和 `Timeout`。

这也是实现自定义本地或远程执行后端的扩展点。`awaken_runtime::extensions::a2a` 仍然重新导出 `AgentBackend`、`AgentBackendFactory` 和 `DelegateRunResult` 作为兼容别名，但新代码应使用 `ExecutionBackend` 命名。

## 另见

- [在工具里调用 Sub-Agent](/awaken/zh-cn/how-to/invoke-sub-agent-from-tool/) —— `run_child_agent` 操作指南
- [A2A 协议](/awaken/zh-cn/reference/protocols/a2a/)
- [架构](/awaken/zh-cn/explanation/architecture/)
- [Tool 与 Plugin 的边界](/awaken/zh-cn/explanation/tool-and-plugin-boundary/)
