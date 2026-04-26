# HTTP API

启用 `server` feature 后，`awaken-server` 会通过 Axum 暴露 HTTP API。大多数接口返回 JSON，流式接口返回 SSE。

本页对应当前代码里的路由树：`crates/awaken-server/src/routes.rs` 与 `crates/awaken-server/src/config_routes.rs`。

## 健康检查与指标

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/health` | 就绪探针；检查 store 连通性，返回 `200` 或 `503` |
| `GET` | `/health/live` | 存活探针；始终返回 `200` |
| `GET` | `/metrics` | Prometheus 指标抓取口 |

## Threads

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/threads` | 列出 thread ID，支持分页与父子过滤；返回 `{ items, offset, limit, total, has_more, next_cursor }` |
| `POST` | `/v1/threads` | 创建 thread；body：`{ "title"?: string, "resource_id"?: string, "parent_thread_id"?: string }` |
| `GET` | `/v1/threads/summaries` | 列出 thread 摘要（id、`resource_id`、`parent_thread_id`、title、`updated_at`、`agent_id`），分页与父子过滤参数与 `/v1/threads` 相同 |
| `GET` | `/v1/threads/:id` | 获取 thread |
| `PATCH` | `/v1/threads/:id` | 更新 thread 元信息 |
| `DELETE` | `/v1/threads/:id` | 删除 thread；可通过 `?child_strategy=detach\|reject\|cascade`（默认 `detach`）控制子 thread 的处理方式 |
| `POST` | `/v1/threads/:id/cancel` | 取消该 thread 上排队或运行中的某个 dispatch；返回 `cancel_requested` |
| `POST` | `/v1/threads/:id/decision` | 向该 thread 上等待中的 run 提交 HITL decision |
| `POST` | `/v1/threads/:id/interrupt` | 中断该 thread：递增 thread dispatch epoch、取消所有待执行 dispatch、中止活动 run；返回 `interrupt_requested` 及 `superseded_dispatches` 计数。与 `/cancel` 不同，此接口通过 `mailbox.interrupt()` 执行完整的"清空并中断"操作 |
| `PATCH` | `/v1/threads/:id/metadata` | 更新 metadata 的别名接口 |
| `GET` | `/v1/threads/:id/messages` | 列出消息，支持游标分页、序号窗口、排序、可见性与产生 run 过滤 |
| `POST` | `/v1/threads/:id/messages` | 作为后台 run 提交消息 |
| `POST` | `/v1/threads/:id/mailbox` | 向 mailbox 推送消息载荷 |
| `GET` | `/v1/threads/:id/mailbox` | 查看该 thread 的 mailbox dispatch |
| `GET` | `/v1/threads/:id/runs` | 列出该 thread 的 runs |
| `GET` | `/v1/threads/:id/runs/active` | 获取该 thread 当前活动 run（如有） |
| `GET` | `/v1/threads/:id/runs/latest` | 获取最新 run |

`POST /v1/threads/:id/messages` 与 `POST /v1/runs/:id/inputs` 支持可选的
`mode` 字段。`queue` 会追加持久化 mailbox dispatch；`live_then_queue` 会先
尝试把消息投递给活动 run，live 投递不可用时再排队；`steer` 是
`live_then_queue` 的别名；`interrupt_then_queue` 会先取消活动 run 再排队；
`resume_open_run` 会继续可恢复的等待中 run。

## Runs

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/runs` | 列出 runs |
| `POST` | `/v1/runs` | 启动 run，并通过 SSE 返回事件 |
| `GET` | `/v1/runs/:id` | 获取 run 记录 |
| `POST` | `/v1/runs/:id/inputs` | 向同一 thread 追加后续输入 |
| `POST` | `/v1/runs/:id/cancel` | 按 run ID 取消 |
| `POST` | `/v1/runs/:id/decision` | 按 run ID 提交 HITL decision |

## Config 与 Capabilities

这些接口由 `config_routes()` 提供。读取与 schema 接口要求 `AppState`
挂接 config store；写接口还要求挂接 config runtime manager，才能在写入后
校验并发布新的 registry snapshot。缺少这些配置时会返回 `400`，错误为
`config management API not enabled`。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/capabilities` | 列出 agents、tools、plugins、models、providers 和 config namespaces |
| `GET` | `/v1/config/:namespace` | 列出某个 namespace 下的配置项 |
| `POST` | `/v1/config/:namespace` | 创建配置项，body 必须含 `"id"` |
| `GET` | `/v1/config/:namespace/:id` | 获取单个配置项 |
| `PUT` | `/v1/config/:namespace/:id` | 整体替换配置项 |
| `DELETE` | `/v1/config/:namespace/:id` | 删除配置项 |
| `GET` | `/v1/config/:namespace/$schema` | 获取该 namespace 的 JSON Schema |
| `GET` | `/v1/agents` | `/v1/config/agents` 的便捷别名 |
| `GET` | `/v1/agents/:id` | `/v1/config/agents/:id` 的便捷别名 |

`GET /v1/capabilities` 会包含每个已注册插件的 `config_schemas`。admin console
使用该字段渲染 agent 级插件配置表单，并把值保存到 `AgentSpec.sections`。
配置写入成功后，runtime manager 会发布新的 registry snapshot，因此后续
`/v1/runs` 会使用更新后的 agents、models、providers、MCP servers 和插件
section。

当前内置 namespace：

- `agents`
- `models`
- `providers`
- `mcp-servers`

