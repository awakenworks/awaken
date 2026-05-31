---
title: "HTTP API"
description: "启用 server feature 后，awaken-server 会通过 Axum 暴露 HTTP API。大多数接口返回 JSON，流式接口返回 SSE。"
---

启用 `server` feature 后，`awaken-server` 会通过 Axum 暴露 HTTP API。大多数接口返回 JSON，流式接口返回 Server-Sent Events（SSE）。

本页对应当前代码里的路由树：`crates/awaken-server/src/routes.rs`、
`config_routes.rs`、`event_routes.rs`、`eval_router.rs`、`system_routes.rs`
以及协议模块。

## Admin 认证

Admin、config、eval、trace 和 system 路由要求
`Authorization: Bearer <token>`。该 token 来自
`AdminApiConfig.bearer_token` 或 `AWAKEN_ADMIN_API_BEARER_TOKEN`。如果路由已挂载
但没有配置 admin token，会返回 `401`，不会退回到匿名访问。

Server 启动时也会校验暴露的 admin surface。只要 config、eval 或 trace 路由被暴露，
`build_service_router` 就会在没有 admin token 时拒绝启动。协议 run 路由与 admin
控制面是两条边界，应由嵌入方在部署层按自己的入口策略保护。

## 路由暴露开关

| Surface | 何时挂载 | Auth/scope |
|---|---|---|
| Health、threads、runs | `ServerState` 始终提供 | health 之外使用 agent-invocation scope |
| 协议路由（AI SDK、AG-UI、A2A、MCP HTTP） | `ServerState` 始终提供 | agent-invocation scope |
| Canonical event 路由 | 接入 `EventModuleState` | 由 event store 可用性决定行为 |
| System 路由 | 始终挂载 | admin bearer + admin scope |
| Config 与 capabilities | `AdminApiConfig.expose_config_routes` 且接入 `ConfigStore` | admin bearer + admin scope |
| Admin run summary/runtime stats | `AdminApiConfig.expose_config_routes` | admin bearer + admin scope |
| Eval 路由 | `AdminApiConfig.expose_eval_routes` 且接入 config/eval modules | admin bearer + admin scope |
| Trace 路由 | `AdminApiConfig.expose_trace_routes` 且接入 trace module | admin bearer + admin scope |
| Metrics | 始终挂载 | 部署边界 |

