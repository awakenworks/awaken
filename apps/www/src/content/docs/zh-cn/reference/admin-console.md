---
title: "管理控制台界面清单"
description: "Awaken 管理控制台 screens、widgets,以及每个界面调用的 server API 的技术清单。"
---

这是管理控制台界面的技术清单。操作者工作流请先阅读
[使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/);当你需要核对 screen 覆盖、
endpoint 映射时,再使用本页。

管理控制台是运行中 `awaken-server` 的浏览器控制面:配置 provider 和 model,编辑
prompt 与工具描述,分配 MCP 工具,调优 reminder 与 deferred-tool 策略,预览草稿,
然后发布下一版 registry snapshot。启动 server + 控制台见
[使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/#前置条件)。

## 截图

截图展示代表性的控制台状态。实际运行中的控制台会从你的后端 API 读取数据;如果
某个子系统没有接入,对应界面会显示 disabled / unavailable 提示。

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/01-dashboard.png">
      <img src="/awaken/assets/admin-console/01-dashboard.png" alt="管理控制台 Dashboard，展示 live workload、agent activity、最近审计事件、provider/MCP health 和当前 scope 元数据。" loading="lazy" />
    </a>
    <figcaption>Dashboard：实时负载、健康状态、审计事件和只读 scope。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/02-agent-editor.png">
      <img src="/awaken/assets/admin-console/02-agent-editor.png" alt="Agent 编辑器，包含模型选择、系统提示、tools、plugins、delegates、history、保存控制和 preview chat。" loading="lazy" />
    </a>
    <figcaption>Agent editor：prompt、tools、plugins、delegates、history 和草稿预览。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/03-agents-list.png">
      <img src="/awaken/assets/admin-console/03-agents-list.png" alt="Agents 列表，包含筛选器、model/plugin 元数据和 runtime inference 统计。" loading="lazy" />
    </a>
    <figcaption>Agents list：筛选、model/plugin 元数据和 runtime stats。</figcaption>
  </figure>
</div>

### Infrastructure 和 Resources

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/providers.png">
      <img src="/awaken/assets/admin-console/providers.png" alt="Providers 界面,列出 Anthropic、Vertex 和本地 Ollama providers,包含 adapter、base URL、API key 状态以及 test/edit/delete 操作。" loading="lazy" />
    </a>
    <figcaption>Providers：上游 adapter、凭据状态和连接测试入口。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/models.png">
      <img src="/awaken/assets/admin-console/models.png" alt="Models 界面,列出稳定 model id、provider id、upstream model、modalities、context window 和 actions。" loading="lazy" />
    </a>
    <figcaption>Models：把稳定 runtime id 映射到 provider-backed upstream model。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/mcp-servers.png">
      <img src="/awaken/assets/admin-console/mcp-servers.png" alt="MCP Servers 界面,列出 filesystem 和 Linear servers,包含 transport、live status、restart policy、tool count 和 actions。" loading="lazy" />
    </a>
    <figcaption>MCP Servers：transport config、live status 和 restart actions。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/mcp-server-detail.png">
      <img src="/awaken/assets/admin-console/mcp-server-detail.png" alt="MCP server 详情页,展示 filesystem live status、restart 按钮、command、暴露的 tools、prompts 和 resources。" loading="lazy" />
    </a>
    <figcaption>MCP detail：handshake、restart 和暴露的 inventory。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/tools.png">
      <img src="/awaken/assets/admin-console/tools.png" alt="Tools 界面,列出 built-in 和 MCP tools,包含 source badges、description 和 edit actions。" loading="lazy" />
    </a>
    <figcaption>Tools：已发现的工具目录和模型可见描述。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/skills.png">
      <img src="/awaken/assets/admin-console/skills.png" alt="Skills 界面,列出 reusable skill instructions,包含 invocation mode、context mode、allowed tools 和 source paths。" loading="lazy" />
    </a>
    <figcaption>Skills：可复用指令和允许的 tool context。</figcaption>
  </figure>
</div>

### Observe 和 Evaluate

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/audit-log.png">
      <img src="/awaken/assets/admin-console/audit-log.png" alt="Audit Log 界面,展示最近的 update、publish、restart 和 create events,包含 actor、resource、timestamp 和 change summary。" loading="lazy" />
    </a>
    <figcaption>Audit Log：配置历史、restore 上下文和操作者归因。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/datasets.png">
      <img src="/awaken/assets/admin-console/datasets.png" alt="Observe 下的 Datasets 界面,包含 dataset list、fixture counts 和 create/delete actions。" loading="lazy" />
    </a>
    <figcaption>Datasets：把 trace fixture 归组为可重放集合。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/eval-runs.png">
      <img src="/awaken/assets/admin-console/eval-runs.png" alt="Eval Runs 界面,列出已完成 eval jobs,包含 dataset、mode、status、fixture count 和 pass/fail summary。" loading="lazy" />
    </a>
    <figcaption>Eval Runs：dataset replay 的执行记录。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/eval-run.png">
      <img src="/awaken/assets/admin-console/eval-run.png" alt="Eval run 详情页,展示 pass rate、failure count 和每个 fixture 的 report row。" loading="lazy" />
    </a>
    <figcaption>Eval run detail：每个 fixture 的 pass/fail 输出。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/eval-reports.png">
      <img src="/awaken/assets/admin-console/eval-reports.png" alt="Eval Reports 界面,包含上传新 NDJSON report 和可选 baseline report 的卡片。" loading="lazy" />
    </a>
    <figcaption>Eval Reports：离线 NDJSON review 和 baseline comparison。</figcaption>
  </figure>
</div>

### Assistant 和 Shortcuts

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/admin-assistant.png">
      <img src="/awaken/assets/admin-console/admin-assistant.png" alt="Admin Assistant 面板,包含能力说明、建议 agent chips,以及描述 agent 或询问配置的输入框。" loading="lazy" />
    </a>
    <figcaption>Admin Assistant：引导式 agent 创建和 config 帮助。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/a2a-create.png">
      <img src="/awaken/assets/admin-console/a2a-create.png" alt="Create A2A server 表单,包含 server id、base URL、timeout、optional target、options JSON 和 bearer token 控件。" loading="lazy" />
    </a>
    <figcaption>A2A server setup：远程 agent-card discovery 配置。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/cmdk.png">
      <img src="/awaken/assets/admin-console/cmdk.png" alt="Command palette overlay,包含搜索框以及跳转 agents、providers、models、tools 和 observe screens 的快捷入口。" loading="lazy" />
    </a>
    <figcaption>Command palette：跨控制台界面的键盘导航。</figcaption>
  </figure>
</div>

## 界面与端点

每个界面都是 admin REST 路由之上的薄客户端(全部在 admin bearer token 之后)。
请求/响应格式见 [HTTP API](/awaken/zh-cn/reference/http-api/)。

| 界面 | 读 / 写 |
|---|---|
| Dashboard | `GET /v1/capabilities`、`/v1/system/info`、`/v1/audit-log`、`/v1/runs/summary`、runtime stats |
| Agents(列表 + 编辑器) | `GET/POST/PUT /v1/config/agents`、校验 `POST /v1/config/agents/validate`、草稿预览 `POST /v1/ai-sdk/agent-previews/runs`、恢复 `POST /v1/config/agents/:id/restore`、统计 `GET /v1/agents/:id/runtime-stats` |
| Providers | `GET/POST/PUT/DELETE /v1/config/providers`、测试 `POST /v1/providers/:id/test` |
| Models | `GET/POST/PUT/DELETE /v1/config/models` |
| MCP Servers | `…/config/mcp-servers`、重启 `POST /v1/mcp-servers/:id/restart`、inventory `GET /v1/mcp-servers/:id/inventory` |
| A2A Servers | `…/config/a2a-servers`、状态 `GET /v1/a2a-servers/:id/status` |
| Skills / Tools | `GET /v1/config/skills`(只读)、工具目录来自 `/v1/capabilities` |
| Admin Assistant | 运行 `POST /v1/admin/assistant/runs`、策略 `GET/PUT /v1/admin/assistant/config` |
| Audit Log | `GET /v1/audit-log` |
| Datasets / Eval Runs | `…/eval/datasets`(+ `/:id/items`)、`…/eval/runs`(+ `/:id`) |
| Eval Reports | 离线 NDJSON 上传(不调后端) |

provider→model→agent 的配置流程、编辑器各 tab,以及把已保存 Agent 接到前端,都在
how-to 里:[使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)、
[通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/)、
[AI SDK 前端集成](/awaken/zh-cn/how-to/integrate-ai-sdk-frontend/)。

Provider credentials 和 MCP credentials 是两条边界。Provider 服务模型执行;MCP
server credentials 属于对应 transport(stdio 的 `env`,HTTP 的 URL/config),Agent
对 MCP 的访问由工具选择和可选 permission rules 控制。Admin Assistant 只有在配置第一个
provider-backed model 后才解锁;它的工具由 server 锁定,不出现在普通 tool registry。

## 运维、Trace 与 Eval

- **Dashboard** 展示实时负载、provider/MCP health、最近审计事件、可选 runtime stats 和只读 `scope_id`。
- 已保存 Agent 的 **Recent runs** 会在 trace routes 启用时打开持久 trace。
- **Datasets** 从 trace 捕获 eval fixture。
- **Eval Runs** 对配置好的 agents/models 运行 dataset。
- **Eval Reports** 在浏览器中查看 NDJSON report 和 baseline diff。

Trace 和 eval payload 可能包含 prompt、tool arguments 和模型回复。请保护 admin bearer
token 和相关路由访问范围。

## 版本历史与 Pinning

每次配置保存都会记录 metadata;启用 audit log 后,也会出现在 Audit Log 中。Agent
History 可以查看 diff,并把历史 snapshot 恢复到 editing store。

Restore 是审查步骤:恢复后,如果要让该 payload 对新 run 生效,需要再次 Save/Publish。
当 server 挂接 versioned registry store 后,已发布 runtime registry snapshot 是不可变的;
durable run 会携带 `resolution_id`,让 resume/replay 重新选择同一个 graph。

## REST-only 功能

控制台聚焦于**配置**。有些面今天有意只走 REST —— 用相同的 admin bearer token 通过 `curl`
或脚本驱动(请求格式见 [HTTP API](/awaken/zh-cn/reference/http-api/)):

| 面 | 内容 | 端点 |
|---|---|---|
| Threads & runs | 列出 / 创建 / 取消 / 查看消息 | `/v1/threads`、`/v1/runs` |
| HITL 决策 | resume / cancel 挂起的工具调用 | `POST /v1/runs/:id/decision` |
| Mailbox | peek / push 跨 agent 派发 | mailbox 路由 |
| Skill CRUD | 控制台列出 skill 但不编辑 | `/v1/config/skills` |
| 配置诊断 | 全 registry 校验报告(暂无界面渲染) | `GET /v1/config/diagnostics` |

## Scope

`scope_id` 在控制台里只读展示。浏览器不直接选择 scope;server 会通过可信的
`HttpScopeProvider` 为每个请求解析 scope。托管版或多 workspace 产品应在
auth/provider 层切换 tenant/workspace,然后在控制台显示解析后的值。

## 相关

- [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/) - 操作 walkthrough
- [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/) - 完整调优面
- [HTTP API](/awaken/zh-cn/reference/http-api/) - 请求与响应参考
