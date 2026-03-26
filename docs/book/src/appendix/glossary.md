# Glossary

| Term | 中文 | Description |
|------|------|-------------|
| `Thread` | 会话线程 | Persisted conversation + state history. |
| `Run` | 运行 | One execution attempt over a thread. |
| `Phase` | 阶段 | Named step in the execution loop (RunStart, StepStart, BeforeInference, AfterInference, BeforeToolExecute, AfterToolExecute, StepEnd, RunEnd). |
| `Snapshot` | 快照 | Immutable state snapshot (`Arc<StateMap>`) seen by phase hooks. |
| `StateKey` | 状态键 | Typed key with scope, merge strategy, and value type. |
| `MutationBatch` | 变更批次 | Collected state mutations applied atomically after phase convergence. |
| `AgentRuntime` | 智能体运行时 | Orchestration layer for agent resolution and run execution. |
| `AgentSpec` | 智能体规约 | Serializable agent definition with model, prompt, tools, plugins. |
| `AgentEvent` | 智能体事件 | Canonical runtime stream event. |
| `Plugin` | 插件 | System-level lifecycle hook registered via `PluginRegistrar`. |
| `PluginRegistrar` | 插件注册器 | Registration API for phase hooks, tools, state keys, handlers. |
| `PhaseHook` | 阶段钩子 | Async hook executed during a specific phase. |
| `PhaseContext` | 阶段上下文 | Immutable context passed to phase hooks. |
| `StateCommand` | 状态命令 | Result of a phase hook: mutations + scheduled actions + effects. |
| `Tool` | 工具 | User-facing capability with descriptor, validation, and execution. |
| `ToolDescriptor` | 工具描述符 | Tool metadata: id, name, description, parameters schema. |
| `ToolResult` | 工具结果 | Tool execution result: success, error, or suspended. |
| `ToolCallContext` | 工具调用上下文 | Execution context with state access, identity, and activity reporting. |
| `TerminationReason` | 终止原因 | Why a run ended (NaturalEnd, Stopped, Error, etc.). |
| `SuspendTicket` | 挂起票据 | Suspension payload with pending call and resume mode. |
| `MailboxJob` | 邮箱任务 | Durable job entry for async/HITL workflows. |
| `RunRequest` | 运行请求 | Input to start a run: messages, thread ID, agent ID. |
| `MergeStrategy` | 合并策略 | How parallel state mutations are reconciled: Exclusive (conflict = error) or Commutative (order-independent). |
| `KeyScope` | 键作用域 | Lifetime of a state key: Run (per-execution) or Thread (persisted across runs). |
| `StateMap` | 状态映射 | Type-safe heterogeneous map backing Snapshot. |
| `RunStatus` | 运行状态 | Coarse run status: Running, Waiting, Done. |
| `ToolCallStatus` | 工具调用状态 | Per-call status: New, Running, Suspended, Resuming, Succeeded, Failed, Cancelled. |
| `ResolvedAgent` | 已解析智能体 | Agent fully resolved from registries with config, tools, plugins, and executor. |
| `AgentResolver` | 智能体解析器 | Trait that resolves an agent spec ID into a ResolvedAgent. |
| `BuildError` | 构建错误 | Error from AgentRuntimeBuilder when validation or wiring fails. |
| `RuntimeError` | 运行时错误 | Error during agent loop execution. |
| `InferenceOverride` | 推理覆盖 | Per-request override for model, temperature, max_tokens, reasoning_effort. |
| `ContextWindowPolicy` | 上下文窗口策略 | Token budget and compaction rules for context management. |
| `StreamEvent` | 流事件 | Envelope wrapping AgentEvent with sequence number and timestamp. |
| `TokenUsage` | 令牌用量 | Token consumption report from LLM inference. |
| `ExecutionEnv` | 执行环境 | Assembled runtime environment holding hooks, handlers, tools, and plugins. |
| `CommitHook` | 提交钩子 | Hook invoked when state is committed to storage. |
