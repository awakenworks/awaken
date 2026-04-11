# 常见问题

## 支持哪些 LLM provider？

任何兼容 `genai` 的 provider 都可以，包括 OpenAI、Anthropic、DeepSeek、Google Gemini、Ollama 等。当前做法不是在 `AgentSpec` 里直接写 provider 名，而是把 `AgentSpec.model_id` 写成模型注册表里的 ID，再由运行时 `ModelBinding` 解析到 provider 和真实模型名。服务端 `/v1/config/models` 持久化的是可序列化的 `ModelBindingSpec`，发布 registry 时会转换成 `ModelBinding`。

## 如何添加新的存储后端？

实现 `awaken-contract` 里的 `ThreadRunStore` trait。它要求你支持 thread、message、run、checkpoint 的读写。可以参考 `awaken-stores` 里的 `InMemoryStore`、`FileStore`、`PostgresStore`。

## 不启用 server 能用 awaken 吗？

可以。`AgentRuntime` 本身就是独立运行时。你可以自己构造 `RunRequest`，再直接调用 `runtime.run(request, sink)`。`awaken-server` 只是附加的 HTTP / SSE / mailbox / protocol gateway。

## 如何运行多个 agent？

两种主要方式：

- 本地委托：在 `AgentSpec.delegates` 里声明可委托的 agent ID，由运行时在步骤边界完成委托切换。
- 远程 A2A：给 `AgentSpec.endpoint` 配置远端地址，或注册远程 agent，让它通过 A2A 以“工具调用”的形式执行。

## Run scope 和 Thread scope 的区别是什么？

- `Run`：只在一次 run 生命周期内有效。run 结束即清空。适合步骤计数、预算、临时上下文。
- `Thread`：在同一 thread 的多次 run 之间持续存在。适合用户偏好、会话记忆、长期状态。

## 如何处理工具错误？

可恢复错误返回 `ToolResult::error(...)`，这样错误会以 tool 响应消息的形式回写给 LLM，LLM 可以继续决定下一步。如果是参数校验失败或真正要中止工具执行的错误，则返回 `ToolError`。

## 工具可以并行执行吗？

可以。通过 `ToolExecutionMode` 配置。并行模式下，运行时会并发执行同一步里的多个 tool call，并在下一轮推理前收集结果、做冲突检查和状态合并。

## run 卡住时怎么排查？

先看 run 的状态：

- `Waiting`：通常是 HITL 决策未完成，检查 `SuspendTicket`、mailbox 和待处理 decision。
- `Running`：检查 `max_rounds`、timeout、工具是否阻塞，以及是否有流式调用未结束。

如果需要细节，启用 observability 插件查看 phase、tool、inference 级别的 tracing。

## 不连真实 LLM 怎么测试？

实现一个返回固定响应的 `LlmExecutor` 即可。详细模式见[测试策略](../how-to/testing-strategy.md)。

## 并行工具同时写同一个状态键会怎样？

如果该键是 `MergeStrategy::Exclusive`，并行合并会失败，相关执行会回退到串行或报冲突。若该键天然支持交换律，应使用 `MergeStrategy::Commutative`。

## request transform 是怎么工作的？

插件可以注册 `InferenceRequestTransform`。它会在请求真正发给 LLM 前修改 system prompt、工具列表、推理参数等。只有当前 agent 激活的插件 transform 才会生效。

## 可以自定义存储后端吗？

可以。状态与消息持久化实现 `ThreadRunStore`；如果还要支持 HITL / 后台队列，再实现 `MailboxStore`。

## context compaction 是怎么做的？

当 `ContextWindowPolicy` 启用自动压缩时，运行时会在超过预算后寻找安全边界，把较早消息总结成 `<conversation-summary>`，再保留最近一段原始上下文。

## AI SDK v6、AG-UI、A2A 该怎么选？

- AI SDK v6：适合 Vercel AI SDK / `useChat` 前端。
- AG-UI：适合 CopilotKit 和带生成式 UI 的前端。
- A2A：适合 agent 到 agent 的服务间编排和远程委托。

它们共享同一个 `AgentRuntime`，差别主要在协议编码层和前端生态。
