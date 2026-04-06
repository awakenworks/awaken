# 术语表

| 术语 | 中文 | 说明 |
|------|------|------|
| `Thread` | 会话线程 | 持久化的对话与状态历史。 |
| `Run` | 运行 | 针对某个 thread 的一次执行尝试。 |
| `Phase` | 阶段 | 执行循环中的命名阶段。 |
| `Snapshot` | 快照 | 传给 hook / tool 的不可变状态视图（`struct Snapshot { revision: u64, ext: Arc<StateMap> }`）。 |
| `StateKey` | 状态键 | 带作用域、合并策略和值类型的类型化键。 |
| `MutationBatch` | 变更批次 | 在提交前收集的一组状态变更。 |
| `AgentRuntime` | 智能体运行时 | 负责解析 agent 和执行 run 的核心运行时。 |
| `AgentSpec` | 智能体规约 | 可序列化的 agent 定义。 |
| `AgentEvent` | 智能体事件 | 运行时统一事件流。 |
| `Plugin` | 插件 | 通过 `PluginRegistrar` 注册的系统级扩展。 |
| `PluginRegistrar` | 插件注册器 | 注册 phase hook、tool、state key、handler 的入口。 |
| `PhaseHook` | 阶段钩子 | 绑定到某个 phase 的异步 hook。 |
| `PhaseContext` | 阶段上下文 | hook 执行时拿到的只读上下文。 |
| `StateCommand` | 状态命令 | hook 返回值，包含变更、action、effect。 |
| `Tool` | 工具 | 暴露给 agent 的能力接口。 |
| `ToolDescriptor` | 工具描述符 | tool 的 ID、名称、描述、参数 schema。 |
| `ToolResult` | 工具结果 | tool 执行成功、失败或挂起后的结构化结果。 |
| `ToolCallContext` | 工具调用上下文 | tool 执行期可读取状态、上报活动，并携带恢复上下文（`resume_input`、`suspension_id`、`suspension_reason`）的上下文。 |
| `TerminationReason` | 终止原因 | run 结束的原因。 |
| `SuspendTicket` | 挂起票据 | 描述外部挂起标识、恢复模式和待决 tool call 投影的载荷。 |
| `call_id` | 工具调用 ID | 一个 tool call 在完整生命周期内保持稳定的内部标识。 |
| `suspension_id` | 挂起 ID | 某个 tool call 当前活跃挂起对应的外部标识。 |
| `MailboxJob` | 邮箱任务 | 后台执行与 HITL 的持久化作业项。 |
| `RunRequest` | 运行请求 | 启动 run 的输入。 |
| `MergeStrategy` | 合并策略 | 并行状态写入如何合并。 |
| `KeyScope` | 键作用域 | 状态键生命周期：`Run` 或 `Thread`。 |
| `StateMap` | 状态映射 | `Snapshot` 背后的类型安全异构 map。 |
| `RunStatus` | 运行状态 | 粗粒度 run 状态：`Running`、`Waiting`、`Done`；当前没有单独的 `Running+Waiting`。 |
| `ToolCallStatus` | 工具调用状态 | 单个 tool call 的状态。 |
| `ResolvedAgent` | 已解析智能体 | 从 registries 解析完成、可直接运行的 agent。 |
| `AgentResolver` | 智能体解析器 | 把 agent ID 解析成 `ResolvedAgent` 的组件。 |
| `BuildError` | 构建错误 | `AgentRuntimeBuilder::build()` 阶段的错误。 |
| `RuntimeError` | 运行时错误 | agent loop 执行中的错误。 |
| `InferenceOverride` | 推理覆盖 | 针对单次推理的 model / temperature 等覆盖。 |
| `ContextWindowPolicy` | 上下文窗口策略 | token 预算和压缩策略。 |
| `StreamEvent` | 流事件 | 对 `AgentEvent` 的带序号封装。 |
| `TokenUsage` | 令牌用量 | LLM 推理返回的 token 统计。 |
| `ExecutionEnv` | 执行环境 | 解析后组装出来的 hook、tool、handler 集合。 |
| `CommitHook` | 提交钩子 | 状态提交到存储时触发的 hook。 |
