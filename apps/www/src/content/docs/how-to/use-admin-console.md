---
title: "Use the Admin Console"
description: "Operate Awaken from the browser: connect to a server, tune agents, validate drafts, inspect traces, run evals, and restore prior versions."
---

The Admin Console is the primary tuning and operations UI for an Awaken server.
Use it after runtime capabilities exist in code: configure providers and models,
create agents, tune prompts and tool descriptions, assign tools/skills/delegates,
preview drafts, inspect traces, capture datasets, run evals, and review audit
history from the browser.

This guide focuses on **what to click**. For endpoint shapes and screen-to-route
mapping, see [HTTP API](/awaken/reference/http-api/), [Config](/awaken/reference/config/),
and the [Admin Console surface inventory](/awaken/reference/admin-console/).

## Prerequisites

- A running `awaken-server`. The default starter URL is `http://127.0.0.1:38080`.
- An admin bearer token configured on the server.
- The Admin Console dev server running locally, or a production build served by
  your deployment.

```sh
# Terminal 1 — runtime
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
cargo run -p ai-sdk-starter-agent

# Terminal 2 — admin console
pnpm --filter awaken-admin-console dev
# → http://127.0.0.1:3002
```

Open the console, click the top-right token pill, paste `dev-token`, and save.
The pill changes from **Token missing** to **Connected**.

## Visual tour

<figure class="screenshot">
  <a href="/awaken/assets/awaken-demo.gif">
    <img src="/awaken/assets/awaken-demo.gif" alt="Animated walkthrough: connect Gemini on Vertex, build an agent by hand, and run a live eval." loading="lazy" />
  </a>
  <figcaption>The full flow in motion — recorded against a live Gemini backend.</figcaption>
</figure>

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/01-dashboard.png">
      <img src="/awaken/assets/admin-console/01-dashboard.png" alt="Admin dashboard showing live workload, agent activity, recent audit events, health status, and current scope metadata." loading="lazy" />
    </a>
    <figcaption>Dashboard: workload, health, system metadata, and recent activity.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/02-agent-editor.png">
      <img src="/awaken/assets/admin-console/02-agent-editor.png" alt="Agent editor with model selection, system prompt fields, tabs, save controls, and draft preview chat." loading="lazy" />
    </a>
    <figcaption>Agent editor: tune, validate, preview, then save.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/03-agents-list.png">
      <img src="/awaken/assets/admin-console/03-agents-list.png" alt="Agents list with filters, model and plugin metadata, and runtime inference statistics." loading="lazy" />
    </a>
    <figcaption>Agents list: filter by model/plugin and inspect runtime signals.</figcaption>
  </figure>
</div>

## Navigate the workspace

The left sidebar groups screens by operator intent:

| Group | What lives here |
|---|---|
| **Agents** | Agent list, agent editor, per-agent dashboard. |
| **Infrastructure** | Providers and Models. Configure upstream access before live runs. |
| **Resources** | MCP Servers, A2A Servers, Skills, Tools. |
| **Observe** | Dashboard, Audit Log, Datasets, Eval Runs, Eval Reports. |

Use the breadcrumb in the topbar to move back to the parent screen. The Admin
Assistant is a floating bubble, not a sidebar destination; it becomes useful
after a provider-backed model is configured.

## Connect and inspect the system

1. Open the console and enter the bearer token.
2. Start on **Dashboard**.
3. Check **Health** for providers without keys and MCP servers that are failing.
4. Check **System** for version, scope, uptime, and which optional subsystems
   are connected.
5. Use stat cards to jump to Agents, Models, Providers, Skills, MCP Servers, or
   Tools.

If **Recent activity** says the audit log is disabled, the server can still run,
but History tabs and restore workflows will be empty until audit logging is
wired.

## Create provider and model

1. Open **Infrastructure → Providers**.
2. Create a provider with the adapter, base URL, and credentials for your model
   upstream.
3. Use the provider row's **Test** action. A toast reports success, latency, or
   the upstream error.
4. Open **Infrastructure → Models**.
5. Create a model id that points at the provider. Agents reference this stable
   model id, not raw provider credentials.

Related API: [provider/model config](/awaken/reference/provider-model-config/) and
[HTTP API](/awaken/reference/http-api/).

## Edit an agent

To create one, click **Agents → + New Agent**. To edit one, click a row in the
Agents list.

1. **Basics** — set model, max rounds, reasoning effort, and system prompt.
2. **Tools** — choose all tools or a custom allow/exclude selection. Use source
   filters to narrow built-in, plugin, and MCP tools.
3. **Plugins** — enable or disable plugin-backed behavior.
4. **Delegates** — select which other agents this agent can hand off to.
5. **Advanced** — inspect the raw JSON if you need to review the final shape.
6. **History** — inspect past changes and restore an earlier version when audit
   logging is enabled.

When you edit anything, the bottom save bar appears:

