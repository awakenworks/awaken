---
title: "在线调优 Prompt"
description: "在管理控制台中调优已保存 Agent 的 prompt：编辑 Basics、Validate、用草稿 Preview、Save，然后用 trace 或 eval 对比。"
---

当 Agent 已存在，只想改进指令而不重新构建 server 时，用这个流程。管理控制台是主要路径；
API 写入用于自动化，并且应该复用同样的 UI 验证思路。

## 你要点击什么

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-agent-basics.png">
    <img src="/awaken/assets/admin-console/flow-agent-basics.png" alt="Agent editor Basics tab，包含 model 选择、system prompt、保存控件和 preview chat。" loading="lazy" />
  </a>
  <figcaption>在 Basics 中编辑 system prompt，Validate 草稿，Preview，再 Save。</figcaption>
</figure>

1. 打开 **Agents**，选择目标 Agent。
2. 停留在 **Basics**。
3. 编辑 **System prompt**。一次只改一个小范围：角色、约束、回答结构、工具使用指导或拒答边界。
4. 点击 **Validate**。先修复字段错误，再预览。
5. 用代表性 prompt 在 preview chat 中验证。Preview 使用的是未保存草稿。
6. 草稿表现更好后点击 **Save**。
7. 需要对比或恢复时，打开 **History** 或 **Audit Log**。

## 对比什么

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-history.png">
      <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Agent editor 的 History tab，展示 system_prompt update 和 restore action。" loading="lazy" />
    </a>
    <figcaption>History 展示保存过的 prompt 变更和 restore action。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run 详情页，展示每个 fixture 的 pass/fail。" loading="lazy" />
    </a>
    <figcaption>Eval 结果展示 prompt 是否改善了你关心的行为。</figcaption>
  </figure>
</div>

前后使用同一个场景：

- Preview chat prompt 用于快速反馈。
- 需要看精确工具调用和模型输出时，查看保存的 trace。
- 行为必须可复现时，使用 dataset/eval run。

见 [采集数据集并运行评测](/awaken/zh-cn/how-to/capture-a-dataset-and-run-an-eval/)。

## 可以在线安全调什么

| Prompt 变更 | UI 位置 | 说明 |
|---|---|---|
| 角色和语气 | **Basics → System prompt** | 最适合先改，容易 preview。 |
| 输出格式 | **Basics → System prompt** | 格式重要时配合 eval checks。 |
| 工具使用指导 | **Basics → System prompt** 加 **Tools** | 如果文案不足以约束，就收窄 allowed tools。 |
| 安全边界 | **Basics → System prompt** 加 **Plugins → Permission** | 工具级约束用 permission rules。 |
| 模型选择 | **Basics → Model** | 重新 preview；不同模型会不同地解释同一 prompt。 |

保存后的变更只影响**新的 run**。正在运行的任务继续使用它已经解析到的 spec。

## 何时改用 API

当 prompt 变更来自 CI、迁移或内部工具时，用 config API。保持同样循环：validate、write、跑代表性任务、再对比。

相关参考：

- [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/)
- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [配置参考](/awaken/zh-cn/reference/config/#agentspec)
