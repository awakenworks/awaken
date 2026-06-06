---
title: "Configure Stop Policies"
description: "Bound agent runs from the Admin Console: adjust rounds, timeout/token/error/tool-stop policies, validate, preview, then save."
---

Stop policies keep an agent from running longer than intended. Configure them
from the agent editor first; use API-level policy objects only when you are
automating the same change.

## What you click

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-agent-basics.png">
    <img src="/awaken/assets/admin-console/flow-agent-basics.png" alt="Agent editor Basics tab with max rounds, max continuation retries, reasoning effort, system prompt, save controls, and preview chat." loading="lazy" />
  </a>
  <figcaption>Use Basics for max rounds and retries first; open Advanced only when you need raw policy review.</figcaption>
</figure>

1. Open **Agents** and choose the agent.
2. In **Basics**, set the common bound first: **max rounds**.
3. If the agent needs stricter conditions, open **Advanced** and review the stop
   policy shape exposed by the server.
4. Click **Validate**.
5. Preview two scenarios: one that should complete normally, and one that should
   stop.
6. Save only when both scenarios behave correctly.
7. Run an eval fixture for loops or time-sensitive behavior.

## Policy choices

| Risk | UI choice | How to verify |
|---|---|---|
| Repeating the same plan | Lower max rounds or add loop detection | Preview a prompt that used to loop |
| Long-running tool chain | Add timeout or tool-stop condition | Preview with a tool-heavy task |
| Token runaway | Add token budget | Eval fixture with long context/tool output |
| Error storm | Add consecutive-error bound | Preview with a failing tool or provider |
| Stop after a known terminal tool | Add stop-on-tool condition | Preview a task that calls that tool once |

## Operate after saving

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agents-list.png">
      <img src="/awaken/assets/admin-console/flow-agents-list.png" alt="Agents list with runtime inference statistics." loading="lazy" />
    </a>
    <figcaption>Watch run and latency signals after tightening limits.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-history.png">
      <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Agent editor History tab with update/create events and Restore actions." loading="lazy" />
    </a>
    <figcaption>Use the History tab to restore if a limit cuts off valid work.</figcaption>
  </figure>
</div>

A stricter stop policy can hide valid multi-step work. If users report early
stops, restore the previous version or raise the bound and rerun the same eval.

## API references for automation

- [Config reference: AgentSpec](/awaken/reference/config/#agentspec)
- [HTTP API](/awaken/reference/http-api/)
- [Cancellation](/awaken/reference/cancellation/)