## 健康检查与指标

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/health` | 就绪探针；检查 store 连通性，返回 `200` 或 `503` |
| `GET` | `/health/live` | 存活探针；始终返回 `200 OK` |
| `GET` | `/v1/system/info` | 管理控制台使用的服务身份信息：`{version, scope_id, uptime_seconds, config_store_enabled, audit_log_enabled, runtime_stats_enabled}` |
| `GET` | `/v1/system/modules` | 已挂载模块名，例如 `["run","admin","protocol","config","events","eval"]` |
| `GET` | `/metrics` | Prometheus 指标抓取入口 |

`GET /v1/system/info` 是管理控制台 “System” 卡片的数据源。`scope_id`
是当前 admin 请求经过可信 `HttpScopeProvider` 解析出的结果，只读展示；
接口不会把它作为 query/body 参数接受。它也不会暴露具体 store backend；
如果嵌入方需要暴露这些信息，应基于自己的 `ServerState` 额外添加路由。

## Threads

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/threads` | 列出 thread ID，支持分页与父子过滤；返回 `{ items, offset, limit, total, has_more, next_cursor }` |
| `POST` | `/v1/threads` | 创建 thread；body：`{ "title"?: string, "resource_id"?: string, "parent_thread_id"?: string }` |
| `GET` | `/v1/threads/summaries` | 列出 thread 摘要（id、`resource_id`、`parent_thread_id`、title、`updated_at`、`agent_id`），分页与父子过滤参数与 `/v1/threads` 相同 |
| `GET` | `/v1/threads/:id` | 获取 thread |
| `PATCH` | `/v1/threads/:id` | 更新 thread metadata |
| `DELETE` | `/v1/threads/:id` | 删除 thread；可通过 `?child_strategy=detach\|reject\|cascade`（默认 `detach`）控制直接和间接子 thread 的处理方式 |
| `POST` | `/v1/threads/:id/cancel` | 取消该 thread 上指定的排队或运行中 dispatch；返回 `cancel_requested` |
| `POST` | `/v1/threads/:id/decision` | 向该 thread 上等待中的 run 提交 HITL decision |
| `POST` | `/v1/threads/:id/interrupt` | 中断该 thread：递增 thread dispatch epoch、清空待执行 dispatch、取消活动 run；返回 `interrupt_requested` 与 `superseded_dispatches` 计数。与 `/cancel` 不同，它通过 `mailbox.interrupt()` 执行完整的清空并中断流程 |
| `PATCH` | `/v1/threads/:id/metadata` | 更新 metadata 的别名接口 |
| `GET` | `/v1/threads/:id/messages` | 列出消息，支持游标分页、序号窗口、排序、可见性与产生 run 过滤 |
| `POST` | `/v1/threads/:id/messages` | 作为后台 run 向该 thread 提交消息 |
| `POST` | `/v1/threads/:id/mailbox` | 向 thread mailbox 推送消息载荷 |
| `GET` | `/v1/threads/:id/mailbox` | 列出该 thread 的 mailbox dispatch |
| `GET` | `/v1/threads/:id/events` | 列出该 thread 作用域的持久 canonical events |
| `GET` | `/v1/threads/:id/events/stream` | 通过 SSE 流式读取该 thread 作用域的持久 canonical events |
| `GET` | `/v1/threads/:id/runs` | 列出该 thread 的 runs |
| `GET` | `/v1/threads/:id/runs/active` | 获取该 thread 当前活动 run（如有） |
| `GET` | `/v1/threads/:id/runs/latest` | 获取该 thread 最新 run |

`POST /v1/threads/:id/messages` 与 `POST /v1/runs/:id/inputs` 支持可选的 `mode` 字段。`queue` 会追加持久化 mailbox dispatch；`live_then_queue` 会先尝试把消息投递给活动 run，live 投递不可用时再排队；`steer` 是 `live_then_queue` 的别名；`interrupt_then_queue` 会先取消活动 run 再排队；`resume_open_run` 会继续可恢复的等待中 run。

Thread 列表 cursor 是 opaque token，并绑定到生成它的 query 形状。裸数字 cursor 只在没有任何筛选条件的 thread listing 中继续接受。带 resource、lineage 等筛选的列表必须使用同一 query 返回的 `next_cursor` 继续翻页。Backend scope filter 不是 HTTP 参数；scoped store wrapper 会根据 `ScopeContext` 在服务端内部注入。

## Runs

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/runs` | 列出 runs |
| `POST` | `/v1/runs` | 启动 run，并通过 SSE 返回事件 |
| `GET` | `/v1/runs/summary` | 带 admin 认证的 running、waiting、created 计数；管理控制台 dashboard 使用 |
| `GET` | `/v1/runs/:id` | 获取 run 记录 |
| `POST` | `/v1/runs/:id/inputs` | 在同一 thread 上作为后台 run 提交后续输入消息 |
| `POST` | `/v1/runs/:id/cancel` | 按 run ID 取消 |
| `POST` | `/v1/runs/:id/decision` | 按 run ID 提交 HITL decision |
| `GET` | `/v1/runs/:id/events` | 列出该 run 作用域的持久 canonical events |
| `GET` | `/v1/runs/:id/events/stream` | 通过 SSE 流式读取该 run 作用域的持久 canonical events |

## Canonical events

接入 event store 后会暴露 canonical event 路由。列表接口支持 `?cursor=` 和
`?limit=`（`1..=200`，默认 `50`），返回 `{ items, next_cursor, has_more }`。
流式接口优先从 `?cursor=` 开始，其次读取 `Last-Event-ID` header；两者都没有时
从当前时刻开始。

Event cursor 是 event store 的 opaque cursor。过期 cursor 返回 `410 Gone`。
SSE frame 使用 canonical cursor 作为 SSE `id`，并在 `data` 中序列化
`CanonicalEventHttp` JSON。

## Agent runtime stats

这些接口返回 observability plugin 发布到 `RuntimeStatsRegistry` 的滚动窗口快照。嵌入方没有接入 registry 时，两条路由都会返回 `503 {"error":"runtime_stats registry not configured"}`；管理控制台会把它当成未启用的功能并显示友好提示。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/agents/:id/runtime-stats?window=` | 单个 agent 快照。`window` 可选（`1h`、`24h`、`7d`、`<n>s`）；未设置时返回 registry 保留的完整窗口 |
| `GET` | `/v1/agents/runtime-stats` | 每个已知 agent 一个快照：`{ "agents": AgentRuntimeSnapshot[] }` |

