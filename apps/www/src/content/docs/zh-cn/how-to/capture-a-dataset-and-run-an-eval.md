---
title: "采集数据集并运行评测"
description: "把真实 trace 变成回归数据集,针对某个 agent 在真实或脚本模型上运行,读取逐样本通过/失败与基线对比 —— 全程在管理控制台完成。"
---

Awaken 的评测是一个循环:把真实运行采集为**数据集(dataset)**里的
**样本(fixture)**,针对某个 agent 与模型**运行**该数据集,再读取逐样本的
**报告(report)**。运行可以是 *scripted*(确定性重放已记录的 provider 事件 —— 快、
不耗 token)或 *live*(用真实模型重新执行)。本页在浏览器里走完整个循环。

## 前置条件

- 一个接入了 `ConfigStore`、并启用了 **trace 路由**(`AWAKEN_EXPOSE_TRACE_ROUTES=true`)
  的 `awaken-server`,这样运行才会被采集。
- 一个可被你触发的 agent;若要 live 运行,还需一个 provider-backed 模型。

## 1. 产生一条 trace

先让 agent 跑一次,才有可采集的内容。打开 agent 编辑器,在 **Sandbox**(草稿预览
聊天)里发一条消息,或通过 API 驱动它。开启 trace 路由后,这次运行会记录进 trace
存储。

## 2. 创建数据集

1. 打开 **Datasets**(侧边栏 → **Observe**),点击 **+ New Dataset**。
2. 填入 **Dataset ID**(字母数字、`-`、`_`;≤64 字符)和可选 **Description**,点击
   **Create**。这会调用 `POST /v1/eval/datasets`,fixtures 初始为空。

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/datasets.png">
    <img src="/awaken/assets/admin-console/datasets.png" alt="Observe 下的 Datasets 界面,带「+ New Dataset」按钮,空状态说明数据集把 trace fixture 归组成可重放的 eval 套件。" loading="lazy" />
  </a>
  <figcaption>数据集把 trace fixture 归组成可重放的 eval 套件。</figcaption>
</figure>

## 3. 从 trace 添加样本

一个 fixture = 一次记录的运行 + 一条**期望(expectation)**。

1. 打开一条最近的运行/trace,选择 **Save trace as fixture**。
2. 选择目标数据集,给 fixture 起 id/描述,并设置期望 —— 通常是最终回答里的
   **must include** / **must exclude** 子串(也可断言 `tool_sequence` 或最低 judge
   分数)。
3. 保存。控制台调用 `POST /v1/eval/datasets/:id/items`,带
   `{ from_run_id, expected, … }`。后端取出用户输入,并在你不跳过时把 provider 事件
   记成可重放的 `provider_script`。

以 `provider_script_mode: "skip"` 保存的 fixture 是**仅 live**(没有脚本重放,必须
针对真实模型运行)。

## 4. 运行评测

**Scripted(快、确定性):** 在数据集详情页点击 **Run**。每个 fixture 重放它记录的
`provider_script` —— 不耗 token,适合 CI。(当所有 fixture 都是 live-only 时禁用。)

**Live(真实模型):** 用一个模型矩阵发起运行:

```http
POST /v1/eval/runs
{ "dataset_id": "my-dataset", "mode": "live",
  "agent_id": "concierge", "models": ["default"] }
```

- `mode` —— `live` 或 `scripted`。省略时:有 `models` 推断为 `live`,否则
  `scripted`。
- `models` —— 要评测的模型 id;每个 `(fixture, model)` 组合是一个 cell。
  `["default"]` 让每个 fixture 在 `default` 上各跑一次。
- `agent_id` —— 作为 live 重放基底的已注册 agent(其 prompt、工具、采样参数)。可用
  `agent_overrides` 按次打补丁。
- 可选:`samples`(抖动采样)、`judge`(LLM 评审)、`baseline_run_id`(与历史运行
  对比)、`max_walltime_secs`、`max_total_tokens`。

响应是一个 `EvalRun`,其 `items[*].report.passed` 给出逐样本结论。

## 5. 查看结果

- **Eval Runs** 列出运行(`GET /v1/eval/runs?dataset_id=…`),含 mode、样本数与失败
  数。点开某次运行查看详情(`GET /v1/eval/runs/:id`):逐样本通过/失败、失败原因、
  token、耗时。
- **Eval Reports** 是离线查看器 —— 上传报告 NDJSON(及可选 baseline),查看通过/失败
  汇总、逐样本对比,以及 **Regressions** / **Newly fixed** 过滤。在运行时传
  `baseline_run_id` 可由服务端算出 diff。

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/eval-run.png">
    <img src="/awaken/assets/admin-console/eval-run.png" alt="一次 eval 运行详情:Mode live、通过率 100%、0 失败,以及逐样本报告显示 passed=true 与模型最终回答。" loading="lazy" />
  </a>
  <figcaption>eval 运行详情 —— 通过率、失败数与逐样本报告。</figcaption>
</figure>

## 控制台调用的端点

| 操作 | 端点 |
|---|---|
| 列出 / 创建数据集 | `GET` / `POST /v1/eval/datasets` |
| 获取 / 更新 / 删除 | `GET` / `PUT` / `DELETE /v1/eval/datasets/:id` |
| 从 trace 采集样本 | `POST /v1/eval/datasets/:id/items` |
| 发起运行 | `POST /v1/eval/runs` |
| 列出 / 获取运行 | `GET /v1/eval/runs`、`GET /v1/eval/runs/:id` |

## 相关

- [在线调优 Prompt](/awaken/zh-cn/how-to/hot-tune-prompts/)
- [启用可观测性](/awaken/zh-cn/how-to/enable-observability/)
- [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)
