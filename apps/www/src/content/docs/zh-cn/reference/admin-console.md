---
title: "管理控制台"
description: "Awaken 管理控制台是 apps/admin-console 中的 Vite + React 19 SPA，通过 admin HTTP API 连接正在运行的 awaken-server。"
---

Awaken 管理控制台（`apps/admin-console`）是一个 Vite + React 19 SPA。它通过 admin HTTP API 连接正在运行的 `awaken-server`，让操作者在不重启服务的情况下查看和编辑 live config。

本文是界面与后端依赖清单。操作流程见[使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)。

## 截图

这些截图是使用 sample API data 生成的静态文档图，并随 docs site 一起发布，
保证操作手册和 README 指向同一组界面。实际运行中的控制台会从配置的后端 API
读取这些值。

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/01-dashboard.png">
      <img src="/awaken/assets/admin-console/01-dashboard.png" alt="管理控制台 Dashboard，包含 live workload、agent activity、recent activity、provider/MCP health 与只读 scope 元数据。" loading="lazy" />
    </a>
    <figcaption>Dashboard 与当前 scope</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/02-agent-editor.png">
      <img src="/awaken/assets/admin-console/02-agent-editor.png" alt="Agent 编辑器，包含 basics、tools、plugins、delegates、advanced JSON、history、保存控制和 preview chat。" loading="lazy" />
    </a>
    <figcaption>Agent editor</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/cmdk.png">
      <img src="/awaken/assets/admin-console/cmdk.png" alt="管理控制台命令面板，可快速跳转 Dashboard、Agents、Providers、MCP Servers、Audit Log 和 Assistant。" loading="lazy" />
    </a>
    <figcaption>Command palette</figcaption>
  </figure>
</div>

## 架构

| 层 | 位置 | 用途 |
|---|---|---|
| Token pipeline | `packages/design-tokens/`（`@awaken/design-tokens`） | Style Dictionary v4 源，生成 `--aw-*` CSS variables |
| Generated CSS | `packages/design-tokens/dist/css/`（gitignored） | `tokens.css`、`tokens-dark.css`、`tokens-auto-dark.css`、`tokens.json`，由 `pnpm tokens:build` 生成 |
| Tailwind | `tailwind.config.ts` | 把 `--aw-*` tokens 暴露成语义 Tailwind class |
| Routing | `src/app.tsx` | data router，支持 unsaved-changes guard |
| Auth | `src/components/auth-provider.tsx` | bearer token 存在 `localStorage`，并显示在 topbar 状态 pill 中 |

## 后端依赖

每个界面都消费 [HTTP API](/awaken/zh-cn/reference/http-api/) 中的一个或多个端点。控制台不会伪造数据；端点返回 `503` 或 `null` 时，对应组件会折叠成占位或“功能未启用”提示。

| 界面 | Endpoint(s) | 失败表现 |
|---|---|---|
| Sidebar nav counts | `/v1/config/{providers,mcp-servers,agents}` | 计数省略 |
| Topbar status pill | `/v1/capabilities` | 按错误类型显示状态 |
| Dashboard workload card | `/v1/runs/summary` | `404` 或后端错误时显示禁用提示 |
| Dashboard stat cards | `/v1/capabilities` + 各 namespace list | 失败时不渲染对应卡片 |
| Health card | `/v1/config/providers` + `/v1/config/mcp-servers` | 行内显示错误 |
| Recent activity | `/v1/audit-log?limit=12` | `503` 时显示 audit log disabled 提示 |
| System card | `/v1/system/info` + `/v1/system/modules` | 出错时隐藏 |
| Agents list inferences | `/v1/agents/runtime-stats` | `503` 时显示 banner 与 `n/a` |
| Provider Test button | `POST /v1/providers/:id/test` | toast 显示后端错误 |
| Model Test action | `POST /v1/eval/online` | eval 路由隐藏时显示禁用提示 |
| Editor Validate button | `POST /v1/config/:ns/validate` | toast 显示后端错误 |
| Editor History tab | `/v1/audit-log?resource=agents/{id}` | `503` 时为空列表 |
| Editor Restore action | `POST /v1/config/:ns/:id/restore` | 失败时 toast |
| Recent runs drawer | `/v1/traces?agent_id=…`、`/v1/traces/:run_id` | trace 路由或 trace store 不可用时显示禁用提示 |
| Save trace as fixture | `/v1/eval/datasets`、`/v1/eval/datasets/:id/items` | eval 路由隐藏时显示禁用提示 |
| Datasets | `/v1/eval/datasets`、`/v1/eval/datasets/:id` | eval 路由隐藏时显示禁用提示 |
| Eval runs | `/v1/eval/runs`、`/v1/eval/runs/:id` | eval 路由隐藏时显示禁用提示 |
| MCP Live Status card | `/v1/mcp-servers/:id/status` | Loading / Unavailable |
| MCP Restart button | `POST /v1/mcp-servers/:id/restart` | toast |

