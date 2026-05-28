---
title: "使用管理控制台"
description: "管理控制台是 Awaken runtime 的操作界面。本文说明操作者最常用的浏览器工作流。"
---

管理控制台是 Awaken runtime 的操作界面。本文说明操作者最常用的浏览器工作流。完整界面清单见[管理控制台参考](/awaken/zh-cn/reference/admin-console/)。

## 前置条件

- 浏览器可以访问正在运行的 `awaken-server`。默认后端 URL 是 `http://127.0.0.1:38080`。
- 已配置 admin bearer token：`AWAKEN_ADMIN_API_BEARER_TOKEN` 环境变量，或 server config 中的 `AdminApiConfig.bearer_token`。
- 本地运行 `apps/admin-console` dev server，或部署生产构建。

```sh
# Terminal 1 — runtime
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
cargo run -p ai-sdk-starter-agent

# Terminal 2 — admin console
pnpm --filter awaken-admin-console dev
# → http://127.0.0.1:3002
```

首次打开控制台时，右上角 topbar pill 会显示 **Token missing**。点击它，粘贴 token 并保存；连通后会显示 **Connected**。

## 浏览工作区

左侧 sidebar 按意图分组：

| 分组 | 内容 |
|---|---|
| **Configure** | Agents、Models、Providers、MCP Servers：可编辑的 runtime catalog |
| **Observe** | Dashboard、Audit Log、Eval Reports、Skill Registry：runtime 状态只读视图 |
| **Assistant** | AI Assistant：基于 live config 运行真实 agent 的聊天界面 |

使用 topbar breadcrumb 确认当前位置并返回上级页面。

## 检查系统状态

打开 **Dashboard**：

- **Stat cards**：agents、skills、models、providers、MCP servers、tools 的计数，来自 `/v1/capabilities`。
- **Health**：provider 是否配置 key，MCP server 是自动重启还是手动。
- **Recent activity**：audit log 启用时显示最近 8 条事件；未启用时显示黄色提示。
- **System**：server version、uptime，以及 config store / audit log / runtime stats 是否接入。

## 编辑 Agent

1. 在 sidebar 点击 **Agents**。
2. 使用 filter chips 按 model、plugin 或 modified range 缩小范围。Observability registry 接入时，Inferences 列显示真实调用统计。
3. 点击一行进入编辑器。
4. 编辑器包含 Basics、Tools、Plugins、Delegates、Advanced、History tabs。
5. 修改后底部 save bar 会出现：
   - **Validate** 调用 `POST /v1/config/agents/validate`，只校验不保存。
   - **Save** / **Save & Publish** 持久化并 apply；下一次请求使用新 spec。
6. 右侧 preview chat 调用 `POST /v1/ai-sdk/agent-previews/runs`，可以在保存前运行草稿。

## 测试 Provider

Providers 列表每行有 **Test** 按钮：

1. 点击 provider id 旁的 **Test**。
2. 控制台调用 `POST /v1/providers/:id/test`。
3. toast 显示 `OK · <latency>ms` 或后端错误文本。

发布新的 model config 前，先用它确认凭据、adapter 和上游可达性。

## 重启 MCP server

1. 打开 **MCP Servers**，进入已有 server 的编辑页。
2. 查看 **Live Status**：连接状态、handshake、tool count，以及 restart policy 或失败次数。
3. `last attempt` / `last success` 显示 manager 是否仍在重试。
4. 点击 **Restart** 调用 `POST /v1/mcp-servers/:id/restart`；成功时写入 `restart` audit event。

## 恢复历史版本

Audit log 也是版本历史：

1. 打开任意 resource editor（agent / model / provider / MCP server）。
2. 切到 **History** tab。
3. 展开事件查看 before/after diff。
4. 点击 **Restore this version**。
5. 确认后，控制台调用 `POST /v1/config/:ns/:id/restore`，server 会走正常 validate + apply pipeline，并写入新的 `restore` audit event。

## 浏览 Audit Log

打开 **Audit Log** 查看所有 resource 的事件：

