# A2A 协议

A2A 适配器实现了官方 [A2A 协议](https://a2a-protocol.org/latest/specification/)，用于远程 agent 发现、任务委托与 agent 间通信。

**Feature gate**：`server`

## 端点

| 路径 | 方法 | 说明 |
|-------|--------|-------------|
| `/.well-known/agent-card.json` | `GET` | 公共/默认 agent card 发现端点。 |
| `/v1/a2a/message:send` | `POST` | 向公共/默认 A2A agent 发送消息，返回 task 包装结果。 |
| `/v1/a2a/message:stream` | `POST` | 通过 SSE 进行流式发送。 |
| `/v1/a2a/tasks` | `GET` | 列出 A2A 任务。 |
| `/v1/a2a/tasks/:task_id` | `GET` | 按 task ID 查询状态。 |
| `/v1/a2a/tasks/:task_id:cancel` | `POST` | 取消运行中的任务。 |
| `/v1/a2a/tasks/:task_id:subscribe` | `POST` | 通过 SSE 订阅任务更新。 |
| `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | `POST` | 创建推送通知配置。 |
| `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | `GET` | 列出推送通知配置。 |
| `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | `GET` / `DELETE` | 读取或删除推送通知配置。 |
| `/v1/a2a/extendedAgentCard` | `GET` | 扩展 agent card；只有 `capabilities.extendedAgentCard=true` 时才受支持。 |

租户/agent 作用域的等价路由位于 `/v1/a2a/:tenant/...`，例如 `/v1/a2a/research/message:send` 和 `/v1/a2a/research/tasks/:task_id`。

## Agent Card

发现端点返回描述接口与能力的 `AgentCard`：

```json
{
  "name": "My Agent",
  "description": "A helpful assistant",
  "supportedInterfaces": [
    {
      "url": "https://example.com/v1/a2a",
      "protocolBinding": "HTTP+JSON",
      "protocolVersion": "1.0"
    }
  ],
  "version": "1.0.0",
  "capabilities": {
    "streaming": true,
    "pushNotifications": true,
    "stateTransitionHistory": false,
    "extendedAgentCard": false
  },
  "defaultInputModes": ["text/plain"],
  "defaultOutputModes": ["text/plain"],
  "skills": [
    {
      "id": "general",
      "name": "General Q&A",
      "description": "Answer general questions",
      "tags": ["qa"],
      "inputModes": ["text/plain"],
      "outputModes": ["text/plain"]
    }
  ]
}
```

Agent card 由已注册的 `AgentSpec` 生成。旧版顶层 `url` / `id` 字段不会再输出。

## Message Send

```json
{
  "message": {
    "taskId": "optional-client-provided-id",
    "contextId": "optional-client-provided-id",
    "messageId": "msg-123",
    "role": "ROLE_USER",
    "parts": [{ "text": "Summarize this document" }]
  },
  "configuration": {
    "returnImmediately": true
  }
}
```

服务端会把 A2A task 映射到 Awaken 的 thread / mailbox 执行链路。响应使用 v1 的 task 包装结构：

```json
{
  "task": {
    "id": "optional-client-provided-id",
    "contextId": "optional-client-provided-id",
    "status": {
      "state": "TASK_STATE_SUBMITTED"
    }
  }
}
```

如果未设置 `returnImmediately` 或传入 `false`，适配器会等待任务进入终态或中断态后再返回。

## Task 状态

`GET /v1/a2a/tasks/:task_id` 返回 `Task` 资源：

```json
{
  "id": "abc-123",
  "contextId": "abc-123",
  "status": {
    "state": "TASK_STATE_COMPLETED",
    "message": {
      "messageId": "msg-response",
      "role": "ROLE_AGENT",
      "parts": [{ "text": "..." }]
    }
  },
  "history": []
}
```

任务状态使用 v1 枚举名，例如 `TASK_STATE_SUBMITTED`、`TASK_STATE_WORKING`、`TASK_STATE_COMPLETED`、`TASK_STATE_FAILED`、`TASK_STATE_CANCELED`。

## 可选能力默认值

Awaken 当前默认启用以下 A2A 能力：

- `streaming = true`
- `pushNotifications = true`

`extendedAgentCard` 仍然是可选能力，只有在配置 `ServerConfig.a2a_extended_card_bearer_token` 后才会启用。未启用时，对应端点会返回符合规范的 unsupported 错误。

## 远程 Agent 委托

Awaken agent 可以通过 `AgentTool::remote()` 委托到远程 A2A agent。`A2aBackend` 会向远端发送 `message:send` 请求，读取返回的 `task.id`，再轮询 `/tasks/:task_id` 直到完成。从 LLM 视角看，这仍然只是一次普通工具调用。

远程 agent 配置写在 `AgentSpec` 中：

```json
{
  "id": "remote-researcher",
  "endpoint": {
    "base_url": "https://remote-agent.example.com",
    "bearer_token": "...",
    "poll_interval_ms": 1000,
    "timeout_ms": 300000
  }
}
```

带 `endpoint` 的 agent 会按远程 A2A agent 解析；没有 `endpoint` 的 agent 仍在本地运行。

## 另见

- [多智能体模式](../../explanation/multi-agent-patterns.md) —— 委托与 handoff 设计
- [A2A 规范](https://a2a-protocol.org/latest/specification/) —— 官方协议参考
