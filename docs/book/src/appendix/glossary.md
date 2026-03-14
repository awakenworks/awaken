# Glossary / 术语表

| Term | 中文 | Description |
|------|------|-------------|
| `Thread` | 会话线程 | Persisted conversation + state history. |
| `Run` | 运行 | One execution attempt over a thread. |
| `RunContext` | 运行上下文 | Loop-internal workspace that owns the live `DocCell`, message log, and patch accumulator. Plugins receive `ReadOnlyContext` instead. |
| `Patch` | 补丁 | Ordered list of state `Op` operations. |
| `TrackedPatch` | 追踪补丁 | Patch plus metadata for traceability. |
| `ThreadChangeSet` | 线程变更集 | Append payload persisted to storage. |
| `AgentOs` | 智能体操作系统 | Orchestration layer for registries and run prep. |
| `AgentEvent` | 智能体事件 | Canonical runtime stream event. |
| `RunPolicy` | 运行策略 | Strongly-typed per-run scope and execution policy carrying allow/exclude lists for tools, skills, and agents. |
| `AgentBehavior` | 智能体行为 | The plugin trait (formerly `AgentPlugin`); implementations register CRDT paths/state scopes and return typed `ActionSet`s from phase hooks. |
| `ActionSet` | 动作集 | Typed collection of phase actions returned by `AgentBehavior` hooks; composed with `ActionSet::and`. |
| `ReadOnlyContext` | 只读上下文 | Immutable snapshot of step context passed to `AgentBehavior` phase hooks; the plugin-facing API surface. |
| `RunDelta` | 运行增量 | Incremental output from a run step — new messages, `TrackedPatch`es, and serialized state actions since last `take_delta()`. |
| `RunStream` | 运行流 | Result of `AgentOs::run_stream`; carries resolved thread/run IDs, a decision sender for mid-run HITL, and the `AgentEvent` stream. |
| `StateSpec` | 状态规约 | Extension of `State` that adds a typed `Action` associated type, a `SCOPE` constant (Thread/Run/ToolCall), and a pure `reduce` method. |
| `ToolCallContext` | 工具调用上下文 | Execution context passed to tool invocations; provides typed state read/write, run policy, identity, and message queuing. |
| `AgentDefinition` | 智能体定义 | Orchestration-facing agent composition definition; holds model, system prompt, behavior/stop-condition IDs, and declarative specs. |
| `RunRequest` | 运行请求 | Unified runtime input for all external protocols; carries agent ID, thread/run IDs, messages, and initial decisions. |
| `ToolExecutionEffect` | 工具执行效果 | Rich tool return type wrapping a `ToolResult` plus a list of typed `Action`s applied during `AfterToolExecute`. |
| `SuspendTicket` | 挂起票据 | Suspension payload carrying external `suspension` data, a `pending` projection emitted to the event stream, and a `resume_mode` strategy. |
