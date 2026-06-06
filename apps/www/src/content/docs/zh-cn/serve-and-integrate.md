---
title: "服务与集成"
description: "开发期把 AgentRuntime 包装成 server、protocol、mailbox、config 和 admin surfaces。"
---

Serve & Integrate 是进入 [调优与运营](/awaken/zh-cn/operate/) 前的最后一个开发
步骤：把本地 runtime 暴露给其他系统调用。应在
[状态与存储](/awaken/zh-cn/state-and-storage/) 接好之后再做，因为 server 模式依赖
durable stores 支撑 mailbox、config、events、trace、eval 和 recovery。

价值在于一套 agent 实现服务多类客户端：server 模式负责 wire、队列、配置、
Trace/Eval 与管理 surface，runtime 仍然是执行核心。

## Runtime 开发 vs Server 开发

Runtime 开发把 Awaken 当作进程内 Rust library 使用。你的应用自己负责 transport、
请求队列、鉴权、配置加载和运维工作流；代码构造 `AgentRuntime`，注册可执行
能力，并决定如何把 `RunActivation` 送进去。这个模式仍然要求有 Tokio 可用的
标准 Rust async 环境，不是 `no_std` 或无 Tokio 的嵌入式设备目标。

Server 开发使用同一个 runtime execution core，但由 `awaken-server` 接管它外层的
服务边界。Server 额外提供：

- threads、runs、config、capabilities、health 的 HTTP 资源。
- SSE streaming，以及 AI SDK v6、AG-UI、A2A、MCP、ACP 协议适配器。
- mailbox-backed 后台派发，让 run 可恢复、可取消、可中断，并支持 HITL gate。
- `/v1/config/*` 托管配置 API：校验、持久化、编译并发布 registry snapshot。
- 管理控制台工作流：编辑 agent/model/provider/plugin section，预览行为，恢复
  配置版本，查看审计数据。
- server/store scope 边界、protocol replay、outbox/event 发布，以及基于存储的
  run recovery。

在线配置通过发布 `AgentSpec`、`ModelSpec`、provider 设置、plugin section、MCP
server、skill 和权限规则来生成可直接调用的 agent。真正可执行的 tools、plugins、
providers、stores 和 backend factories 仍然必须由代码提供。

## 服务化后发生了什么

服务化 Awaken 不会产生第二套 agent 实现。Server 包住的是同一个可以进程内直接运行的 `AgentRuntime`：

1. 协议适配器把客户端消息转换成 `RunActivation`。
2. Mailbox 持久化并分发工作，使 run 可以恢复、取消、中断或在 worker 侧重新领取。
3. Runtime events 被转码成调用方需要的协议流：AI SDK v6、AG-UI、A2A、MCP HTTP 或 ACP stdio。
4. Admin 路由修改 `/v1/config/*`；成功的 create/update/delete 会写入配置、编译通过校验的 registry snapshot，并让后续 run 使用新 snapshot。

这样同一套后端可以同时服务本地 Rust 调用、浏览器聊天客户端、运维工具和 agent-to-agent 集成。Tools 和 plugins 仍然在代码里；prompt、model、provider wiring、权限规则、MCP server 和 agent profile 进入托管配置。

## Server module wiring

`ServerState` 由多个模块组装。只有同时接入模块并打开对应暴露开关的 surface
才会出现在路由树里。

| Module | 增加的能力 | 典型接线 |
|---|---|---|
| Run | `/v1/threads`、`/v1/runs`、health | `AgentRuntime`、`Mailbox`、`ThreadRunStore`、resolver |
| Protocol | AI SDK v6、AG-UI、A2A、MCP HTTP | 同一 run module 加协议适配器 |
| Config | `/v1/config/*`、`/v1/capabilities`、audit、provider/MCP admin | `ConfigStore`、`ConfigRuntimeManager`、可选 `AuditLogStore` |
| Events | `/v1/threads/:id/events`、`/v1/runs/:id/events` | `EventStore` 加 server staged commits |
| Eval | `/v1/eval/*` | Config module、eval stores/services、`ServerConfig.eval_limits` |
| Trace | `/v1/traces*` | Trace store 和 `AdminApiConfig.expose_trace_routes = true` |

`AdminApiConfig.expose_config_routes`、`expose_eval_routes` 和
`expose_trace_routes` 会分别控制 admin surface。只要其中任何一个 surface 暴露，
启动时就要求配置 admin bearer token。

Scope 在 server 边界通过 `HttpScopeProvider` 解析。OSS/local 默认使用
`SingleScopeProvider::default_scope()`。多租户部署应从认证后的请求上下文派生
`ScopeContext`，由 server scoped store 负责下推 backend filter，并只把解析出的
`scope_id` 作为只读界面上下文展示。

## 代码参考

接 host application 时优先参考：

- `crates/awaken-doctest/examples/http_app_builder.rs` —— 离线示例：
  `AgentRuntime` → `Mailbox` → `ServerState`。
- `crates/awaken-server/src/app.rs` —— `ServerState` builder，覆盖 config、
  trace、event、eval、admin、runtime stats、scope 和 A2A push relay modules。
- `crates/awaken-server/src/app/modules.rs` —— module-specific state structs
  以及它们启用的 route surfaces。
- `crates/awaken-server/tests/http_api.rs` 与
  `crates/awaken-server/tests/transport_tests.rs` —— served runs 的 route 与
  transport smoke coverage。

## 从这里开始

1. 先确认 [状态与存储](/awaken/zh-cn/state-and-storage/) 中的 thread/run data、config、mailbox、events、trace、eval 和 profile/shared state 选择。
2. 阅读 [通过 SSE 暴露 HTTP](/awaken/zh-cn/how-to/expose-http-sse/)，先把 runtime 放到 HTTP 和流式端点后面。
3. 阅读 [集成 AI SDK 前端](/awaken/zh-cn/how-to/integrate-ai-sdk-frontend/)，对接 React + AI SDK v6。
4. 阅读 [集成 CopilotKit（AG-UI）](/awaken/zh-cn/how-to/integrate-copilotkit-ag-ui/)，对接 CopilotKit 前端。
5. 阅读 [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)，让操作者通过浏览器调优 agent。
6. 阅读 [部署到生产](/awaken/zh-cn/how-to/deploy-to-production/),加固 server:持久化 store、TLS、密钥、健康探针。

## 建议同时查阅

- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [AI SDK v6 协议](/awaken/zh-cn/reference/protocols/ai-sdk-v6/)
- [AG-UI 协议](/awaken/zh-cn/reference/protocols/ag-ui/)
- [A2A 协议](/awaken/zh-cn/reference/protocols/a2a/)
- [MCP HTTP 协议](/awaken/zh-cn/reference/protocols/mcp/)
- [ACP 协议](/awaken/zh-cn/reference/protocols/acp/)
