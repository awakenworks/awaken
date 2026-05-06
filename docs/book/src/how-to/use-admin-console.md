# Use the Admin Console

The admin console is the operator UI for an Awaken runtime. This guide walks
through the workflows that operators most often run from the browser.

For the technical inventory of every screen and widget, see
[Admin Console reference](../reference/admin-console.md).

## Prerequisites

- A running `awaken-server` reachable from your browser. The default URL is
  `http://127.0.0.1:38080`.
- An admin bearer token. Set it on the server via either:
  - `AWAKEN_ADMIN_API_BEARER_TOKEN` environment variable, or
  - `AdminApiConfig.bearer_token` field in the server config.
- The admin console dev server (`apps/admin-console`) running locally — or a
  production build served behind your edge.

```sh
# Terminal 1 — runtime
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
cargo run -p ai-sdk-starter-agent

# Terminal 2 — admin console
cd apps/admin-console && npm run dev
# → http://127.0.0.1:3002
```

When you first open the console, the topbar pill on the right shows
**Token missing**. Click it, paste your token, save. The pill flips to
**Connected** with a green dot.

## Navigate the workspace

The left sidebar groups every screen by intent:

| Group | What lives here |
|---|---|
| **Configure** | Agents, Models, Providers, MCP Servers — the runtime catalog you can edit. |
| **Observe** | Dashboard, Audit Log, Eval Reports, Skill Registry — read-only views into runtime state. |
| **Assistant** | AI Assistant — chat interface that runs a real agent against your live config. |

Hit `⌘K` (`Ctrl+K` on Linux/Windows) anywhere to open the **command
palette**. Type to search by agent id, tool name, or page name; arrow keys
to highlight; `Enter` to jump.

The **breadcrumb** in the topbar always tells you which group you're in
and lets you click back up.

## Inspect the system

