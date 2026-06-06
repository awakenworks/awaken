---
title: "Tune & Operate"
description: "Operate Awaken from the Admin Console first: tune agents, review permissions, inspect traces, run evals, and use API references only for automation."
---

This section is for people operating a running Awaken server from the browser.
Treat the **Admin Console** as the primary surface: click through the UI, change
one thing, validate, preview, save, then compare behavior. Use REST/config APIs
only when the same operation needs automation.

## Start from the console

<figure class="screenshot">
  <a href="/awaken/assets/admin-console/flow-dashboard-health.png">
    <img src="/awaken/assets/admin-console/flow-dashboard-health.png" alt="Admin Console dashboard showing workload, health, recent audit events, and system metadata." loading="lazy" />
  </a>
  <figcaption>Start on Dashboard: confirm the server is connected, healthy, and using the expected scope.</figcaption>
</figure>

1. Open [Use the Admin Console](/awaken/how-to/use-admin-console/) and connect to
   the server.
2. Check **Dashboard → Health** before making changes.
3. Configure **Providers** and **Models** under **Infrastructure**.
4. Open **Agents**, edit a draft, click **Validate**, use the preview chat, then
   **Save**.
5. Use **Audit Log** and editor **History** whenever you need to review or
   restore a saved change.

## Tune behavior by interaction

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-basics.png">
      <img src="/awaken/assets/admin-console/flow-agent-basics.png" alt="Agent editor with Basics, Tools, Plugins, Delegates, Advanced, History, save controls, and preview chat." loading="lazy" />
    </a>
    <figcaption>Agent editor: tune prompt, model, tools, plugins, delegates, and limits.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-tools.png">
      <img src="/awaken/assets/admin-console/flow-agent-tools.png" alt="Agent editor Tools tab with allowed tool checkboxes, excluded delete_file, patterns, and preview chat." loading="lazy" />
    </a>
    <figcaption>Agent Tools tab: expose web_search, read_document, and filesystem/read_file while excluding delete_file.</figcaption>
  </figure>
</div>

| Goal | Click path | Guide |
|---|---|---|
| Tune prompt/model/tools/plugins/delegates | **Agents → Agent → Basics / Tools / Plugins / Delegates → Validate → Preview → Save** | [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/) |
| Change prompt wording safely | **Agent → Basics → System prompt → Validate → Preview → Save** | [Hot-Tune Prompts](/awaken/how-to/hot-tune-prompts/) |
| Manage context and compaction | **Agent → Advanced / JSON preview → Validate → Preview long task → Save** | [Optimize Context Window](/awaken/how-to/optimize-context-window/) |
| Bound loops and runaway work | **Agent → Basics / Advanced → limits and stop policy → Validate → Preview → Save** | [Configure Stop Policies](/awaken/how-to/configure-stop-policies/) |

## Govern and evaluate by interaction

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-agent-history.png">
      <img src="/awaken/assets/admin-console/flow-agent-history.png" alt="Agent editor History tab with update/create events and View/Restore actions." loading="lazy" />
    </a>
    <figcaption>History tab: review saved agent changes and restore a prior version before publishing.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/flow-eval-run-detail.png">
      <img src="/awaken/assets/admin-console/flow-eval-run-detail.png" alt="Eval run detail page with pass rate and per-fixture results." loading="lazy" />
    </a>
    <figcaption>Eval detail: compare behavior with replayable fixtures before accepting a change.</figcaption>
  </figure>
</div>

| Goal | Click path | Guide |
|---|---|---|
| Gate sensitive tools | **Agent → Plugins → Permission rules → Ask / Allow / Deny → Save** | [Enable Tool Permission HITL](/awaken/how-to/enable-tool-permission-hitl/) |
| Add remote agents for handoff | **Resources → A2A Servers → New A2A server → Refresh card → Save** | [Connect an A2A Server](/awaken/how-to/connect-an-a2a-server/) |
| Capture and replay behavior | **Observe → Datasets → Eval Runs → Eval Reports** | [Capture a Dataset and Run an Eval](/awaken/how-to/capture-a-dataset-and-run-an-eval/) |
| Inspect runtime signals | **Dashboard → Agents list → Agent dashboard / Recent runs** | [Enable Observability](/awaken/how-to/enable-observability/) |

## API references for automation

Use these when the same operation should be scripted instead of clicked:

- [HTTP API](/awaken/reference/http-api/) — routes, auth, request/response shapes.
- [Config](/awaken/reference/config/) — `AgentSpec`, provider/model config, plugin sections.
- [Admin Console surface inventory](/awaken/reference/admin-console/) — screen-to-endpoint mapping and REST-only surfaces.
