---
title: "启用工具权限 HITL"
description: "在管理控制台中为敏感工具添加 Ask/Allow/Deny 规则，Preview Agent，Save，并在 trace/audit 中审查。"
---

工具权限 HITL 是治理工作流：决定哪些工具可直接运行，哪些必须询问人工，哪些不应暴露。
优先使用管理控制台，因为 policy 会和 Agent 草稿放在一起审查。

## 你要点击什么

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-permissions.png">
      <img src="/awaken/assets/admin-console/flow-agent-permissions.png" alt="Agent editor 的 Plugins tab，包含 Permissions、Reminders、deferred_tools 和 Permission rules config card。" loading="lazy" />
    </a>
    <figcaption>Agent editor：启用 permission plugin，并为草稿编辑 rules。</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-tools.png">
      <img src="/awaken/assets/admin-console/flow-agent-tools.png" alt="Agent editor 的 Tools tab，展示 allowed web_search、read_document、filesystem/read_file、summarize 和 excluded delete_file。" loading="lazy" />
    </a>
    <figcaption>Agent Tools tab：添加 permission rules 前，确认哪些工具会暴露给模型。</figcaption>
  </figure>
</div>

1. 打开 **Tools**，确认敏感工具的 id。
2. 打开 **Agents**，选择目标 Agent。
3. 在 **Tools** tab，只暴露这个 Agent 应该知道的工具。
4. 在 **Plugins** tab，启用 permission plugin。
5. 添加规则：
   - 安全只读工具用 **Allow**。
   - 会花钱、写文件、调用外部系统或暴露敏感数据的工具用 **Ask**。
   - 该 Agent 永远不能调用的工具用 **Deny**。
6. 点击 **Validate**。
7. Preview 两个 prompt：一个应直接运行，一个应暂停等待审核。
8. 两条路径都正确后 Save。

## 规则检查表

| 问题 | UI 检查 |
|---|---|
| Agent 是否需要看到这个工具？ | **Tools** tab 的 allow/exclude selection |
| 工具可用但需要审核？ | **Plugins → Permission → Ask** |
| 工具应从有效目录中消失？ | **Plugins → Permission → Deny**，或在 **Tools** 中 exclude |
| 是否需要按参数匹配？ | 添加匹配工具参数的 rule，并 preview 完全相同的 case |
| 保存后 policy 是否变化？ | **History** 和 **Audit Log** |

## 验证和运营

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-agent-history.png">
    <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Agent editor 的 History tab，包含 update/create events 和 Restore actions。" loading="lazy" />
  </a>
  <figcaption>使用 History 审查 permission-rule 变更，并恢复旧 policy。</figcaption>
</figure>

保存后，运行一个真实任务或 eval fixture，让它尝试敏感工具。正确的 Ask 规则应在你的
client/HITL surface 中暂停等待人工。如果模型仍能调用应被阻止的工具，同时收紧 **Tools** allowlist 和 permission rule。

## 自动化 API 参考

- [配置参考：AgentSpec sections](/awaken/zh-cn/reference/config/#agentspec)
- [HTTP API](/awaken/zh-cn/reference/http-api/)
- [HITL 与 Mailbox](/awaken/zh-cn/explanation/hitl-and-mailbox/)
