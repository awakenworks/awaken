---
title: "Admin Console"
description: "Use the Awaken admin console to configure providers, models, agents, tools, MCP servers, traces, datasets, evals, and the built-in Admin Assistant."
---

The Admin Console is the browser control plane for a running `awaken-server`.
Use it when you want to create and tune agents online instead of rebuilding the
Rust binary: configure providers and models, edit prompts and tool descriptions,
assign MCP tools, tune reminders and deferred-tool policy, preview a draft, then
publish the next registry snapshot.

## Start It

For a local server with deterministic scripted replies:

```sh
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
AWAKEN_STORAGE_DIR=./target/admin-sessions \
cargo run -p ai-sdk-starter-agent
```

In another terminal:

```sh
pnpm install
pnpm --filter awaken-admin-console dev
```

Open `http://127.0.0.1:3002`, click the token pill in the top bar, and paste
`dev-token`. The backend URL defaults to `http://127.0.0.1:38080`; set
`VITE_BACKEND_URL` when the server runs elsewhere.

No model key is required for the scripted path. To use a real OpenAI-compatible
provider from boot, set `OPENAI_API_KEY` and optionally `OPENAI_BASE_URL`,
`OPENAI_ADAPTER`, and `AGENT_MODEL` before starting the server. Use
`AWAKEN_SEED_PROFILE=demo` only when you want sample agents and demo tools.

## Screenshots

These screenshots are static documentation captures made with sample API data.
A running console reads values from your backend APIs; if a subsystem is not
wired, the corresponding surface shows a disabled or unavailable notice.

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/01-dashboard.png">
      <img src="/awaken/assets/admin-console/01-dashboard.png" alt="Admin dashboard showing live workload, agent activity, recent audit events, provider and MCP health, and current scope metadata." loading="lazy" />
    </a>
    <figcaption>Dashboard: live workload, health, audit activity, and read-only scope.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/02-agent-editor.png">
      <img src="/awaken/assets/admin-console/02-agent-editor.png" alt="Agent editor with model selection, system prompt, tools, plugins, delegates, history, save controls, and preview chat." loading="lazy" />
    </a>
    <figcaption>Agent editor: prompts, tools, plugins, delegates, history, and draft preview.</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/03-agents-list.png">
      <img src="/awaken/assets/admin-console/03-agents-list.png" alt="Agents list with filters, model and plugin metadata, and runtime inference statistics." loading="lazy" />
    </a>
    <figcaption>Agents list: filters, model/plugin metadata, and runtime stats.</figcaption>
  </figure>
</div>

## First Setup

1. **Connect the backend.** Paste the admin bearer token when the top bar asks
   for it. The status pill turns green when `/v1/capabilities` is reachable.
2. **Configure a provider.** Providers hold endpoint, adapter, credentials,
   timeout, and provider-specific options. Use **Test** before relying on it.
3. **Configure a model.** Models give agents a stable `model_id` and describe
   the upstream model, modalities, context limits, pricing, and capabilities.
4. **Unlock Admin Assistant.** The built-in Admin Assistant becomes available
   after the first provider-backed model is configured. Its tools are locked by
   the server and do not appear in the normal tool registry.
5. **MCP-only setups are full configuration mode.** You can configure MCP
   servers and assign their tools to agents, but chat and preview surfaces still
   need a model executor.

Provider credentials and MCP credentials are intentionally separate. Providers
feed model execution. MCP server credentials belong to that MCP transport
(`env` for stdio, URL/config for HTTP), and agent access is controlled through
tool selection plus optional permission rules.

## Create And Tune An Agent

Open **Agents**, then **New Agent**.

1. In **Basics**, set the agent id, model, max rounds, reasoning effort, and
   system prompt.
2. In **Tools**, choose all tools or a custom set. Built-in, plugin, and MCP
   tools appear together, and tool description overrides are visible where a
   tool supports them.
3. In **Skills**, choose which skills the agent can see. Skills inject catalog
   guidance and activate through the `skill` tool; there is no separate
   `SkillSearch` tool today.
4. In **Delegates**, choose explicit sub-agents. Delegates become delegate
   tools during resolution; there is no separate `AgentSearch` tool today.
5. In **Plugins**, enable policies such as permission, reminder, generative UI,
   and deferred tools. A stored plugin section is inactive until the plugin is
   enabled.
6. Use **Validate** to check the draft without saving.
7. Use the right-side preview chat to test the unsaved draft.
8. **Save** publishes the validated config so new runs use the next registry
   snapshot.

The tuning surface is meant to be broad but still safe: prompts, tool
descriptions, system reminders, ToolSearch/deferred-tool policy, skill metadata,
delegates, plugin sections, model selection, and provider config are editable
online. New executable tools, provider factories, stores, and plugins remain
Rust code.

## Connect The Saved Agent To A Frontend

After an agent is saved, the editor shows a **Frontend integration** card in the
right column. It points to the agent-scoped protocol routes:

```text
POST /v1/ai-sdk/agents/<agent_id>/runs
POST /v1/ag-ui/agents/<agent_id>/runs
```

AI SDK v6 example:

```ts
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

const { messages, sendMessage } = useChat({
  transport: new DefaultChatTransport({
    api: "http://127.0.0.1:38080/v1/ai-sdk/agents/support-agent/runs",
  }),
});
```

Use the generic `/v1/ai-sdk/chat` route when the client should choose an agent
per request with `agent_id`. Use the agent-scoped route when a UI is bound to
one saved agent. See [AI SDK frontend integration](/awaken/how-to/integrate-ai-sdk-frontend/),
[AI SDK v6 reference](/awaken/reference/protocols/ai-sdk-v6/), and
[CopilotKit / AG-UI integration](/awaken/how-to/integrate-copilotkit-ag-ui/).

## Operate, Trace, And Evaluate

- **Dashboard** shows live workload, provider/MCP health, recent audit events,
  optional runtime stats, and read-only `scope_id`.
- **Recent runs** on a saved agent opens persisted traces when trace routes are
  enabled.
- **Datasets** can capture trace fixtures for evaluation.
- **Eval Runs** execute datasets against configured agents and models.
- **Eval Reports** view NDJSON reports and baseline diffs in the browser.

Trace and eval payloads may contain prompts, tool arguments, and model replies.
Protect the admin bearer token and route access accordingly.

## Version History And Pinning

Every config save records metadata and, when audit logging is wired, appears in
the Audit Log. Agent History lets you inspect diffs and restore a previous
snapshot back into the editing store.

Restore is intentionally a review step: after restoring, save/publish the
resource again when that restored payload should become active for new runs.
When the server is wired with a versioned registry store, published runtime
registry snapshots are immutable and durable runs carry a `resolution_id` so
resume and replay can reselect the same graph.

## Scope

`scope_id` is shown as read-only system metadata. The browser does not choose
scope directly; the server resolves scope from the trusted `HttpScopeProvider`
for each request. Hosted products should switch tenant/workspace scope in their
auth/provider layer and display the resolved value in the console.

## Related

- [Get Started](/awaken/get-started/) - start a local server and console
- [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/) - full tuning surface
- [Use the Admin Console](/awaken/how-to/use-admin-console/) - longer operator walkthrough
- [HTTP API](/awaken/reference/http-api/) - request and response reference
