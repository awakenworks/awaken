---
title: "优化 Context Window"
description: "在管理控制台中调 context limits 和 compaction：检查 Agent、调整 policy、Validate、Preview 长任务，然后 Save。"
---

当 runtime 已支持相关 policy 字段后，context 调优就是一个 operator 工作流。优先用管理控制台：
看到 Agent，做一个小改动，Validate，用长对话预览，再保存。

## 你要点击什么

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-agent-advanced.png">
    <img src="/awaken/assets/admin-console/flow-agent-advanced.png" alt="Agent editor 的 Advanced tab，展示 context window policy、compaction mode、autocompact threshold 和 preview chat。" loading="lazy" />
  </a>
  <figcaption>打开 Agent editor，审查草稿，再用 Advanced/JSON preview 修改 context policy。</figcaption>
</figure>

1. 打开 **Agents**，选择遇到 context limit 的 Agent。
2. 先看 **Basics** 中的 model 和 max rounds。换更大窗口模型或降低 max rounds，可能已经足够。
3. 打开 **Advanced**，审查最终草稿形态。
4. 调整该 server 暴露给 Agent 的 context policy 字段。
5. 点击 **Validate**。
6. 用长场景在 preview chat 中验证：多轮、工具结果、最终回答仍应记住早期事实。
7. 只有当 preview 保留了关键信息后再 Save。
8. 需要可复现验证时，把场景沉淀成 dataset 并运行 eval。

## 调什么

| 现象 | 在 UI 中尝试 | 用什么确认 |
|---|---|---|
| Agent 忘记最近用户意图 | 提高 recent-message retention，或减少噪音工具输出 | 同一个多轮 prompt 的 preview chat |
| Prompt 过大 | 启用或降低 auto-compaction threshold | 带长工具结果的 trace/eval |
| 最终回答被截断 | 提高 output-token limit，或换更大输出窗口模型 | Preview answer length 和 eval output |
| 长任务循环 | 搭配 stop policy limits | [配置停止策略](/awaken/zh-cn/how-to/configure-stop-policies/) |
| 重要工具结果消失 | 保留更大的 raw suffix，或只在安全点 summarization | Trace detail 和 eval fixtures |

## 观察结果

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agents-list.png">
      <img src="/awaken/assets/admin-console/flow-agents-list.png" alt="Agents list，包含 model metadata 和 runtime inference statistics。" loading="lazy" />
    </a>
    <figcaption>接入 stats 后，Agents list 会展示运行信号。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run 详情页，包含 pass/fail fixture output。" loading="lazy" />
    </a>
    <figcaption>Eval run 能发现长上下文行为的回归。</figcaption>
  </figure>
</div>

用 preview 获得快速反馈，再用 eval 覆盖不能回归的场景。如果改坏了，从 **History** 恢复上一版并保存。

## 自动化 API 参考

当 context policy 由部署工具生成时，用 API 写入。operator 循环不变：validate、write、run、compare。

- [配置参考：AgentSpec](/awaken/zh-cn/reference/config/#agentspec)
- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [管理控制台界面清单](/awaken/zh-cn/reference/admin-console/)