Open the **Dashboard** (it's the default landing page). Key panels:

- **Stat cards** — counts of agents, skills, models, providers, MCP
  servers, and published tools, taken from `/v1/capabilities`. Click a
  card to drill into the corresponding list.
- **Reference graph** — visual map of which agents use which models, which
  models use which providers. Useful before deleting a node: an agent
  that points at a missing model is a configuration error.
- **Health** — providers (key set / no key) and MCP servers (auto-restart
  / manual). This tells you which providers will fail at request time
  because no key is configured.
- **Recent activity** — last 8 audit events, if the audit log is enabled
  on the server. If you see a yellow "**Audit log is disabled**" notice,
  enable it on the server (see [Enable the audit log](#enable-the-audit-log)).
- **System** — server version, uptime, and which optional subsystems
  (config store, audit log, runtime stats) are wired in.

## Edit an agent

1. Click **Agents** in the sidebar.
2. Use the **filter chips** to narrow by `model`, `plugin`, or `modified
   range`. The "Inferences (24h)" column shows real call counts when the
   observability registry is on (see [Enable runtime stats](#enable-runtime-stats)).
3. Click a row to open the editor.
4. The editor uses **visible tabs**:
   - **Basics** — agent ID (read-only after creation), model, max rounds,
     reasoning effort, system prompt.
   - **Tools** — choose between "All tools" and "Custom selection". Custom
     mode reveals a search box plus source filter tabs (All / Built-in /
     Plugin / MCP) and per-group select-all/clear actions. The same UI
     repeats for "Excluded Tools".
   - **Plugins** — toggle plugins on/off. The badge on the tab shows the
     enabled count.
   - **Delegates** — pick which other agents this one can hand off to.
     Selected delegates take the **agent-tint** (purple) treatment used
     across both Awaken and Oversight.
   - **Advanced** — raw JSON preview of the spec.
   - **History** — audit events for this resource. Each row has a
     **Restore** action that rolls the agent back to the version recorded
     by that event (see [Restore a previous version](#restore-a-previous-version)).
5. As soon as you change anything, the **bottom save bar** appears with two
   buttons:
   - **Validate** — sends your draft to `POST /v1/config/agents/validate`,
     which runs the same prepare + schema check as a real save but does
     **not** persist or apply. Use it to confirm your edits parse before
     publishing.
   - **Save** (or **Save & Publish** for new agents) — persists and
     applies. The runtime swaps to the new spec on the next request.
6. The **right column** is a draft-preview chat backed by
   `POST /v1/ai-sdk/agent-previews/runs`. You can talk to your draft
   *before* saving it; messages run against the unsaved spec.

## Test a provider

The Providers list has a per-row **Test** button:

1. Click **Test** next to the provider id.
2. The console calls `POST /v1/providers/:id/test`.
3. A toast reports either `OK · <latency>ms` or the backend error
   verbatim — for example, `unsupported provider adapter: scripted`.

Use this before publishing a new model binding to confirm the credentials
and adapter actually reach the upstream.

## Restart an MCP server

1. Open **MCP Servers** and click an existing server to edit it.
2. Scroll to **Live Status**. The four cells show: connection state,
   handshake result, tool count, and either restart-policy summary or
   "Failures (since last ok)" with a warn/error tone if the server is
   currently misbehaving.
3. The relative timestamps below the cells (`last attempt`, `last
   success`) tell you whether the manager is actively retrying.
4. Click **Restart** to trigger `POST /v1/mcp-servers/:id/restart`.
   The button is disabled while a restart is in flight; an audit
   `restart` event is emitted on success.

## Restore a previous version

Awaken's audit log is also a version history.

1. Open any resource editor (agent / model / provider / MCP server).
2. Switch to the **History** tab.
3. Each event row shows the actor, timestamp, and a one-line description
   of what changed. Click a row to expand the before/after diff.
4. Click **Restore this version** on the row you want to roll back to.
   The console previews the JSON diff between current and target and
   asks for confirmation.
5. On confirm, the console calls
   `POST /v1/config/:ns/:id/restore` with the event id. The server
   re-applies that snapshot through the normal validate + apply pipeline
   and emits a fresh `restore` audit event with `restored_from = <event-id>`
   so the rollback itself is auditable.

## Browse the audit log

Open **Audit Log** for a global view across every resource:

- **Since / Until** filter for time range.
- **Action** filter (create / update / delete / restart / publish / restore).
- **Resource** filter — substring match on `<namespace>/<id>`.
- **Actor** filter — accepts the SHA-256 prefix shown on each row.

Click a row to open a side panel with the full event JSON, before/after
diff, and (when applicable) the **Restore** button.

If you see an empty page that says the filter form but never any rows,
the audit log is probably disabled — see below.

## Enable optional subsystems

The console honestly degrades when the runtime hasn't opted into these,
but you'll get a much better experience with them on.

### Enable the audit log

In the server config (or via `AdminApiConfig`):

```toml
[admin_api]
audit_log_enabled = true
audit_retention_days = 90      # optional, default 90
```

Without this:
- Dashboard "Recent activity" shows the disabled notice.
- Audit Log page renders the filters but always returns 0 rows.
- Editor "History" tab is empty.

### Enable runtime stats

Wire the observability plugin into your `AppState`:

```rust,ignore
use awaken_ext_observability::{ObservabilityPlugin, RuntimeStatsRegistry};

let registry = Arc::new(RuntimeStatsRegistry::new());
let observability = ObservabilityPlugin::new()
    .with_sink(SharedRegistrySink(Arc::clone(&registry)));

let state = AppState::new(/* ... */)
    .with_runtime_stats(registry);

let runtime = AgentRuntimeBuilder::default()
    .with_plugin("observability", Arc::new(observability))
    .build();
```

Without this:
- Agents list shows a banner and `n/a` cells in the "Inferences (24h)"
  column.
- Per-agent dashboard cannot render its latency histogram.

See [Enable Observability](./enable-observability.md) for the full
recipe.

## Switch to dark mode

Add `data-theme="dark"` to `<html>` (or any subtree) and the
`--aw-*` tokens flip automatically. There is no built-in toggle in the
chrome yet — most operators run dark via a browser extension or the
system colour scheme (`tokens-auto-dark.css` honours
`prefers-color-scheme: dark`).

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Topbar pill says **Token missing** or **Token rejected** | Bearer token absent or wrong | Click the pill, paste the token configured on the server |
| Topbar pill says **Backend unreachable** | Server not listening or wrong URL | Confirm the server is running on `BACKEND_URL`. The default is `http://127.0.0.1:38080`. Override with `VITE_BACKEND_URL` at build time |
| Console shows `503` errors but pages still load | An optional subsystem (audit / runtime stats) is off | See [Enable optional subsystems](#enable-optional-subsystems) |
| Save fails with "config management API not enabled" | The server has no `ConfigStore` wired | Embedder must call `AppState::with_config_store(...)` |
| Provider Test always returns "unsupported adapter" | The provider uses the `scripted` adapter (no upstream to probe) | Expected; only real adapters have a meaningful test path |
| Sidebar nav health dot stays neutral | Health badges are derived from list payloads only — full per-server probes are intentionally not made on every page load | Open the resource detail to see live `/status` |

## Related

- [Admin Console reference](../reference/admin-console.md)
- [HTTP API](../reference/http-api.md)
- [Enable Observability](./enable-observability.md)
