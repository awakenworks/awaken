---
title: "AI SDK v6 协议"
description: "AI SDK v6 适配层会把 Awaken 的 AgentEvent 流转换成 Vercel AI SDK v6 UI Message Stream 格式。这样 useChat、useAssistant 等前端无需自定义解析器就能直接消费 Awaken 的输出。"
---

AI SDK v6 适配层会把 Awaken 的 `AgentEvent` 流转换成 [Vercel AI SDK v6 UI Message Stream](https://sdk.vercel.ai/docs/ai-sdk-ui/stream-protocol) 格式。这样 `useChat`、`useAssistant` 等前端无需自定义解析器就能直接消费 Awaken 的输出。

## 入口

```text
POST /v1/ai-sdk/chat
```

### 请求体

```json
{
  "messages": [{ "role": "user", "content": "Hello" }],
  "threadId": "optional-thread-id",
  "agentId": "optional-agent-id"
}
```

| 字段 | 类型 | 必填 | 说明 |
|-------|------|------|------|
| `messages` | `AiSdkMessage[]` | 是 | 输入消息。内容可以是字符串或 content parts 数组。 |
| `threadId` | `string` | 否 | 继续已有 thread。省略时会创建新 thread。 |
| `agentId` | `string` | 否 | 指定 agent。省略时使用默认 agent。 |

### 响应

SSE 流（`text/event-stream`），每一行是一个 JSON 编码的 `UIStreamEvent`。

### 辅助路由

| 路由 | 方法 | 说明 |
|-------|--------|------|
| `/v1/ai-sdk/threads/:thread_id/runs` | `POST` | 在指定 thread 上启动 run |
| `/v1/ai-sdk/agents/:agent_id/runs` | `POST` | 在指定 agent 上启动 run |
| `/v1/ai-sdk/agent-previews/runs` | `POST` | 使用当前 registries 运行草稿 `AgentSpec`，不会持久化该 agent |
| `/v1/ai-sdk/chat/:thread_id/stream` | `GET` | 按 thread ID 续接 SSE |
| `/v1/ai-sdk/threads/:thread_id/stream` | `GET` | 同上，thread 路由别名 |
| `/v1/ai-sdk/threads/:thread_id/messages` | `GET` | 读取 thread 消息历史 |
| `/v1/ai-sdk/threads/:thread_id/cancel` | `POST` | 取消 thread |
| `/v1/ai-sdk/threads/:thread_id/interrupt` | `POST` | 中断 thread |

## 事件映射

`AiSdkEncoder` 会把 `AgentEvent` 映射到 `UIStreamEvent`：

| AgentEvent | UIStreamEvent |
|------------|---------------|
| `RunStart` | `MessageStart` + `Data("run-info", ...)` |
| `TextDelta` | `TextStart`（如果 block 未打开）+ `TextDelta` |
| `ReasoningDelta` | `ReasoningStart`（如果 block 未打开）+ `ReasoningDelta` |
| `ReasoningEncryptedValue` | `ReasoningStart`（如果未打开）+ `ReasoningDelta` |
| `ToolCallStart` | 关闭当前 text/reasoning block，然后发 `ToolCallStart` |
| `ToolCallDelta` | `ToolCallDelta` |
| `ToolCallDone` | `ToolCallEnd` |
| `StepStart` | （无直接映射） |
| `StepEnd` | （无直接映射） |
| `InferenceComplete` | `Data("inference-complete", ...)` |
| `MessagesSnapshot` | `Data("messages-snapshot", ...)` |
| `StateSnapshot` | `Data("state-snapshot", ...)` |
| `StateDelta` | `Data("state-delta", ...)` |
| `ActivitySnapshot` | `Data("activity-snapshot", ...)` |
| `ActivityDelta` | `Data("activity-delta", ...)` |
| `RunFinish` | 关闭当前 block，然后发 `Data("finish", ...)` 和 `Finish` |

## UIStreamEvent 类型

- `start`
- `text-start` / `text-delta` / `text-end`
- `reasoning-start` / `reasoning-delta` / `reasoning-end`
- `tool-call-start` / `tool-call-delta` / `tool-call-end`
- `data`
- `finish`

## 文本块生命周期

编码器会自动管理文本块边界：

1. 第一个 `TextDelta` 会打开一个 text block。
2. 后续 delta 追加到同一个 block。
3. 遇到 `ToolCallStart` 会先关闭当前 block。
4. 工具结束后的新文本会重新开一个新 block。

## 相关

- [事件](/events/)
- [HTTP API](/http-api/)
- [集成 AI SDK 前端](/../how-to/integrate-ai-sdk-frontend/)
