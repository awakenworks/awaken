---
title: "Hot-Tune Prompts"
description: "Tune a saved agent's prompt from the Admin Console: edit Basics, validate, preview with the draft, save, then compare traces or evals."
---

Use this when the agent already exists and you want to improve instructions
without rebuilding the server. The Admin Console is the primary path; API writes
are for automation after the same change is proven in the UI.

## What you click

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-agent-basics.png">
    <img src="/awaken/assets/admin-console/flow-agent-basics.png" alt="Agent editor Basics tab with model selection, system prompt, save controls, and preview chat." loading="lazy" />
  </a>
  <figcaption>Edit the system prompt in Basics, validate the draft, preview it, then save.</figcaption>
</figure>

1. Open **Agents** and choose the agent.
2. Stay on **Basics**.
3. Edit **System prompt**. Keep the change narrow: role, constraints, answer
   shape, tool-use guidance, or refusal policy.
4. Click **Validate**. Fix any field errors before previewing.
5. Use the preview chat with a representative prompt. The preview uses the
   unsaved draft.
6. Click **Save** when the draft behaves better.
7. Open **History** or **Audit Log** if you need to compare or restore.

## What to compare

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-history.png">
      <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Agent editor History tab showing a system_prompt update and restore action." loading="lazy" />
    </a>
    <figcaption>History shows the saved prompt change and restore action for the agent.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run detail with per-fixture pass and fail results." loading="lazy" />
    </a>
    <figcaption>Eval results show whether the prompt improved the behavior you care about.</figcaption>
  </figure>
</div>

Use the same scenario before and after the edit:

- A preview chat prompt for quick feedback.
- A saved trace if you need to inspect exact tool calls and model output.
- A dataset/eval run when the behavior must be repeatable.

See [Capture a Dataset and Run an Eval](/awaken/how-to/capture-a-dataset-and-run-an-eval/).

## What is safe to tune live

| Prompt change | UI location | Notes |
|---|---|---|
| Role and tone | **Basics → System prompt** | Good first edit; easy to preview. |
| Output format | **Basics → System prompt** | Pair with eval checks when format matters. |
| Tool-use guidance | **Basics → System prompt** plus **Tools** | Also narrow allowed tools if wording alone is not enough. |
| Safety boundaries | **Basics → System prompt** plus **Plugins → Permission** | Use permission rules for tool-level enforcement. |
| Model choice | **Basics → Model** | Preview again; models can interpret the same prompt differently. |

Changes affect **new runs after save**. Active runs keep the spec they already
resolved.

## When to use API instead

Use the config API when prompt edits come from CI, migrations, or internal tools.
Keep the same loop: validate, write, run a representative task, then compare.

Related references:

- [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/)
- [HTTP API](/awaken/reference/http-api/)
- [Config reference](/awaken/reference/config/#agentspec)
