---
title: "常见问题"
description: "关于 Awaken 的常见问题:何时用 runtime 还是 server、如何选协议、provider 与 model、state,以及运维。"
---

## 支持哪些 LLM provider？

任何兼容 `genai` 的 provider 都可以，包括 OpenAI、Anthropic、DeepSeek、Google Gemini、Ollama 等。当前做法不是在 `AgentSpec` 里直接写 provider 名，而是把 `AgentSpec.model_id` 写成模型注册表里的 ID，再由 `ModelRegistry` 解析到对应的 `ModelSpec`（含 provider、上游模型名，以及可选的 capabilities、定价）。服务端 `/v1/config/models` 持久化的就是这份 `ModelSpec`，发布 registry 时直接使用、无需二次转换。

## 如何添加新的存储后端？

按需要实现对应存储 trait：thread/run 持久化实现 `ThreadRunStore`，运行时配置实现 `ConfigStore`，profile/shared state 实现 `ProfileStore`，HITL / 后台队列实现 `MailboxStore`。可以参考 `awaken-stores` 里的 `InMemoryStore`、`FileStore`、`PostgresStore`、`InMemoryMailboxStore` 和 `SqliteMailboxStore`。

## 不启用 server 能用 awaken 吗？

可以。`AgentRuntime` 本身就是独立运行时。你可以自己构造 `RunActivation`；只需要最终结果时调用 `runtime.run_to_completion(request)`，调用方需要流式事件时调用 `runtime.run(request, sink)`。`awaken-server` 只是附加的 HTTP / SSE / mailbox / protocol gateway。

## 如何运行多个 agent？

两种主要方式：

- 委托：在 `AgentSpec.delegates` 里声明可委托的 agent ID，运行时会把每个 delegate 暴露为 `AgentTool`；本地 delegate 在进程内执行，带 `endpoint` 的 delegate 通过 `ExecutionBackend` 执行。
- Handoff：需要让另一个 agent 接管当前 thread，而不是把结果返回父 agent 时，使用 handoff 扩展。
- 远程 A2A：给 `AgentSpec.endpoint` 配置远端地址，或通过 `AgentRuntimeBuilder::with_remote_agents()` 注册远程 agent，让它通过 A2A 执行。

## Run scope 和 Thread scope 的区别是什么？

- `Run`：只在一次 run 生命周期内有效。run 结束即清空。适合步骤计数、预算、临时上下文。
- `Thread`：在同一 thread 的多次 run 之间持续存在。适合用户偏好、会话记忆、长期状态。

## 如何处理工具错误？

可恢复错误返回 `ToolResult::error(...)`，这样错误会以 tool 响应消息的形式回写给 LLM，LLM 可以继续决定下一步。如果是参数校验失败或真正要中止工具执行的错误，则返回 `ToolError`。

## 工具可以并行执行吗？

可以，但不是通过 `AgentSpec` 配置。内置 resolver 默认使用 `SequentialToolExecutor`。如果工具彼此独立，并且可以安全共享同一份冻结状态快照，可以通过自定义 resolver 或 `ResolvedAgent::with_tool_executor(...)` 安装 `ParallelToolExecutor`。

## run 卡住时怎么排查？

先看 run 的状态：

- `Waiting`：通常是 HITL 决策未完成，检查 `SuspendTicket`、mailbox 和待处理 decision。
- `Running`：检查 `max_rounds`、timeout、工具是否阻塞，以及是否有流式调用未结束。

如果需要细节，启用 observability 插件查看 phase、tool、inference 级别的 tracing。

## 不连真实 LLM 怎么测试？

实现一个返回固定响应的 `LlmExecutor` 即可。详细模式见[测试策略](/awaken/zh-cn/how-to/testing-strategy/)。

## 并行工具同时写同一个状态键会怎样？

如果你自己合并并行 state batch，`MergeStrategy::Exclusive` 会在两个 batch 写同一键时冲突；天然支持交换律的键应使用 `MergeStrategy::Commutative`。默认 loop 会按结果顺序提交 tool 结果；自定义并行集成应使用 parallel merge helpers。

## request transform 是怎么工作的？

插件可以注册 `InferenceRequestTransform`。它会在请求真正发给 LLM 前修改 system prompt、工具列表、推理参数等。只有当前 agent 激活的插件 transform 才会生效。

## 可以自定义存储后端吗？

可以。状态与消息持久化实现 `ThreadRunStore`；运行时配置实现 `ConfigStore`；profile/shared state 实现 `ProfileStore`；如果还要支持 HITL / 后台队列，再实现 `MailboxStore`。

## context compaction 是怎么做的？

当 `ContextWindowPolicy` 启用自动压缩时，运行时会在超过预算后寻找安全边界，把较早消息总结成 `<conversation-summary>`，再保留最近一段原始上下文。

## AI SDK v6、AG-UI、A2A、MCP、ACP 该怎么选？

- AI SDK v6：适合 Vercel AI SDK / `useChat` 前端。
- AG-UI：适合 CopilotKit 和带生成式 UI 的前端。
- A2A：适合 agent 到 agent 的服务间编排和远程委托。
- MCP HTTP：适合外部 MCP client 通过 JSON-RPC 调用 Awaken 工具，并使用 `MCP-Session-Id` 管理 session 生命周期。
- ACP stdio：适合 Agent Client Protocol host 把 Awaken 作为本地进程启动，并通过 stdin/stdout 交换消息。

它们共享同一个 `AgentRuntime`，差别主要在协议编码层、transport 和客户端生态。
