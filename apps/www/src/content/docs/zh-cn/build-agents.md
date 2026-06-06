---
title: "开发 Agent 路径"
description: "在 Rust 中实现可执行 Agent 能力：runtime setup、tool、plugin、state 和受控 sub-agent 调用。"
---

这条路径对应 Awaken 的开发侧：实现 runtime 可以安全执行的能力。代码聚焦 tool、
plugin、state、provider、store 和明确执行边界。后续应由运营者调整的行为放进
托管配置，再进入 [调优与运营](/awaken/zh-cn/operate/) 使用浏览器和 REST 工作流。

## 目的

Build Agents 先说明**为什么某项能力应该写进代码**，再把可运营调优的部分交给
配置。这样能把高成本、强安全边界的决策留在经过 review 的 Rust 中，同时让运营者
后续清楚地调 prompt、tool、permission 和 governance。

## 需要提前确定的设计选择

| 需求 | 放在这里 | 为什么这样更好 |
|---|---|---|
| 长时间运行且不应阻塞当前 turn 的工作 | 后台任务或后台 agent | run 可以等待、恢复或接收 inbox 事件，不会把工作藏在不可追踪的线程里。 |
| 专家型子任务需要返回一个边界清晰的结果 | delegate 或 sub-agent tool | 父 agent 收到普通 tool result，可自行决定继续、重试或总结。 |
| 另一个 agent 应接管同一段对话 | Agent handoff | 在安全 step 边界切换 active agent，同时保留 thread history 和 state 连续性。 |
| 多个独立 agent 之间需要通信 | `send_message` / mailbox-backed 通信 | 实时 child message 和持久跨 thread message 都有明确 receipt，不依赖临时共享内存。 |
| 子 agent 需要父侧 state | 类型化 `StateKey` seed/export 策略 | state 契约可见，持久 key 是显式选择，传递失败会暴露为错误。 |
| thread、run、config 或 profile 需要持久化 | File/Postgres/NATS store 与 commit coordinator | 存储边界在开发期接好，后续运营调优才有可靠的 config、mailbox 和历史数据。 |
| 插件需要向模型注入上下文 | `PhaseContext` + `StateCommand` + `AddContextMessage` | hook 只读 snapshot 并返回命令，runtime 统一节流、排序、注入和提交。 |

## 需要显式呈现的开发面

文档说明代码能力时，应尽量指向已经能编译或已被测试覆盖的示例：

| 能力 | 开发面 | 代码参考 |
|---|---|---|
| Runtime 组装 | `AgentRuntimeBuilder`、provider、model、tool、commit coordinator | `crates/awaken-doctest/examples/http_app_builder.rs`、`crates/awaken-runtime/src/builder.rs` |
| 自定义 provider | `LlmExecutor`、`ProviderExecutorFactory`、`ModelPoolSpec` | `crates/awaken/tests/readme_quickstart.rs`、`crates/awaken-server/tests/config_api.rs` |
| Plugin 注入上下文 | `PhaseHook`、`PhaseContext`、`StateCommand`、`AddContextMessage`、tool filter | `crates/awaken-doctest/examples/plugin_registrar.rs`、`crates/awaken-runtime/src/agent/state/loop_actions.rs` |
| 后台工作 | `BackgroundTaskManager`、`BackgroundTaskPlugin`、`SendMessageTool`、`CancelTaskTool` | `crates/awaken-runtime/tests/background_task_lifecycle.rs`、`crates/awaken-runtime/src/extensions/background/` |
| Sub-agent 作为工具 | `run_child_agent`、`ChildAgentParams`、`BackendRunResult.state` 导出 | `crates/awaken-runtime/tests/child_agent_seed.rs`、`crates/awaken-runtime/src/child_agent/mod.rs` |
| Store 边界 | `ThreadRunStore`、`ConfigStore`、`ProfileStore`、`MailboxStore`、`VersionedRegistryStore` | `crates/awaken-doctest/examples/thread_store_trait.rs`、`crates/awaken-stores/tests/` |
| MCP 集成 | `McpToolRegistryManager`、custom transport、sampling handler | `crates/awaken-ext-mcp/tests/mcp_tests.rs`、`crates/awaken-ext-mcp/src/transport.rs` |
| Observability / Eval | `MetricsSink`、`TraceStore`、`RuntimeReplayer`、`JudgeConfig` | `crates/awaken-ext-observability/tests/`、`crates/awaken-eval/tests/eval_integration.rs` |

## 推荐顺序

1. 先读 [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)，确定 runtime、model registry 和 agent spec 的骨架。
2. 再读 [添加 Tool](/awaken/zh-cn/how-to/add-a-tool/) 和 [添加 Plugin](/awaken/zh-cn/how-to/add-a-plugin/)，用安全的方式扩展行为。
3. 用 [状态与存储概览](/awaken/zh-cn/state-and-storage/)、[状态管理](/awaken/zh-cn/explanation/state-management/) 和 store guides 接好 runtime state、config、profile 和 mailbox 存储边界。
4. 阅读 [多 Agent 模式](/awaken/zh-cn/explanation/multi-agent-patterns/)，选择 delegation、后台 agent、handoff 或通信方式。
5. 需要 mailbox 路由、等待 run、HITL 或分布式调度时，阅读 [HITL 与 Mailbox](/awaken/zh-cn/explanation/hitl-and-mailbox/)。
6. 需要一个 Agent 接管当前 thread 时，阅读 [使用 Agent Handoff](/awaken/zh-cn/how-to/use-agent-handoff/)。
7. 自定义 tool 代码需要受控调用子 Agent 并显式传递 state 时，阅读 [在工具里调用 Sub-Agent](/awaken/zh-cn/how-to/invoke-sub-agent-from-tool/)。
8. 需要 Agent 在文本之外流式输出 UI 文档时，阅读 [使用 Generative UI](/awaken/zh-cn/how-to/use-generative-ui/)。
9. [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/) 用于理解代码能力和运营调优的边界。

## 建议搭配阅读

- [Tool Trait](/awaken/zh-cn/reference/tool-trait/) 用于核对精确契约。
- [Tool 与 Plugin 的边界](/awaken/zh-cn/explanation/tool-and-plugin-boundary/) 用于判断扩展应该放在哪一层。
- [架构](/awaken/zh-cn/explanation/architecture/) 用于理解完整运行时模型。