## AI SDK v6 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/ai-sdk/chat` | 启动 chat run，并流式返回 AI SDK 编码事件 |
| `POST` | `/v1/ai-sdk/agent-previews/runs` | 使用未保存的草稿 `AgentSpec` 运行；admin console 预览功能使用 |
| `POST` | `/v1/ai-sdk/threads/:thread_id/runs` | 在指定 thread 上启动 run |
| `POST` | `/v1/ai-sdk/agents/:agent_id/runs` | 在指定 agent 上启动 run |
| `POST` | `/v1/ai-sdk/agent-previews/runs` | 使用当前 registries 运行草稿 `AgentSpec`，不会持久化该 agent |
| `GET` | `/v1/ai-sdk/chat/:thread_id/stream` | 按 thread ID 续接 SSE |
| `GET` | `/v1/ai-sdk/threads/:thread_id/stream` | 同上别名 |
| `GET` | `/v1/ai-sdk/threads/:thread_id/messages` | 列出 thread 消息 |
| `POST` | `/v1/ai-sdk/threads/:thread_id/cancel` | 取消该 thread 上活动或排队中的 run |
| `POST` | `/v1/ai-sdk/threads/:thread_id/interrupt` | 中断 thread（递增 dispatch epoch、取消待执行 dispatch、中止活动 run）|

## AG-UI 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/ag-ui/run` | 启动 AG-UI run，并流式返回 AG-UI 事件 |
| `POST` | `/v1/ag-ui/threads/:thread_id/runs` | 在指定 thread 上启动 run |
| `POST` | `/v1/ag-ui/agents/:agent_id/runs` | 在指定 agent 上启动 run |
| `POST` | `/v1/ag-ui/threads/:thread_id/interrupt` | 中断 thread |
| `GET` | `/v1/ag-ui/threads/:id/messages` | 列出 thread 消息 |

## A2A 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/.well-known/agent-card.json` | 获取公共/默认 agent card |
| `POST` | `/v1/a2a/message:send` | 向公共/默认 A2A agent 发送消息 |
| `POST` | `/v1/a2a/message:stream` | 通过 SSE 进行流式发送 |
| `GET` | `/v1/a2a/tasks` | 列出 A2A 任务 |
| `GET` | `/v1/a2a/tasks/:task_id` | 查询任务状态 |
| `POST` | `/v1/a2a/tasks/:task_id:cancel` | 取消任务 |
| `POST` | `/v1/a2a/tasks/:task_id:subscribe` | 通过 SSE 订阅任务更新 |
| `POST` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | 创建推送通知配置 |
| `GET` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | 列出推送通知配置 |
| `GET` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | 获取推送通知配置 |
| `DELETE` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | 删除推送通知配置 |
| `GET` | `/v1/a2a/extendedAgentCard` | 获取扩展 agent card；未启用时返回 `501` |
| `POST` | `/v1/a2a/:tenant/message:send` | 向 tenant 作用域 agent 发送消息 |
| `POST` | `/v1/a2a/:tenant/message:stream` | tenant 作用域流式发送 |
| `GET` | `/v1/a2a/:tenant/tasks` | 列出 tenant 作用域任务 |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id` | 查询 tenant 作用域任务状态 |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id:cancel` | 取消 tenant 作用域任务 |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id:subscribe` | 订阅 tenant 作用域任务更新 |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs` | 创建 tenant 作用域推送通知配置 |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs` | 列出 tenant 作用域推送通知配置 |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs/:config_id` | 获取 tenant 作用域推送通知配置 |
| `DELETE` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs/:config_id` | 删除 tenant 作用域推送通知配置 |
| `GET` | `/v1/a2a/:tenant/extendedAgentCard` | 获取 tenant 作用域扩展 agent card |

## MCP HTTP 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/mcp` | MCP JSON-RPC 请求/响应入口。`initialize` 会创建 session 并返回 `MCP-Session-Id`；后续 request、notification 和 response 都必须带该 header |
| `GET` | `/v1/mcp` | 为 MCP 服务端主动 SSE 预留；当前返回 `405` |
| `DELETE` | `/v1/mcp` | 根据 `MCP-Session-Id` 终止已知 MCP HTTP session；返回 `204` 或 `404` |

`initialize` 请求不能携带 `MCP-Session-Id`。`tools/call` 可能返回流式响应。所有 MCP HTTP 路由都会在存在 `Origin` header 时进行校验。

## 常见查询参数

分页：

- `offset`：跳过的条数
- `limit`：返回上限，范围限制在 `1..=200`（默认 `50`）
- `cursor`：不透明分页游标，提供后会优先于 `offset`。游标绑定到原始 query
  形状，filter 一旦改变就会被拒绝
- 响应中的 `next_cursor` / `prev_cursor` 在仍有更多页时返回

Thread 列表过滤（`/v1/threads`、`/v1/threads/summaries`）：

- `resource_id`（别名 `resourceId`）：按外部资源分组过滤
- `parent_thread_id`（别名 `parentThreadId`）：仅返回该父 thread 的直接子线程
- `root`：为 `true` 时仅返回没有父线程的根 thread；不能与 `parent_thread_id`
  同时使用

消息列表过滤（`/v1/threads/:id/messages` 及各协议别名）：

- `after`、`before`：序号窗口
- `order`：`asc`（默认）或 `desc`
- `visibility`：`external`（默认）、`internal` 或 `all`
- `run_id`（别名 `runId`）：仅保留由该 run 产生的消息

Run 列表过滤：

- `status`：`running`、`waiting` 或 `done`

## 错误格式

大多数接口返回：

```json
{ "error": "human-readable message" }
```

MCP 接口返回 JSON-RPC 错误对象，而不是上面的通用形状。

## 相关

- [通过 SSE 暴露 HTTP](../how-to/expose-http-sse.md)
- [配置](./config.md)
