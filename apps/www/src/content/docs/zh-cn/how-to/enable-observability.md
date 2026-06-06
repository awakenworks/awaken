---
title: "启用可观测性"
description: "在 server 接入可观测性 stores 后，通过管理控制台检查 dashboard health、runtime stats、recent runs、traces、datasets 和 eval results。"
---

可观测性分两部分：server wiring 和 operator review。本指南聚焦 server 暴露 runtime stats、trace、audit、eval stores 后，operator 在 UI 中应该看到什么、点击什么。

## 先点击什么

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-dashboard-health.png">
      <img src="/awaken/assets/admin-console/flow-dashboard-health.png" alt="Dashboard，展示 workload、health、recent activity 和 system metadata。" loading="lazy" />
    </a>
    <figcaption>Dashboard：确认可选 observability subsystems 已接入。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agents-list.png">
      <img src="/awaken/assets/admin-console/flow-agents-list.png" alt="Agents list，展示 model/plugin metadata 和 runtime inference statistics。" loading="lazy" />
    </a>
    <figcaption>Agents list：先看 runtime stats，再进入单个 Agent。</figcaption>
  </figure>
</div>

1. 打开 **Dashboard**。
2. 在 **Health** 和 **System** 中检查 audit log、runtime stats、trace、eval 是否可用。
3. 打开 **Agents**，查看 inference count、latency 和 error signals。
4. 打开已保存 Agent；trace routes 启用后，使用 **Recent runs**。
5. 把重要 trace 保存为 dataset fixture。
6. 运行 eval，在接受行为变更前查看 report。

## 每个界面告诉你什么

| 界面 | 用途 |
|---|---|
| **Dashboard** | Health、workload、recent audit activity 和 subsystem availability。 |
| **Agents list** | 接入 stats 后展示每个 Agent 的 inference count、errors、latency。 |
| **Recent runs / traces** | 单次 run 的 tool calls、model output、prompt variants 和 final status。 |
| **Datasets** | 把 trace 整理成可重复 fixtures。 |
| **Eval Runs** | Live 或 scripted replay 结果。 |
| **Eval Reports** | 离线 NDJSON report review 和 baseline comparison。 |

## 评测你观察到的行为

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-datasets.png">
      <img src="/awaken/assets/admin-console/flow-datasets.png" alt="Datasets 界面，包含 dataset list 和 fixture counts。" loading="lazy" />
    </a>
    <figcaption>Datasets：把 trace 归组成可回放 fixtures。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run 详情页，展示 pass/fail fixture output。" loading="lazy" />
    </a>
    <figcaption>Eval run detail：查看每个 fixture 的 pass/fail output。</figcaption>
  </figure>
</div>

当 dashboard 或 trace 观察引出了调优动作，就用 eval 验证。循环是：observe → tune 一个字段 → validate → preview → save → rerun 同一个 fixture。

## Server wiring 检查表

如果界面提示某个 subsystem unavailable，说明 UI 正常：server 还没有暴露对应 store 或 route。

| UI 中缺失 | Server 侧检查 |
|---|---|
| History 空或 Audit Log disabled | Audit log store/wiring |
| Agent stats 显示 `n/a` | Runtime stats registry/store |
| 没有 recent runs 或 trace drawer | Trace capture/store routes |
| Dataset/eval 页面 disabled | Eval dataset 和 run stores |
| 没有外部 telemetry spans | OTel exporter 和 collector configuration |

## 自动化 API 参考

- [管理控制台界面清单](/awaken/zh-cn/reference/admin-console/)
- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [事件](/awaken/zh-cn/reference/events/)