- **Validate** checks the draft without saving or applying it.
- **Save** / **Save & Publish** persists the draft and makes it available to new
  runs.

Use the right-side preview chat before saving. It runs against the unsaved draft
so you can tune prompts, tools, and model selection without publishing first.

Related API: `agents` config routes in [HTTP API](/awaken/reference/http-api/)
and `AgentSpec` in [Config](/awaken/reference/config/#agentspec).

## Tune behavior safely

A safe tuning pass is:

1. Change one behavior dimension at a time: prompt, model, tools, plugin config,
   permissions, delegates, or stop policy.
2. Click **Validate**.
3. Use the preview chat with the same scenario you care about.
4. Save only after the preview behaves as expected.
5. Run a real task or eval fixture to confirm behavior outside the draft preview.
6. If the result regresses, open **History** and restore the previous version.

See [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/) for the
full tab-by-tab tuning map.

## Manage resources

### Restart an MCP server

1. Open **Resources → MCP Servers**.
2. Click a server and scroll to **Live Status**.
3. Check connection state, handshake result, tool count, and retry/failure
   summary.
4. Click **Restart**. The button is disabled while restart is in flight.

### Connect an A2A server

Open **Resources → A2A Servers**, click **New A2A server**, and enter the remote
server base URL. Awaken discovers the remote agent cards and makes them
available for delegation. See [Connect an A2A Server](/awaken/how-to/connect-an-a2a-server/).

### Review skills and tools

Use **Skills** and **Tools** to inspect what the running server has discovered.
Agent access is still controlled from the agent editor's Tools, Plugins, and
Delegates tabs.

## Observe runs and compare behavior

Use **Observe** screens after real traffic or preview runs exist:

- **Dashboard** — workload, health, and recent activity.
- **Audit Log** — global create/update/delete/restart/restore history.
- **Datasets** — captured fixtures you can replay.
- **Eval Runs** — execution records for eval jobs.
- **Eval Reports** — pass/fail and baseline diffs.

For the full evaluation workflow, see
[Capture a Dataset and Run an Eval](/awaken/how-to/capture-a-dataset-and-run-an-eval/).

## Restore a previous version

Awaken's audit log doubles as version history.

1. Open an agent, model, provider, or MCP server editor.
2. Switch to **History**.
3. Expand an event to review the before/after diff.
4. Click **Restore this version**.
5. Review the diff and confirm.
6. Validate and save the restored draft when you are ready for new runs to use it.

Related API: restore routes in [HTTP API](/awaken/reference/http-api/) and audit
behavior in [Admin Console surface inventory](/awaken/reference/admin-console/).

## Enable optional subsystems

The console degrades honestly when optional server modules are absent:

| If absent | What you see | Enable via |
|---|---|---|
| Audit log | Dashboard disabled notice, empty Audit Log, empty History tabs | [Config reference](/awaken/reference/config/#auditlogconfig) and server wiring |
| Runtime stats | Agents list shows `n/a`; per-agent latency charts are unavailable | [Enable Observability](/awaken/how-to/enable-observability/) |
| Trace/eval stores | Dataset/eval screens cannot persist useful records | [Capture a Dataset and Run an Eval](/awaken/how-to/capture-a-dataset-and-run-an-eval/) |

## What still belongs in API automation

The console focuses on configuration and operator review. Use the HTTP API or
your own tooling for live execution and lower-level control:

- Threads, messages, and run inspection.
- Programmatic run creation, cancel, interrupt, and resume.
- HITL decisions for custom UIs.
- Mailbox inspection and dispatch automation.
- Registry diagnostics and bulk config management.

See the [REST-only features matrix](/awaken/reference/admin-console/#rest-only-features-no-console-ui-yet)
and [HTTP API reference](/awaken/reference/http-api/).

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Topbar says **Token missing** or **Token rejected** | Bearer token absent or wrong | Click the pill and paste the token configured on the server. |
| Topbar says **Backend unreachable** | Server not listening or wrong URL | Confirm the server is running on `BACKEND_URL`; default is `http://127.0.0.1:38080`. |
| Pages load but show optional subsystem warnings | Audit/runtime stats/trace/eval stores are not wired | Enable the corresponding subsystem on the server. |
| Save fails with "config management API not enabled" | No config store is wired | Start a server with config management enabled. |
| Provider Test reports unsupported adapter | The provider is scripted or not testable | Expected for scripted/demo providers; test a real adapter before production. |

## Related

- [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/)
- [Build an Agent with the Admin Assistant](/awaken/how-to/build-an-agent-with-the-assistant/)
- [Enable Tool Permission HITL](/awaken/how-to/enable-tool-permission-hitl/)
- [Capture a Dataset and Run an Eval](/awaken/how-to/capture-a-dataset-and-run-an-eval/)
- [Admin Console surface inventory](/awaken/reference/admin-console/)