`AgentRuntimeSnapshot` 结构（Rust source：`awaken_ext_observability::AgentRuntimeSnapshot`）：

```jsonc
{
  "agent_id": "research",
  "window_seconds": 86400,
  "bucket_window_seconds": 3600,
  "bucket_count": 24,
  "inference_count": 12,
  "error_count": 0,
  "input_tokens": 4180,
  "output_tokens": 980,
  "avg_inference_duration_ms": 480.5,
  "min_inference_duration_ms": 110,
  "max_inference_duration_ms": 1820,
  "p50_inference_duration_ms": 410,
  "p95_inference_duration_ms": 1410,
  "p99_inference_duration_ms": 1810,
  "inference_duration_histogram": [
    { "upper_bound_ms": 100, "count": 0 },
    { "upper_bound_ms": 250, "count": 1 }
    /* ... */
  ],
  "suspensions": 0,
  "handoffs": 0,
  "delegations": 0,
  "tool_calls_by_tool": [
    {
      "tool": "search",
      "call_count": 7,
      "failure_count": 0,
      "total_duration_ms": 2840,
      "avg_duration_ms": 405.7,
      "min_duration_ms": 110,
      "max_duration_ms": 920,
      "p50_duration_ms": 380,
      "p95_duration_ms": 880,
      "p99_duration_ms": 920
    }
  ]
}
```

`inference_duration_histogram` 是延迟值分布，不是时间序列。需要粗粒度时间过滤时使用 `window` 查询参数。

## Config 与 Capabilities

