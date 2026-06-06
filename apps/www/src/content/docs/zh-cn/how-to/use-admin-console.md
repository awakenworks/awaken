---
title: "使用管理控制台"
description: "从浏览器运营 Awaken：连接 server、调优 Agent、校验草稿、检查 trace、运行 eval，并恢复历史版本。"
---

Admin Console 是 Awaken server 的主要调优与运营 UI。Runtime 能力在代码里实现后，
可以在浏览器里配置 provider/model、创建 Agent、调 prompt 和工具描述、分配 tools / skills / delegates、预览草稿、检查 trace、采集 dataset、运行 eval，并审查 audit history。

本指南重点写**界面怎么操作**。Endpoint 形态和 screen 到 route 的映射，请看
[HTTP API](/awaken/zh-cn/reference/http-api/)、[配置](/awaken/zh-cn/reference/config/) 和
[管理控制台界面清单](/awaken/zh-cn/reference/admin-console/)。

## 前置条件

- 一个运行中的 `awaken-server`。Starter 默认 URL 是 `http://127.0.0.1:38080`。
- Server 上配置好的 admin bearer token。
- 本地运行的 Admin Console dev server，或部署环境提供的生产构建。

```sh
# Terminal 1 — runtime
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
cargo run -p ai-sdk-starter-agent

# Terminal 2 — admin console
pnpm --filter awaken-admin-console dev
# → http://127.0.0.1:3002
```

打开控制台，点击右上角 token pill，填入 `dev-token` 并保存。状态会从
**Token missing** 变成 **Connected**。

## 界面概览

<figure class="screenshot">
  <a href="/awaken/assets/awaken-demo-zh.gif">
    <img src="/awaken/assets/awaken-demo-zh.gif" alt="动画演示：接入 Vertex 上的 Gemini、手动构建 agent、运行实时评测。" loading="lazy" />
  </a>
  <figcaption>完整流程录屏 —— 录制于真实 Gemini 后端。</figcaption>
</figure>

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/01-dashboard.png">
      <img src="/awaken/assets/admin-console/01-dashboard.png" alt="管理控制台 dashboard，展示 live workload、agent activity、recent audit events、health status 和 scope metadata。" loading="lazy" />
    </a>
    <figcaption>Dashboard：workload、health、system metadata 和 recent activity。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/02-agent-editor.png">
      <img src="/awaken/assets/admin-console/02-agent-editor.png" alt="Agent editor，包含 model 选择、system prompt 字段、tabs、save controls 和 draft preview chat。" loading="lazy" />
    </a>
    <figcaption>Agent editor：调优、校验、预览、保存。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/03-agents-list.png">
      <img src="/awaken/assets/admin-console/03-agents-list.png" alt="Agents list，包含 filters、model/plugin metadata 和 runtime inference statistics。" loading="lazy" />
    </a>
    <figcaption>Agents list：按 model/plugin 过滤，并查看 runtime signals。</figcaption>
  </figure>
</div>

## 浏览工作区

左侧 sidebar 按 operator 意图分组：

| Group | 内容 |
|---|---|
| **Agents** | Agent 列表、Agent editor、per-agent dashboard。 |
| **Infrastructure** | Providers 和 Models。Live run 前先配置上游访问。 |
| **Resources** | MCP Servers、A2A Servers、Skills、Tools。 |
| **Observe** | Dashboard、Audit Log、Datasets、Eval Runs、Eval Reports。 |

用 topbar breadcrumb 返回父页面。Admin Assistant 是浮动气泡，不是 sidebar 目的地；配置至少一个 provider-backed model 后才真正有用。

## 连接并检查系统

1. 打开控制台并填入 bearer token。
2. 从 **Dashboard** 开始。
3. 查看 **Health**：provider 是否缺 key，MCP server 是否失败。
4. 查看 **System**：version、scope、uptime，以及接入了哪些可选 subsystem。
5. 点击 stat card 跳转到 Agents、Models、Providers、Skills、MCP Servers 或 Tools。

如果 **Recent activity** 提示 audit log disabled，server 仍可运行，但 History tab 和 restore 工作流会为空，直到接入 audit logging。

## 创建 Provider 和 Model

1. 打开 **Infrastructure → Providers**。
2. 创建 provider，填写 adapter、base URL 和凭据。
3. 点击 provider 行上的 **Test**。Toast 会显示成功、延迟或上游错误。
4. 打开 **Infrastructure → Models**。
5. 创建一个指向该 provider 的 model id。Agent 引用的是稳定 model id，而不是原始 provider 凭据。

相关 API：[Provider/Model 配置](/awaken/zh-cn/reference/provider-model-config/) 和
[HTTP API](/awaken/zh-cn/reference/http-api/)。

## 编辑 Agent

新建 Agent：点击 **Agents → + New Agent**。编辑已有 Agent：点击 Agents list 中的一行。

1. **Basics** — 设置 model、max rounds、reasoning effort 和 system prompt。
2. **Tools** — 选择全部工具，或自定义 allow/exclude。可用 source filter 缩小到 built-in、plugin、MCP tools。
3. **Plugins** — 启用或关闭 plugin-backed behavior。
4. **Delegates** — 选择这个 Agent 可以 hand off 给哪些其他 Agent。
5. **Advanced** — 检查最终 raw JSON。
6. **History** — audit logging 启用后，可查看历史变更并恢复旧版本。

编辑后，底部 save bar 会出现：

