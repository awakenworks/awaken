---
title: "Enable Tool Permission HITL"
description: "Use the Admin Console to add Ask/Allow/Deny rules for sensitive tools, preview the agent, save, and review decisions in traces/audit."
---

Tool permission HITL is a governance workflow: decide which tools may run
without review, which must ask a human, and which should never be exposed. Use
the Admin Console first so the policy is visible next to the agent draft.

## What you click

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-permissions.png">
      <img src="/awaken/assets/admin-console/flow-agent-permissions.png" alt="Agent editor Plugins tab with Permissions, Reminders, deferred_tools, and a Permission rules config card." loading="lazy" />
    </a>
    <figcaption>Agent editor: enable the permission plugin and edit rules for the draft.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-tools.png">
      <img src="/awaken/assets/admin-console/flow-agent-tools.png" alt="Agent editor Tools tab showing allowed web_search, read_document, filesystem/read_file, summarize, and excluded delete_file." loading="lazy" />
    </a>
    <figcaption>Agent Tools tab: confirm which tools are exposed before adding permission rules.</figcaption>
  </figure>
</div>

1. Open **Tools** and identify the sensitive tool ids.
2. Open **Agents** and choose the agent.
3. On **Tools**, expose only the tools this agent should know about.
4. On **Plugins**, enable the permission plugin.
5. Add rules:
   - **Allow** for safe read-only tools.
   - **Ask** for tools that can spend money, write files, call external systems,
     or reveal sensitive data.
   - **Deny** for tools this agent must never call.
6. Click **Validate**.
7. Preview one prompt that should run without asking and one that should suspend
   for review.
8. Save when both paths are correct.

## Rule checklist

| Question | UI check |
|---|---|
| Does the agent need to see this tool at all? | **Tools** tab allow/exclude selection |
| Should the tool be available but reviewed? | **Plugins → Permission → Ask** |
| Should the tool disappear from the effective catalog? | **Plugins → Permission → Deny** or exclude it in **Tools** |
| Are argument-sensitive rules needed? | Add a rule that matches the tool arguments, then preview that exact case |
| Did the policy change after save? | **History** and **Audit Log** |

## Verify and operate

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-agent-history.png">
    <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Agent editor History tab with update/create events and Restore actions." loading="lazy" />
  </a>
  <figcaption>Use History to review permission-rule changes and restore prior policy.</figcaption>
</figure>

After saving, run a real task or eval fixture that tries the sensitive tool. A
correct Ask rule should suspend for human review in your client/HITL surface. If
the model can still call a tool that should be blocked, tighten the **Tools**
allowlist and the permission rule together.

## API references for automation

- [Config reference: AgentSpec sections](/awaken/reference/config/#agentspec)
- [HTTP API](/awaken/reference/http-api/)
- [HITL and Mailbox](/awaken/explanation/hitl-and-mailbox/)