## 主要界面

### Chrome（sidebar + topbar）

- Sidebar 按工作流分组：Agents、Resources（Models、Tools、Skills）、
  Infrastructure（Providers、MCP Servers）、Observe（Dashboard、Audit Log、
  Datasets、Eval Runs、Eval Reports）和 Assistant。
- Topbar 提供 breadcrumb、notification bell stub、bearer-token 状态 pill，以及搜索 / 命令入口。

### Dashboard

- Workload card 使用 `/v1/runs/summary`，显示 running、waiting、created run 计数。
- 六个统计卡片：Agents、Skills、Models、Providers、MCP、Tools，来自 `/v1/capabilities` 与相关列表。
- Health card：显示 provider 是否配置 key，以及 MCP server restart policy。
- Activity timeline：最近 8 条 audit events。
- System card：`version`、只读 `scope_id`、`uptime`、config store / audit log / runtime stats 三个 wiring flags，以及 `/v1/system/modules` 返回的模块名。

`scope_id` 是运维侧信号，不是管理控制台筛选条件。Server 会为每个请求通过可信
`HttpScopeProvider` 解析 scope。托管版或多 workspace 产品应在 auth/provider
层切换 scope，并在界面只读展示解析结果，而不是让浏览器提交任意 scope key。

### Agent 列表与编辑器

Agent 列表显示 filter chips、plugin chips 和 runtime stats 中的 inference/error/p95 信息。runtime stats 未接入时列显示 `n/a`。

Agent 编辑器包含 Basics / Tools / Plugins / Delegates / Advanced / History tabs：

- **Validate** 调用 `POST /v1/config/agents/validate`，只校验不保存。
- **Save** 对新资源调用 `POST /v1/config/agents`，对已有资源调用 `PUT /v1/config/agents/:id`。
- Tools tab 使用 `/v1/agents/:id/permission-preview` 解析有效工具权限。
- 右侧 preview chat 调用 `POST /v1/ai-sdk/agent-previews/runs`，可以在保存前运行草稿 `AgentSpec`。
- 已保存 agent 会显示 Recent runs drawer；它读取 `/v1/traces`，可打开单个 run 的 NDJSON trace page，并在 trace 与 eval 路由都启用时保存为 eval fixture。
- History tab 从 `/v1/audit-log?resource=agents/{id}` 读取版本历史，并用 `POST /v1/config/:ns/:id/restore` 回滚。Restore 会把选中的 snapshot 写回 `ConfigStore`，但不会调用 runtime hot-swap；确认后如果要让它成为后续 run 使用的 active registry snapshot，请再通过普通 Save / PUT 发布一次。

### Providers 与 MCP Servers

Providers 列表的 **Test** 按钮调用 `POST /v1/providers/:id/test`，返回成功延迟或后端错误。

MCP Servers 编辑页读取 `/v1/mcp-servers/:id/status` 展示连接状态、握手、工具数量、失败次数与最近尝试时间；**Restart** 按钮调用 `POST /v1/mcp-servers/:id/restart`，成功时写入 `restart` audit event。

### Skill registry

Skill registry 当前是只读界面，来自 `/v1/capabilities`.skills。每张卡展示 allowed tools、source paths、arguments，以及基于真实 `SkillInfo` 字段生成的 “What the LLM sees” preview。

### Audit log、Datasets、Eval runs 与 Eval reports

Audit Log 页面支持 `since` / `until` / `action` / `resource` / `actor` 过滤，点击行可查看完整事件 JSON 和 before/after diff，并在适用时执行 restore。