- `since` / `until`：时间范围。
- `action`：create / update / delete / restart / publish / restore。
- `resource`：对子串匹配 `<namespace>/<id>`。
- `actor`：每行显示的 SHA-256 prefix。

点击行可打开 side panel，查看完整 event JSON、before/after diff，以及适用时的 restore 按钮。

## 启用可选子系统

控制台会如实降级；启用以下子系统后体验更完整。

### Audit log

在 `AppState` 上接入 audit logger：

```rust
use awaken_server::app::AuditLogConfig;

let state = state
    .with_audit_log_config(AuditLogConfig {
        retention_days: 90,
        ..AuditLogConfig::default()
    })
    .with_audit_log_from_config();
```

未启用时：Dashboard Recent activity 显示禁用提示，Audit Log 页面只有过滤表单，History tab 为空。

### Runtime stats

接入 observability plugin 和 `RuntimeStatsRegistry`：

```rust
use awaken_ext_observability::{ObservabilityPlugin, RuntimeStatsRegistry};

let registry = Arc::new(RuntimeStatsRegistry::new());
let observability = ObservabilityPlugin::new()
    .with_sink(SharedRegistrySink(Arc::clone(&registry)));

let state = AppState::new(/* ... */)
    .with_runtime_stats(registry);

let runtime = AgentRuntimeBuilder::default()
    .with_plugin("observability", Arc::new(observability))
    .build();
```

未启用时：Agents list 显示 banner，Inferences 列为 `n/a`，per-agent latency histogram 不渲染。

完整接入方式见[启用可观测性](/awaken/zh-cn/how-to/enable-observability/)。

## 控制台不覆盖的范围

控制台聚焦**配置**：agents、models、providers、MCP servers、tools、只读 skills、audit log 和 runtime stats。Live execution 相关表面当前走 REST：

- Threads & runs：列出、创建、取消、查看 messages。
- HITL decisions：提交挂起工具调用的 resume/cancel。
- Mailbox：查看或推送 inter-agent dispatch。
- Skill CRUD：控制台列出 skills，但不编辑。
- Config diagnostics：`GET /v1/config/diagnostics` 返回 registry-wide validation report。

请使用相同 admin bearer token 通过 `curl` 或脚本调用。端点清单见[管理控制台参考中的 REST-only 表](/awaken/zh-cn/reference/admin-console/#rest-only-功能)，请求格式见 [HTTP API](/awaken/zh-cn/reference/http-api/)。

## 深色模式

给 `<html>`（或任意子树）加 `data-theme="dark"`，`--aw-*` tokens 会自动切换。控制台 chrome 目前没有内置切换按钮；`tokens-auto-dark.css` 支持 `prefers-color-scheme: dark`。

## 排查

| 现象 | 可能原因 | 修复 |
|---|---|---|
| Topbar pill 显示 **Token missing** 或 **Token rejected** | bearer token 缺失或错误 | 点击 pill，粘贴 server 配置的 token |
| Topbar pill 显示 **Backend unreachable** | server 未监听或 URL 错误 | 确认 server 正在 `BACKEND_URL` 上运行；默认 `http://127.0.0.1:38080`，构建时可用 `VITE_BACKEND_URL` 覆盖 |
| 页面出现 `503` 但仍可加载 | audit / runtime stats 等可选子系统未启用 | 见[启用可选子系统](#启用可选子系统) |
| Save 失败并提示 “config management API not enabled” | server 没接入 `ConfigStore` | 嵌入方需要调用 `AppState::with_config_store(...)` |
| Provider Test 一直返回 “unsupported adapter” | provider 使用 `scripted` adapter | 符合预期；只有真实 adapter 才有有意义的 test path |
| Sidebar nav health dot 一直 neutral | health badge 只来自 list payload，不会每页都完整 probe | 打开资源详情查看 live `/status` |

## 相关

- [管理控制台参考](/awaken/zh-cn/reference/admin-console/)
- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [启用可观测性](/awaken/zh-cn/how-to/enable-observability/)