这些接口由 `config_routes()` 提供。读取与 schema 接口要求 `ServerState` 挂接 config store；写接口还要求挂接 config runtime manager，才能在普通配置写入后校验并发布新的 registry snapshot。缺少这些配置时会返回 `400`，错误为 `config management API not enabled`。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/capabilities` | 列出 agents、tools、plugins、models、providers 和 config namespaces |
| `GET` | `/v1/config/:namespace` | 列出某个 namespace 下的配置项 |
| `POST` | `/v1/config/:namespace` | 创建配置项；body 必须含 `"id"` |
| `POST` | `/v1/config/:namespace/validate?id=` | 干跑校验。执行和 create/update 相同的 `prepare_body` + schema check，但不持久化、不 apply。成功返回 `{"ok":true,"normalized":{...}}`，失败返回和真实保存相同的 `400`/`409`。可选 `?id=` 允许调用方在不使用 `:id` path 的情况下校验一次 update |
| `GET` | `/v1/config/:namespace/:id` | 获取单个配置项 |
| `PUT` | `/v1/config/:namespace/:id` | 整体替换配置项 |
| `DELETE` | `/v1/config/:namespace/:id` | 删除配置项。`?force=true` 会绕过依赖检查（并记录该 override）。如果其它记录依赖它，返回 `409` 与 `{"error":"...","used_by":[...]}` |
| `POST` | `/v1/config/:namespace/:id/restore` | 把旧版本恢复到 editing store。body：`{"version": "<event-id>"}`，其中 event id 是要回滚到的审计事件 id。会写入新的 `restore` 审计事件，并设置 `restored_from = <event-id>`。该接口不会热替换 runtime registry；审查后用普通配置写入发布恢复出的 payload |
| `GET` | `/v1/config/:namespace/$schema` | 返回该 namespace 的 JSON Schema |
| `GET` | `/v1/config/:namespace/meta` | 列出所有配置项的 metadata（created_at / updated_at / version / actor），不返回完整 body |
| `GET` | `/v1/config/:namespace/:id/meta` | 单个配置项 metadata |
| `GET` | `/v1/config/diagnostics` | registry 级校验报告；暴露悬空 model/provider 引用等跨实体不一致，覆盖单实体 validate 看不到的问题 |
| `GET` | `/v1/config/providers/:id/removal-preview` | 删除 provider 前预览会受影响的 agents、models、pools 与 providers |
| `POST` | `/v1/config/agents/:id/overrides` | 校验 agent override patch，不持久化 |
| `PATCH` | `/v1/config/agents/:id/overrides` | 合并一个 partial agent override object。支持用 `null` 清除字段，也支持 `_clear` 字段列表。以 `update` + `overrides` payload 写入审计 |
| `DELETE` | `/v1/config/agents/:id/overrides` | 删除该 agent 的全部 overrides，回到 base spec |
| `DELETE` | `/v1/config/agents/:id/overrides/:field` | 删除某个 overridden field |
| `PATCH` | `/v1/config/tools/:id/overrides` | patch 内置 tool 的 `description`。未知字段会被拒绝；空描述和超过 4096 bytes 的值无效；`null` 会清除 override |
| `DELETE` | `/v1/config/tools/:id/overrides` | 删除该 tool description override |
| `DELETE` | `/v1/config/tools/:id/overrides/:field` | 删除 tool 的某个 overridden field |
| `GET` | `/v1/agents/:id/permission-preview` | 解析 agent 的有效工具权限（内置 + plugin + MCP，应用 include/exclude 后）。编辑器 Tools tab 用它展示 LLM 实际会看到什么 |
| `GET` | `/v1/agents` | `/v1/config/agents` 的便捷别名 |
| `GET` | `/v1/agents/:id` | `/v1/config/agents/:id` 的便捷别名 |
| `POST` | `/v1/providers/:id/test` | 探测已有 provider。返回 `{"ok": bool, "latency_ms": number, "error"?: string}`。管理控制台在编辑器和 providers 列表的 “Test” 按钮中使用它 |
| `GET` | `/v1/mcp-servers/:id/status` | 见下方 [MCP server status](#mcp-server-status) |
| `POST` | `/v1/mcp-servers/:id/restart` | 重新连接托管 MCP server。成功返回 `202`，并写入 `restart` 审计事件 |
| `GET` | `/v1/audit-log?…` | 查询 admin audit events。返回 `{"items": AuditEvent[], "next_cursor": string?}`。未配置审计日志时返回 `503 {"error":"audit log is not configured"}`。见下方 [Admin audit log](#admin-audit-log) |

`GET /v1/capabilities` 会包含每个已注册插件的 `config_schemas`。管理控制台用它渲染 agent 级插件配置表单，并把值保存到 `AgentSpec.sections`。每个条目包含 section key、JSON Schema、可选展示 metadata、默认值、UI schema hints 和可选 editor hint；客户端不认识 editor 时回退到 JSON Schema 表单。create/update/delete 或 override 修改成功后，runtime manager 会发布新的 registry snapshot，因此后续 `/v1/runs` 会使用更新后的 agents、models、providers、MCP servers 与 plugin sections。Restore 是例外：它只把恢复出的 payload 写入 `ConfigStore`，让操作者先审查回滚状态，再通过一次普通配置写入发布。

当前内置 namespace：

- `agents`
- `models`
- `model-pools`
- `providers`
- `mcp-servers`
- `skills`

### MCP server status

```jsonc
{
  "connected": true,
  "last_error": null,                  // 最近一次健康检查失败时为 string
  "tools": [
    { "name": "search", "description": "Search the web." }
  ],
  "consecutive_failures": 0,           // 上次成功后的连续失败次数
  "last_attempt_at": 1777708820,       // unix seconds，首次 probe 前为 null
  "last_success_at": 1777708820,       // unix seconds，首次成功前为 null
  "reconnecting": false,
  "permanently_failed": false,         // manager 放弃后为 true
  "session_generation": 2,             // HTTP session reset/reinitialize generation
  "transport_reconnect_count": 0,      // 成功重建 runtime 的次数
  "last_init_at": 1777708820           // unix seconds，initialize 前为 null
}
```

`consecutive_failures` 与 `last_success_at` 来自已有的 `McpRefreshHealth` budget。没有单独的 “last 24h errors” counter；health budget 是事实来源。

该端点有意不暴露原始 HTTP `MCP-Session-Id`。`transport_reconnect_count` 统计 runtime tear-down/recreate 次数；HTTP 404 session reset 抖动通过 `session_generation` 与 `last_init_at` 体现。

### Admin audit log

`AuditEvent`：

```jsonc
{
  "id": "01HXJK...",                   // ULID
  "ts": "2026-05-02T07:58:14.900Z",    // RFC 3339
  "actor": "<sha256-prefix>",          // bearer token 的 SHA-256，可追加 X-Awaken-Actor label
  "action": "create" | "update" | "delete" | "restart" | "publish" | "restore",
  "resource": "agents/research",       // "<namespace>/<id>"
  "before": { /* spec snapshot */ },
  "after":  { /* spec snapshot */ },
  "ip": "127.0.0.1",
  "request_id": null,
  "restored_from": null                // 本次 restore 回滚到的 event id
}
```

过滤：`?resource=`、`?action=`、`?actor=`、`?since=`、`?until=`、`?limit=`（限制到 `[1, 1000]`）、`?cursor=` 分页。

## Trace 路由

Trace 路由需要 admin 认证，只有在 `AdminApiConfig.expose_trace_routes = true`
且接入 trace module 时挂载。Trace 可能暴露 prompt、tool arguments 和模型回复；
默认值是 `false`。未知 query 字段会被拒绝。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/traces?agent_id=&prompt_id=&experiment_id=&variant_name=&since=&limit=` | 列出可追踪的 runs |
| `GET` | `/v1/traces/:run_id?offset=&limit=` | 以 NDJSON 返回某个 run 的一页 trace events，并带分页 header |
| `POST` | `/v1/traces/:run_id/pin` | pin 一条 trace，便于后续审查或 fixture curation |