Datasets 页面管理 `/v1/eval/datasets*` 记录与 fixtures；trace capture 通过
`POST /v1/eval/datasets/:id/items` 追加 fixture。Eval Runs 页面列出并查看
`/v1/eval/runs*`，dataset detail 可以启动新的 eval run。

Eval Reports 页面仍是浏览器内 NDJSON 查看器：拖入外部 report，可选 baseline，
然后按 All / Passing / Failing / Regressions / Newly fixed 查看结果。上传的 report
不由服务端持久化；服务端持久化的是 `/v1/eval/*` 管理的 dataset/run。

Eval 与 trace payload 可能包含 prompt、tool arguments 和模型回复。控制台会在这些
界面显示隐私提示；访问范围应通过 admin bearer token 控制。

## 可选子系统提示

| 子系统 | Endpoint | UI 信号 |
|---|---|---|
| Audit log | `/v1/audit-log` 返回 `503` | Dashboard activity 与 Audit Log 页面显示禁用提示 |
| Runtime stats | `/v1/agents/runtime-stats` 返回 `503` | Agents list banner + Inferences 列 `n/a` |
| Config store | `/v1/system/info` 返回 `config_store_enabled: false` | System card 中显示 neutral 状态 |
| Trace routes/store | `/v1/traces*` 返回 `404`/`503` | Recent runs drawer 解释 trace persistence 不可用 |
| Eval routes/store | `/v1/eval/*` 返回 `404`/`503` | Datasets、Eval Runs、model test 和 trace-to-fixture 控件显示禁用提示 |

## REST-only 功能

以下 server 能力已经由 HTTP API 暴露，但当前控制台没有独立界面。请使用 admin bearer token（`Authorization: Bearer <token>`）通过 REST 调用。

| 区域 | Endpoints | 说明 |
|---|---|---|
| Threads | `GET/POST /v1/threads`、`GET/PATCH/DELETE /v1/threads/:id`、`GET/POST /v1/threads/:id/messages` | 控制台聚焦配置；thread 浏览走 HTTP API |
| Runs | `GET/POST /v1/runs`、`GET /v1/runs/:id`、`GET /v1/threads/:id/runs`、`GET /v1/threads/:id/runs/{latest,active}` | 执行记录以 HTTP API 为准 |
| Run control | `POST /v1/runs/:id/cancel`、`POST /v1/runs/:id/inputs`、`POST /v1/threads/:id/{cancel,interrupt}` | 精确执行控制走 REST |
| HITL decisions | `POST /v1/runs/:id/decision`、`POST /v1/threads/:id/decision` | 提交挂起工具调用的恢复 / 取消决策 |
| Mailbox | `GET/POST /v1/threads/:id/mailbox` | 查看或推送 inter-agent dispatch |
| Canonical events | `GET /v1/threads/:id/events`、`GET /v1/runs/:id/events` 以及 `/stream` 变体 | 控制台用 trace/eval view 服务运维流程；canonical event replay 仍走 REST/SSE |
| Skill CRUD | `POST /v1/config/skills`、`PUT/DELETE /v1/config/skills/:id` | 控制台只读；自动化可直接调用 REST |
| Config diagnostics | `GET /v1/config/diagnostics` | REST-only registry 级校验报告；暂无控制台页面 |
| Permission preview | `GET /v1/agents/:id/permission-preview` | Agent editor Tools tab 使用；无独立页面 |

扩展控制台时应复用这些 HTTP surfaces，不要创建第二套 operator API。

## 控制台未展示的 server 数据

- 没有 `/v1/agents/:id/active-runs`，所以 per-agent dashboard 展示 rolling stats，而不是当前 running / paused / blocked 面板。
- Eval reports 由浏览器读取用户提供的 NDJSON；保存的 eval datasets/runs 使用 `/v1/eval/*` API，不走该上传流程。
- Skill version history、file tree 与 activation log 未暴露，因此控制台中的 skills 是只读的。
- Topbar notification bell 没有 server endpoint。
- MCP `/status` 暴露连接健康与重启计数，不暴露 per-tool latency 或 rolling error totals。
- Runtime stats 暴露保留窗口内的聚合总量和延迟分布，不是 per-agent time-series endpoint。

## 相关

- [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)
- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [启用可观测性](/awaken/zh-cn/how-to/enable-observability/)
