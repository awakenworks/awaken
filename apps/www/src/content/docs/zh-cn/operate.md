---
title: "调优与运营"
description: "优先通过管理控制台运营 Awaken：调 Agent、审权限、查 trace、跑 eval；API 只作为自动化参考。"
---

本节面向在浏览器里操作运行中 Awaken server 的人。把**管理控制台**作为主要入口：
在 UI 中点击操作，一次只改一个点，Validate，Preview，Save，然后对比行为。只有当
同一操作需要自动化时，再使用 REST/config API。

## 从控制台开始

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-dashboard-health.png">
    <img src="/awaken/assets/admin-console/flow-dashboard-health.png" alt="Admin Console dashboard，展示 workload、health、recent audit events 和 system metadata。" loading="lazy" />
  </a>
  <figcaption>先看 Dashboard：确认 server 已连接、健康，并处在预期 scope。</figcaption>
</figure>

1. 打开 [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/) 并连接 server。
2. 修改前先检查 **Dashboard → Health**。
3. 在 **Infrastructure** 下配置 **Providers** 和 **Models**。
4. 打开 **Agents**，编辑草稿，点击 **Validate**，用 preview chat 验证，然后 **Save**。
5. 需要审查或恢复变更时，用 **Audit Log** 和编辑器里的 **History**。

## 通过交互调优行为

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-basics.png">
      <img src="/awaken/assets/admin-console/flow-agent-basics.png" alt="Agent editor，包含 Basics、Tools、Plugins、Delegates、Advanced、History、保存控件和 preview chat。" loading="lazy" />
    </a>
    <figcaption>Agent editor：调 prompt、model、tools、plugins、delegates 和 limits。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-tools.png">
      <img src="/awaken/assets/admin-console/flow-agent-tools.png" alt="Agent editor 的 Tools tab，展示 allowed tool checkboxes、excluded delete_file、patterns 和 preview chat。" loading="lazy" />
    </a>
    <figcaption>Agent Tools tab：暴露 web_search、read_document、filesystem/read_file，同时排除 delete_file。</figcaption>
  </figure>
</div>

| 目标 | 点击路径 | 指南 |
|---|---|---|
| 调 prompt/model/tools/plugins/delegates | **Agents → Agent → Basics / Tools / Plugins / Delegates → Validate → Preview → Save** | [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/) |
| 安全改 prompt 文案 | **Agent → Basics → System prompt → Validate → Preview → Save** | [在线调优 Prompt](/awaken/zh-cn/how-to/hot-tune-prompts/) |
| 管理上下文和压缩 | **Agent → Advanced / JSON preview → Validate → Preview 长任务 → Save** | [优化 Context Window](/awaken/zh-cn/how-to/optimize-context-window/) |
| 约束循环和失控任务 | **Agent → Basics / Advanced → limits 和 stop policy → Validate → Preview → Save** | [配置停止策略](/awaken/zh-cn/how-to/configure-stop-policies/) |

## 通过交互做治理与评测

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-history.png">
      <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Agent editor 的 History tab，包含 update/create events 和 View/Restore actions。" loading="lazy" />
    </a>
    <figcaption>History tab：恢复或发布前，先审查已保存的 Agent 变更。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run 详情页，展示 pass rate 和每个 fixture 的结果。" loading="lazy" />
    </a>
    <figcaption>Eval detail：接受变更前，用可重放样本对比行为。</figcaption>
  </figure>
</div>

| 目标 | 点击路径 | 指南 |
|---|---|---|
| 给敏感工具加 gate | **Agent → Plugins → Permission rules → Ask / Allow / Deny → Save** | [启用工具权限 HITL](/awaken/zh-cn/how-to/enable-tool-permission-hitl/) |
| 接入可 handoff 的远程 Agent | **Resources → A2A Servers → New A2A server → Refresh card → Save** | [接入 A2A Server](/awaken/zh-cn/how-to/connect-an-a2a-server/) |
| 捕获并回放行为 | **Observe → Datasets → Eval Runs → Eval Reports** | [采集数据集并运行评测](/awaken/zh-cn/how-to/capture-a-dataset-and-run-an-eval/) |
| 查看运行信号 | **Dashboard → Agents list → Agent dashboard / Recent runs** | [启用可观测性](/awaken/zh-cn/how-to/enable-observability/) |

## 自动化时使用的 API 参考

同一操作需要脚本化，而不是在 UI 里点击时，使用这些参考：

- [HTTP API](/awaken/zh-cn/reference/http-api/) — 路由、认证、请求/响应形态。
- [配置](/awaken/zh-cn/reference/config/) — `AgentSpec`、provider/model config、plugin sections。
- [管理控制台界面清单](/awaken/zh-cn/reference/admin-console/) — screen 到 endpoint 的映射和 REST-only surface。
