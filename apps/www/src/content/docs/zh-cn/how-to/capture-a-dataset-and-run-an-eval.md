---
title: "采集数据集并运行评测"
description: "用管理控制台把已观察到的 trace 变成 dataset fixtures，运行 eval，并在接受调优变更前检查 pass/fail。"
---

当 prompt、model、tool、permission 或 stop-policy 变更需要每次都用同一批样本检查时，使用 eval。浏览器流程是：观察 run，把有价值 trace 保存为 fixture，运行 dataset，再检查结果。

## 你要点击什么

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-datasets.png">
      <img src="/awaken/assets/admin-console/flow-datasets.png" alt="Datasets 界面，包含 dataset list、fixture counts 和 create actions。" loading="lazy" />
    </a>
    <figcaption>Datasets：把 trace fixtures 归组成可回放套件。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run 详情页，包含 pass rate、failures 和逐 fixture report。" loading="lazy" />
    </a>
    <figcaption>Eval run detail：逐个 fixture 检查 pass/fail output。</figcaption>
  </figure>
</div>

## 1. 产生一条 trace

1. 从你的 client 或 Agent editor preview 运行 Agent。
2. 打开已保存 Agent。
3. trace routes 启用后，用 **Recent runs** 查看 trace。
4. 选择一条能代表你要保留或对比行为的 run。

如果 trace drawer 不可用，先接入 trace storage。见 [启用可观测性](/awaken/zh-cn/how-to/enable-observability/)。

## 2. 创建或选择 dataset

1. 打开 **Observe → Datasets**。
2. 新行为套件点击 **New Dataset**；回归集合则打开已有 dataset。
3. 使用描述行为的稳定 id，例如 `research-citations` 或 `tool-permission`。

## 3. 从 trace 添加 fixtures

1. 在 trace drawer 中点击 **Save as fixture**。
2. 选择 dataset。
3. 给 fixture 填写可读 id 和 description。
4. 如果 run 必须不消耗模型 token 就能回放，保持 provider-script capture required。
5. 保存，并确认 dataset fixture count 已变化。

## 4. 运行 eval

1. 打开 dataset。
2. 点击 **Run eval**。
3. 选择要测试的 Agent 和 model context。
4. 启动 run。
5. 打开生成的 **Eval Run**。

fixtures 含 provider scripts 时使用 scripted mode。只有当你明确要调用配置好的模型 provider 时，才使用 live mode。

## 5. 阅读结果

检查：

- pass rate 和 failure count；
- 每个 fixture 的 final answer；
- expectation/check failures；
- 如果上传或选择了 baseline report，检查 baseline differences。

失败 fixture 是调优输入：回到 Agent editor，一次改一个字段，validate、preview、save，然后重跑同一个 eval。

## 控制台调用的端点

端点细节放在 reference 中。自动化请看：

- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [管理控制台界面清单](/awaken/zh-cn/reference/admin-console/)
- [启用可观测性](/awaken/zh-cn/how-to/enable-observability/)