`since` 使用 RFC 3339。`limit=0` 会被拒绝。Trace event page 的 `limit`
上限是 `1000`，并通过 `x-trace-total-events` 和 `x-trace-next-offset`
返回分页 metadata。Trace event response 是 NDJSON。

## Eval 路由

Eval 路由需要 admin 认证，只有在 `AdminApiConfig.expose_eval_routes = true` 且
接入 config/eval modules 时挂载。请求规模由 `ServerConfig.eval_limits` 控制。

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/eval/datasets` | 列出 eval datasets |
| `POST` | `/v1/eval/datasets` | 创建 eval dataset |
| `GET` | `/v1/eval/datasets/:id` | 获取一个 dataset |
| `PUT` | `/v1/eval/datasets/:id` | 替换一个 dataset |
| `DELETE` | `/v1/eval/datasets/:id` | 删除一个 dataset；支持 revision check |
| `POST` | `/v1/eval/datasets/:id/items` | 把 trace/dialogue input 整理为 dataset fixtures |
| `POST` | `/v1/eval/datasets/:id/fixtures` | 直接追加 fixtures |
| `POST` | `/v1/eval/datasets/:id/import-traces` | 从 traces 导入 fixtures |
| `POST` | `/v1/eval/datasets/:id/import-dialogue` | 从 dialogue 导入 fixtures |
| `GET` | `/v1/eval/runs?dataset_id=&limit=` | 列出 eval runs |
| `POST` | `/v1/eval/runs` | 启动 dataset eval run |
| `GET` | `/v1/eval/runs/:id` | 获取 eval run 及其 item reports |
| `POST` | `/v1/eval/online` | 执行一次不保存 dataset 的 ad-hoc online eval |

Eval run list 支持 `dataset_id`、`limit`、`cursor`、`since_secs`、
`until_secs` 和 `aggregate=samples`。Run detail 支持 `baseline=<run_id>`，
用于返回 baseline comparison 字段。Dataset DELETE/PUT 类 mutation 在请求带
revision 时执行 revision check。Dataset import 路由接受 trace/dialogue selector
和可选上限；trace import 未指定上限时使用
`ServerConfig.eval_limits.default_import_traces_max`。

`POST /v1/eval/runs` 启动 scripted 或 live dataset eval。`samples` 和 matrix
规模受 `ServerConfig.eval_limits` 校验；live execution 需要可运行的
runtime/provider wiring。`POST /v1/eval/online` 对 ad-hoc prompt 使用同一套限制，
不会创建 dataset。

## AI SDK v6 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/ai-sdk/chat` | 启动 chat run，并流式返回协议编码事件 |
| `POST` | `/v1/ai-sdk/agent-previews/runs` | 使用未保存的草稿 `AgentSpec` 运行；管理控制台预览功能使用 |
| `POST` | `/v1/ai-sdk/threads/:thread_id/runs` | 在指定 thread 上启动 AI SDK run |
| `POST` | `/v1/ai-sdk/agents/:agent_id/runs` | 在指定 agent 上启动 AI SDK run |
| `GET` | `/v1/ai-sdk/chat/:thread_id/stream` | 按 thread ID 续接 SSE |
| `GET` | `/v1/ai-sdk/threads/:thread_id/stream` | 按 thread ID 续接 SSE 的别名 |
| `GET` | `/v1/ai-sdk/threads/:thread_id/replay` | 按 thread ID 回放持久 AI SDK 协议 frame |
| `GET` | `/v1/ai-sdk/chat/:thread_id/replay` | 持久 AI SDK 协议回放别名 |
| `GET` | `/v1/ai-sdk/threads/:thread_id/messages` | 列出 thread messages |
| `POST` | `/v1/ai-sdk/threads/:thread_id/cancel` | 取消该 thread 上活动或排队中的 run |
| `POST` | `/v1/ai-sdk/threads/:thread_id/interrupt` | 中断 thread（递增 dispatch epoch、清空 pending dispatch、取消活动 run）|

