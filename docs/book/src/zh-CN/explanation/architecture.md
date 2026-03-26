# 架构

本文解释 Awaken 的整体架构、核心抽象及其协作方式。

## 分层概览

```text
┌─────────────────────────────────────────────────────┐
│  应用层                                              │
│  - 注册工具、定义智能体、调用 run_stream             │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  AgentRuntime                                        │
│  - 解析智能体、执行阶段、发射事件                     │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  Thread + 状态引擎                                   │
│  - 会话历史、快照隔离、状态键                         │
└─────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────┐
│  存储层                                              │
│  - InMemoryStore / FileStore / PostgresStore         │
└─────────────────────────────────────────────────────┘
```

应用层注册工具和智能体，通过 `AgentRuntime` 提交 `RunRequest`。运行时解析智能体配置，加载或创建会话线程，然后执行阶段循环。状态引擎提供快照隔离的读写访问。存储层持久化线程历史和运行记录。

## 阶段执行模型

每次 Run 由 N 个 Step 组成，每个 Step 是一次推理 + 工具执行循环：

```text
RunStart → [StepStart → BeforeInference → [推理] → AfterInference
            → (如有工具调用) BeforeToolExecute → [执行] → AfterToolExecute
            → StepEnd] × N → RunEnd
```

共 8 个阶段。每个阶段按两步执行：

1. **GATHER**：所有注册的 `PhaseHook` 看到同一份冻结快照，返回 `StateCommand`（状态变更 + 调度动作 + 效果）。
2. **EXECUTE**：合并所有 `StateCommand`，提交 `MutationBatch`，处理已调度的动作，派发效果。

快照隔离保证 GATHER 阶段内的钩子互不影响。`MergeStrategy` 决定并发写入同一键时的合并行为：`Exclusive` 要求单一写入者，`Commutative` 允许关联合并。

## 四大机制

系统恰好有四种机制。所有插件交互、控制流决策和外部操作都必须使用其中之一：

### State：持久化可观测数据

所有共享数据的唯一真实来源。通过 `StateKey` 存储的值在各阶段和步骤间持久存在，直到被 reducer 显式修改。状态不用作瞬态队列。

- 变更路径：`StateCommand.update::<K>(update)` -> `MutationBatch` -> `StateStore.commit()`
- 读取始终通过不可变 `Snapshot`，不暴露可变引用。
- 快照是 `Arc<StateMap>`：多个读者可以持有快照而不阻塞写者。

### Action：延迟一次性工作

在特定阶段调度工作的唯一机制。通过 `StateCommand.schedule_action::<A>(payload)` 创建，存储在 `PendingScheduledActions` 队列中，在目标阶段的 EXECUTE 步骤中消费。动作恰好消费一次。

### Effect：即发即忘的外部 I/O

用于不需要观测结果的不可逆外部操作。通过 `StateCommand.emit::<E>(payload)` 发出，在每次提交后立即派发。效果处理器是终端性的：不返回 `StateCommand`，不产生新动作或新效果。

### 状态机：生命周期控制

Run 和工具调用的生命周期建模为 `StateKey`，具有定义良好的状态机。循环运行器在阶段边界检查这些状态机以做出控制流决策。

**RunLifecycle** 状态机：

```text
Running <-> Waiting -> Done
```

- `Running`：正在执行步骤。
- `Waiting`：已挂起，等待外部决策（工具审批、用户输入）。
- `Done`：终态，携带原因（NaturalEnd / Stopped / BehaviorRequested / Cancelled / Error）。

终止条件实现为普通插件的 `AfterInference` 钩子，通过 `StateCommand` 写入 `RunLifecycleUpdate::Done`。这使得终止条件可替换和可扩展，无需修改循环本身。

## Crate 结构

| Crate | 职责 |
|-------|------|
| `awaken-contract` | 核心契约：类型定义、trait、状态模型、`AgentSpec` |
| `awaken-runtime` | 执行引擎：阶段循环、插件系统、`AgentRuntime`、构建器 |
| `awaken-server` | HTTP/SSE 网关，支持 AI SDK v6、AG-UI、A2A 协议适配 |
| `awaken-stores` | 存储后端实现 |
| `awaken` | 门面 crate，重新导出核心模块 |

`awaken-contract` 定义所有 trait 和类型，不包含实现逻辑。`awaken-runtime` 实现执行引擎并依赖 `awaken-contract`。`awaken-server` 在运行时之上提供 HTTP 端点。这种分层确保核心契约可以独立于执行引擎使用。

## 插件系统

插件通过 `PluginRegistrar` 注册以下能力：

- **阶段钩子**：在特定阶段执行的异步函数。
- **状态键**：声明插件拥有的状态键及其作用域和合并策略。
- **调度动作处理器**：处理插件定义的动作类型。
- **效果处理器**：处理插件定义的效果类型。
- **工具**：注册插件提供的工具。

插件在构建 `ExecutionEnv` 时激活。激活顺序决定同一阶段内钩子的执行顺序。

## 存储抽象

`ThreadRunStore` trait 定义了持久化接口：

- 加载和保存 `Thread`（消息历史）。
- 加载和保存 `Run`（执行记录）。
- 加载和保存 `Checkpoint`（状态快照）。

内置实现包括 `InMemoryStore`（开发用）、`FileStore`（本地文件系统）和 `PostgresStore`（生产环境）。通过 `AgentRuntime::with_thread_run_store()` 注入。

## 多协议服务器

`awaken-server` 通过协议适配器支持多种前端集成：

- **AI SDK v6**：Vercel AI SDK 兼容的 SSE 流。
- **AG-UI**：CopilotKit 的 Agent-UI 协议。
- **A2A**：Google 的 Agent-to-Agent 协议，用于多智能体互操作。
- **ACP**：Agent Communication Protocol。

所有协议共享同一个 `AgentRuntime` 实例。协议适配器负责将外部请求转换为 `RunRequest`，并将 `AgentEvent` 流转换为协议特定的响应格式。

## 另见

- [状态与快照模型](../../explanation/state-and-snapshot-model.md)
- [Run 生命周期与阶段](../../explanation/run-lifecycle-and-phases.md)
- [HITL 与邮箱](../../explanation/hitl-and-mailbox.md)
- [HTTP API](../../reference/http-api.md)
- [事件](../../reference/events.md)