- **Validate** 只校验草稿，不保存、不应用。
- **Save** / **Save & Publish** 保存草稿，并让新 run 可以使用。

保存前优先使用右侧 preview chat。它基于未保存草稿运行，方便在发布前调 prompt、tools 和 model 选择。

相关 API：[HTTP API](/awaken/zh-cn/reference/http-api/) 中的 `agents` config routes，
以及 [配置](/awaken/zh-cn/reference/config/#agentspec) 中的 `AgentSpec`。

## 安全调优行为

一次安全调优流程：

1. 一次只改一个行为维度：prompt、model、tools、plugin config、permissions、delegates 或 stop policy。
2. 点击 **Validate**。
3. 用你关心的场景在 preview chat 里验证。
4. Preview 符合预期后再保存。
5. 跑真实任务或 eval fixture，确认 draft preview 之外也符合预期。
6. 如果结果退化，打开 **History** 恢复上一版。

完整 tab-by-tab 调优地图见 [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/)。

## 管理资源

### 重启 MCP server

1. 打开 **Resources → MCP Servers**。
2. 点击一个 server，并滚动到 **Live Status**。
3. 查看 connection state、handshake result、tool count 和 retry/failure summary。
4. 点击 **Restart**。重启进行中按钮会被禁用。

### 接入 A2A server

打开 **Resources → A2A Servers**，点击 **New A2A server**，填入远程 server base URL。Awaken 会发现远程 agent card，并让它们可被 delegation 使用。见 [接入 A2A Server](/awaken/zh-cn/how-to/connect-an-a2a-server/)。

### 查看 Skills 和 Tools

用 **Skills** 和 **Tools** 查看当前 server 发现了什么。Agent 是否能使用它们，仍由 Agent editor 的 Tools、Plugins 和 Delegates tabs 控制。

## 观测 run 并对比行为

有真实流量或 preview run 后，使用 **Observe** 页面：

- **Dashboard** — workload、health 和 recent activity。
- **Audit Log** — 全局 create/update/delete/restart/restore 历史。
- **Datasets** — 可回放 fixture。
- **Eval Runs** — eval job 的执行记录。
- **Eval Reports** — pass/fail 和 baseline diff。

完整评测流程见 [采集数据集并运行评测](/awaken/zh-cn/how-to/capture-a-dataset-and-run-an-eval/)。

## 恢复历史版本

Awaken 的 audit log 也是版本历史。

1. 打开 agent、model、provider 或 MCP server editor。
2. 切换到 **History**。
3. 展开 event，查看 before/after diff。
4. 点击 **Restore this version**。
5. 检查 diff 并确认。
6. 当你准备让新 run 使用恢复后的内容时，Validate 并 Save。

相关 API：[HTTP API](/awaken/zh-cn/reference/http-api/) 中的 restore routes，
以及 [管理控制台界面清单](/awaken/zh-cn/reference/admin-console/) 中的 audit 行为。

## 启用可选子系统

可选 server module 缺失时，控制台会明确降级：

| 缺失项 | 你会看到 | 启用方式 |
|---|---|---|
| Audit log | Dashboard disabled notice、空 Audit Log、空 History tabs | [配置参考](/awaken/zh-cn/reference/config/#auditlogconfig) 和 server wiring |
| Runtime stats | Agents list 显示 `n/a`；per-agent latency charts 不可用 | [启用可观测性](/awaken/zh-cn/how-to/enable-observability/) |
| Trace/eval stores | Dataset/eval 页面无法持久化有效记录 | [采集数据集并运行评测](/awaken/zh-cn/how-to/capture-a-dataset-and-run-an-eval/) |

## 仍应由 API 自动化的内容

控制台聚焦配置和 operator review。以下 live execution 或底层控制面更适合 HTTP API 或你自己的工具：

- Threads、messages 和 run inspection。
- Programmatic run create、cancel、interrupt、resume。
- 自定义 UI 的 HITL decisions。
- Mailbox inspection 和 dispatch automation。
- Registry diagnostics 和 bulk config management。

见 [REST-only features matrix](/awaken/zh-cn/reference/admin-console/#rest-only-功能) 和
[HTTP API reference](/awaken/zh-cn/reference/http-api/)。

## 排查

| 现象 | 可能原因 | 修复 |
|---|---|---|
| Topbar 显示 **Token missing** 或 **Token rejected** | Bearer token 缺失或错误 | 点击 pill，填入 server 上配置的 token。 |
| Topbar 显示 **Backend unreachable** | Server 没启动或 URL 错误 | 确认 server 正在 `BACKEND_URL` 上运行；默认 `http://127.0.0.1:38080`。 |
| 页面能加载但显示 optional subsystem warnings | Audit/runtime stats/trace/eval stores 未接入 | 在 server 上启用对应 subsystem。 |
| Save 失败并提示 "config management API not enabled" | 没有接入 config store | 启动带 config management 的 server。 |
| Provider Test 报 unsupported adapter | Provider 是 scripted 或不可测试 | 对 scripted/demo provider 属于预期；生产前测试真实 adapter。 |

## 相关

- [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/)
- [用 Admin Assistant 构建 Agent](/awaken/zh-cn/how-to/build-an-agent-with-the-assistant/)
- [启用工具权限 HITL](/awaken/zh-cn/how-to/enable-tool-permission-hitl/)
- [采集数据集并运行评测](/awaken/zh-cn/how-to/capture-a-dataset-and-run-an-eval/)
- [管理控制台界面清单](/awaken/zh-cn/reference/admin-console/)