AI SDK 的数字 `Last-Event-ID` 是 live replay buffer 位置。持久 protocol replay
使用 replay endpoint 返回的 opaque protocol replay cursor。Replay endpoint 支持
`?cursor=` 或 `Last-Event-ID`，以及 `?limit=`（默认 `100`，最大 `500`）。未配置
replay 存储返回 `503`，非法 cursor 返回 `400`，过期 cursor 返回 `410 Gone`。

## AG-UI 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/ag-ui/run` | 启动 AG-UI run，并流式返回 AG-UI 事件 |
| `POST` | `/v1/ag-ui/threads/:thread_id/runs` | 在指定 thread 上启动 AG-UI run |
| `POST` | `/v1/ag-ui/agents/:agent_id/runs` | 在指定 agent 上启动 AG-UI run |
| `POST` | `/v1/ag-ui/threads/:thread_id/interrupt` | 中断 thread |
| `GET` | `/v1/ag-ui/threads/:thread_id/replay` | 按 thread ID 回放持久 AG-UI 协议 frame |
| `GET` | `/v1/ag-ui/threads/:id/messages` | 列出 thread messages |

AG-UI replay 与 AI SDK replay 使用相同的 cursor、limit、`503`、`400` 和
`410` 语义。

## A2A 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/.well-known/agent-card.json` | 获取公共/默认 agent card |
| `POST` | `/v1/a2a/message:send` | 向公共/默认 A2A agent 发送消息 |
| `POST` | `/v1/a2a/message:stream` | 通过 SSE 进行流式发送 |
| `GET` | `/v1/a2a/tasks` | 列出 A2A tasks |
| `GET` | `/v1/a2a/tasks/:task_id` | 获取 task 状态 |
| `POST` | `/v1/a2a/tasks/:task_id:cancel` | 取消 task |
| `POST` | `/v1/a2a/tasks/:task_id:subscribe` | 通过 SSE 订阅 task 更新 |
| `POST` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | 创建 push notification config |
| `GET` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs` | 列出 push notification configs |
| `GET` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | 获取 push notification config |
| `DELETE` | `/v1/a2a/tasks/:task_id/pushNotificationConfigs/:config_id` | 删除 push notification config |
| `GET` | `/v1/a2a/extendedAgentCard` | 获取 extended agent card；未启用时返回 `501` |
| `POST` | `/v1/a2a/:tenant/message:send` | 向 tenant 作用域 agent 发送消息 |
| `POST` | `/v1/a2a/:tenant/message:stream` | tenant 作用域流式发送 |
| `GET` | `/v1/a2a/:tenant/tasks` | 列出 tenant 作用域 tasks |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id` | 获取 tenant 作用域 task 状态 |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id:cancel` | 取消 tenant 作用域 task |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id:subscribe` | 订阅 tenant 作用域 task 更新 |
| `POST` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs` | 创建 tenant 作用域 push notification config |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs` | 列出 tenant 作用域 push notification configs |
| `GET` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs/:config_id` | 获取 tenant 作用域 push notification config |
| `DELETE` | `/v1/a2a/:tenant/tasks/:task_id/pushNotificationConfigs/:config_id` | 删除 tenant 作用域 push notification config |
| `GET` | `/v1/a2a/:tenant/extendedAgentCard` | 获取 tenant 作用域 extended agent card |

## MCP HTTP 路由

| 方法 | 路径 | 说明 |
|---|---|---|
| `POST` | `/v1/mcp` | MCP JSON-RPC request/response 入口。`initialize` 会创建 session 并返回 `MCP-Session-Id`；后续 request、notification 和 response 都需要该 header |
| `GET` | `/v1/mcp` | 为 MCP server-initiated SSE 预留；当前返回 `405` |
| `DELETE` | `/v1/mcp` | 根据 `MCP-Session-Id` 终止已知 MCP HTTP session；返回 `204` 或 `404` |

`initialize` 请求不能带 `MCP-Session-Id`。`tools/call` 可能返回流式响应。所有 MCP HTTP 路由都会在存在 `Origin` header 时校验来源。

## 常见查询参数

分页只适用于明确支持 `offset`、`limit` 或 `cursor` 的 route family；不是每个
端点都会自动接受这些参数。

- `offset`：跳过的条数
- `limit`：返回上限，范围限制在 `1..=200`（默认 `50`）
- `cursor`：不透明分页游标，提供后优先于 `offset`。游标绑定到原始 query 形状，filter 改变时会被拒绝
- 响应中的 `next_cursor` / `prev_cursor` 在仍有更多页时返回

Thread 列表过滤（`/v1/threads`、`/v1/threads/summaries`）：

- `resource_id`（别名 `resourceId`）：按外部资源分组过滤
- `parent_thread_id`（别名 `parentThreadId`）：仅返回该父 thread 的直接子 thread
- `root`：为 `true` 时仅返回无父 thread 的根 thread；不能与 `parent_thread_id` 同时使用

消息列表过滤（`/v1/threads/:id/messages` 及各协议别名）：

- `after`、`before`：序号窗口
- `order`：`asc`（默认）或 `desc`
- `visibility`：`external`（默认）、`internal` 或 `all`
- `run_id`（别名 `runId`）：仅保留由该 run 产生的消息

Run 列表过滤：

- `status`：`created`、`running`、`waiting` 或 `done`

## 错误格式

大多数 route group 返回：

```json
{ "error": "human-readable message" }
```

MCP 路由返回 JSON-RPC error object，而不是上面的通用形状。

## 相关

- [通过 SSE 暴露 HTTP](/awaken/zh-cn/how-to/expose-http-sse/)
- [配置](/awaken/zh-cn/reference/config/)
