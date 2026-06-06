---
title: "配置停止策略"
description: "在管理控制台中约束 Agent run：调整 rounds、timeout/token/error/tool-stop policies，Validate，Preview，然后 Save。"
---

Stop policy 用来防止 Agent 运行得比预期更久。优先在 Agent editor 中配置；只有当同一变更需要自动化时，再直接写 API policy object。

## 你要点击什么

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-agent-basics.png">
    <img src="/awaken/assets/admin-console/flow-agent-basics.png" alt="Agent editor 的 Basics tab，展示 max rounds、max continuation retries、reasoning effort、system prompt、保存控件和 preview chat。" loading="lazy" />
  </a>
  <figcaption>先在 Basics 中调 max rounds 和 retries；只有需要审查 raw policy 时再打开 Advanced。</figcaption>
</figure>

1. 打开 **Agents**，选择目标 Agent。
2. 在 **Basics** 里先设置最常见边界：**max rounds**。
3. 如果需要更严格条件，打开 **Advanced**，审查 server 暴露的 stop policy 形态。
4. 点击 **Validate**。
5. Preview 两个场景：一个应正常完成，一个应触发停止。
6. 两个场景都符合预期后再 Save。
7. 对循环或时效敏感行为，运行 eval fixture。

## 选择哪种策略

| 风险 | UI 中选择 | 如何验证 |
|---|---|---|
| 重复同一个计划 | 降低 max rounds，或加 loop detection | Preview 一个过去会循环的 prompt |
| 工具链过长 | 加 timeout 或 tool-stop condition | Preview 一个工具密集任务 |
| Token 失控 | 加 token budget | 使用长 context/tool output 的 eval fixture |
| 连续错误 | 加 consecutive-error bound | 用失败工具或 provider 做 preview |
| 已知终止工具后停止 | 加 stop-on-tool condition | Preview 一个只应调用该工具一次的任务 |

## 保存后如何运营

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agents-list.png">
      <img src="/awaken/assets/admin-console/flow-agents-list.png" alt="Agents list，包含 runtime inference statistics。" loading="lazy" />
    </a>
    <figcaption>收紧限制后，观察 run 和 latency signals。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-history.png">
      <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Audit Log 界面，包含 update 和 restore events。" loading="lazy" />
    </a>
    <figcaption>如果限制截断了有效任务，用 History 恢复。</figcaption>
  </figure>
</div>

更严格的 stop policy 可能隐藏有效的多步任务。如果用户反馈过早停止，恢复上一版，或提高边界并重新运行同一个 eval。

## 自动化 API 参考

- [配置参考：AgentSpec](/awaken/zh-cn/reference/config/#agentspec)
- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [取消](/awaken/zh-cn/reference/cancellation/)
