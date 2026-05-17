---
title: "AG-UI 协议"
description: "AG-UI 适配层会把 Awaken 的 AgentEvent 流转换成 AG-UI / CopilotKit 事件格式，使 CopilotKit 前端可以直接驱动 Awaken agent。"
---

AG-UI 适配层会把 Awaken 的 `AgentEvent` 流转换成 [AG-UI / CopilotKit](https://docs.ag-ui.com) 事件格式，使 CopilotKit 前端可以直接驱动 Awaken agent。

## 入口

```text
POST /v1/ag-ui/run
```

### 请求体

```json
{
  "threadId": "optional-thread-id",
  "agentId": "optional-agent-id",
  "messages": [{ "role": "user", "content": "Hello" }],
  "context": {}
}
```

| 字段 | 类型 | 必填 | 说明 |
|-------|------|------|------|
| `messages` | `AgUiMessage[]` | 是 | 聊天消息，使用 `role` 和 `content` |
| `threadId` | `string` | 否 | 继续已有 thread |
| `agentId` | `string` | 否 | 指定目标 agent |
| `context` | `object` | 否 | 前端上下文透传 |

### 响应

SSE 流（`text/event-stream`），每个 frame 是一个 JSON 编码的 AG-UI `Event`。

### 辅助路由

| 路由 | 方法 | 说明 |
|-------|--------|------|
| `/v1/ag-ui/threads/:thread_id/runs` | `POST` | 在指定 thread 上启动 run |
| `/v1/ag-ui/agents/:agent_id/runs` | `POST` | 在指定 agent 上启动 run |
| `/v1/ag-ui/threads/:thread_id/interrupt` | `POST` | 中断指定 thread |
| `/v1/ag-ui/threads/:id/messages` | `GET` | 读取 thread 消息历史 |

## 事件映射

| AgentEvent | AG-UI Event |
|------------|-------------|
| `RunStart` | `RUN_STARTED` |
| `TextDelta` | `TEXT_MESSAGE_START` + `TEXT_MESSAGE_CONTENT` |
| `ReasoningDelta` | `REASONING_MESSAGE_START` + `REASONING_MESSAGE_CONTENT` |
| `ToolCallStart` | 关闭当前 text/reasoning，然后发 `STEP_STARTED`、`TOOL_CALL_START` |
| `ToolCallDelta` | `TOOL_CALL_ARGS` |
| `ToolCallDone` | `TOOL_CALL_END`、`STEP_FINISHED` |
| `StateSnapshot` | `STATE_SNAPSHOT` |
| `StateDelta` | `STATE_DELTA` |
| `RunFinish` | `RUN_FINISHED` 或 `RUN_ERROR` |

## AG-UI 事件类型

- `RUN_STARTED` / `RUN_FINISHED` / `RUN_ERROR`
- `TEXT_MESSAGE_START` / `TEXT_MESSAGE_CONTENT` / `TEXT_MESSAGE_END`
- `REASONING_MESSAGE_START` / `REASONING_MESSAGE_CONTENT` / `REASONING_MESSAGE_END`
- `STEP_STARTED` / `STEP_FINISHED`
- `TOOL_CALL_START` / `TOOL_CALL_ARGS` / `TOOL_CALL_END`
- `STATE_SNAPSHOT` / `STATE_DELTA`
- `MESSAGES_SNAPSHOT`

## 角色

AG-UI 消息角色使用小写字符串：`system`、`user`、`assistant`、`tool`。

## 文本消息生命周期

1. 第一个 `TextDelta` 会发 `TEXT_MESSAGE_START` 和 `TEXT_MESSAGE_CONTENT`。
2. 后续 delta 只追加 `TEXT_MESSAGE_CONTENT`。
3. 遇到 `ToolCallStart` 或 `RunFinish` 时，当前消息会以 `TEXT_MESSAGE_END` 关闭。

reasoning 消息采用同样模式。

## 相关

- [事件](/zh-cn/reference/events/)
- [集成 CopilotKit (AG-UI)](/zh-cn/how-to/integrate-copilotkit-ag-ui/)
