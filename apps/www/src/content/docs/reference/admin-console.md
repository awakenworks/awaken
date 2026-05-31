---
title: "Admin Console"
description: "The Awaken admin console (apps/admin-console) is a Vite + React 19 SPA that talks to the running awaken-server over its admin HTTP API. It is shipped with the runtime so operators can inspect and…"
---

The Awaken admin console (`apps/admin-console`) is a Vite + React 19 SPA that
talks to the running `awaken-server` over its admin HTTP API. It is shipped
with the runtime so operators can inspect and edit the live config without
restarting the server.

This page is a **surface inventory**. For walkthroughs, see
[Use the Admin Console](/awaken/how-to/use-admin-console/).

## Screenshots

These screenshots are static documentation captures made with sample API data.
They are served with the docs site so operator manuals and README material point
at the same visual surface. A running admin console reads these values from the
configured backend APIs.

<div class="screenshot-grid">
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/01-dashboard.png">
      <img src="/awaken/assets/admin-console/01-dashboard.png" alt="Admin dashboard with live workload, agent activity, recent activity, provider and MCP health, and read-only scope metadata." loading="lazy" />
    </a>
    <figcaption>Dashboard with current scope</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/02-agent-editor.png">
      <img src="/awaken/assets/admin-console/02-agent-editor.png" alt="Agent editor showing basics, tools, plugins, delegates, advanced JSON, history, save controls, and preview chat." loading="lazy" />
    </a>
    <figcaption>Agent editor</figcaption>
  </figure>
  <figure class="screenshot">
    <a href="/awaken/assets/admin-console/cmdk.png">
      <img src="/awaken/assets/admin-console/cmdk.png" alt="Admin command palette for quick navigation across dashboard, agents, providers, MCP servers, audit log, and assistant surfaces." loading="lazy" />
    </a>
    <figcaption>Command palette</figcaption>
  </figure>
</div>

## Architecture

| Layer | Lives at | Purpose |
|---|---|---|
| Token pipeline | `packages/design-tokens/` (`@awaken/design-tokens`) | Style Dictionary v4 source for `--aw-*` CSS variables. JSON sources are split between Awaken-specific (warm-leaning slate + indigo) and a "shared with `~/Codes/teams`" subset enforced by `tokens.parity.test.ts`. Consumed via pnpm workspace dependency. |
| Generated CSS | `packages/design-tokens/dist/css/` (gitignored) | `tokens.css`, `tokens-dark.css`, `tokens-auto-dark.css`, `tokens.json` — produced by `pnpm tokens:build` (auto-runs as `predev`/`prebuild`/`pretest`) and imported via `@awaken/design-tokens/css/*` |
| Tailwind | `tailwind.config.ts` | `theme.extend` exposes the `--aw-*` tokens as semantic Tailwind classes (`bg-fg-strong`, `text-state-progress`, `shadow-card`, etc.) |
| Routing | `src/app.tsx` (data router via `createBrowserRouter`) | Required for `useBlocker` (unsaved-changes guard) |
| Auth | `src/components/auth-provider.tsx` | Bearer token in `localStorage`, surfaced as the topbar status pill |

## Backend dependencies

Every screen consumes one or more endpoints from
[HTTP API](/awaken/reference/http-api/). The console is built so that **no UI element
fabricates data** — when an endpoint returns `503` or `null`, the
corresponding widget collapses to a placeholder or a "feature disabled"
notice. This is enforced by code review, not by runtime check.

| Surface | Endpoint(s) | Failure mode |
|---|---|---|
| Sidebar nav counts | `/v1/config/{providers,mcp-servers,agents}` | Count omitted on error |
| Topbar status pill | `/v1/capabilities` (probe) | Tone reflects error class |
| Dashboard workload card | `/v1/runs/summary` | Disabled notice on `404` or backend error |
| Dashboard stat cards | `/v1/capabilities` + per-namespace lists | None render on error |
| Health card | `/v1/config/providers` + `/v1/config/mcp-servers` | Per-row error displayed |
| Recent activity | `/v1/audit-log?limit=12` | "Audit log is disabled" notice on `503` |
| System card | `/v1/system/info` + `/v1/system/modules` | Card hidden on error |
| Agents list inferences | `/v1/agents/runtime-stats` | Banner + `n/a` cells on `503` |
| Provider Test button | `POST /v1/providers/:id/test` | Toast with backend error text |
| Model Test action | `POST /v1/eval/online` | Disabled notice when eval routes are hidden |
| Editor Validate button | `POST /v1/config/:ns/validate` | Toast with backend error text |
| Editor History tab | `/v1/audit-log?resource=agents/{id}` | Empty list on `503` |
| Editor Restore action | `POST /v1/config/:ns/:id/restore` | Toast on failure |
| Recent runs drawer | `/v1/traces?agent_id=…`, `/v1/traces/:run_id` | Disabled notice when trace routes or trace store are unavailable |
| Save trace as fixture | `/v1/eval/datasets`, `/v1/eval/datasets/:id/items` | Disabled notice when eval routes are hidden |
| Datasets | `/v1/eval/datasets`, `/v1/eval/datasets/:id` | Disabled notice when eval routes are hidden |
| Eval runs | `/v1/eval/runs`, `/v1/eval/runs/:id` | Disabled notice when eval routes are hidden |
| MCP Live Status card | `/v1/mcp-servers/:id/status` | "Loading…"/"Unavailable" |
| MCP Restart button | `POST /v1/mcp-servers/:id/restart` | Toast |

## Major UI surfaces

### Chrome (sidebar + topbar)

- **Sidebar** (`src/components/admin-sidebar.tsx`) — grouped by workflow:
  Agents, Resources (Models, Tools, Skills), Infrastructure (Providers, MCP
  Servers), Observe (Dashboard, Audit Log, Datasets, Eval Runs, Eval Reports),
  and Assistant. Each item supports an optional health dot (`useNavHealth`) and
  count.
- **Topbar** (`src/components/admin-topbar.tsx`) — sticky breadcrumb
  derived from the current route via `lib/nav.ts#resolveBreadcrumbs`,
  notification bell stub (no endpoint yet), bearer-token status pill (clicks
  open the token modal), and optional search/command entry points.

### Dashboard (`src/pages/dashboard-page.tsx`)

- Workload card backed by `/v1/runs/summary`, showing running, waiting, and
  created run counts.
- Six stat cards (Agents/Skills/Models/Providers/MCP/Tools) backed by
  real counts from `/v1/capabilities` plus the relevant lists.
- **Health card** — per-provider `has_api_key` + per-MCP `restart_policy`
  rendered as `Pill` tones.
- **Activity timeline** — last 8 audit events, formatted with
  `formatRelativeTime` (auto-detects seconds vs ms).
- **System card** — `version`, read-only `scope_id`, `uptime`, three boolean
  wiring flags from `/v1/system/info`, and mounted module names from
  `/v1/system/modules`.

`scope_id` is an operator signal, not an admin-console filter. The server
resolves it from the trusted `HttpScopeProvider` for each request. Hosted or
multi-workspace products should switch scope in their auth/provider layer and
show the resolved value read-only, rather than letting the browser submit an
arbitrary scope key.

### Agents list (`src/pages/agents-page.tsx`)

- `PageHeader` with eyebrow / count / description / "+ New Agent" CTA.
- `FilterBar` chips: model · plugin · modified-range, plus a sort pill.
- Plugin chips overflow at `+N` after 3 visible.
- "Inferences" cell consumes `inference_count`, `error_count`,
  `p95_inference_duration_ms` from `/v1/agents/runtime-stats`. The
  effective window is the `RuntimeStatsRegistry`'s configured window
  (not necessarily 24h). When the registry is unconfigured the column
  shows `n/a` and a banner explains why.

### Agent editor (`src/pages/agent-editor-page.tsx`)

- Visible tab strip (Basics/Tools/Plugins/Delegates/Advanced/History) with
  per-tab badges computed from the spec (`Tools 3·−1` = 3 allowed, 1
  excluded; `Plugins 2` = `plugin_ids.length`).
- **Sticky bottom save bar** (only visible when dirty/new) with two
  buttons: **Validate** (calls `POST /v1/config/agents/validate`) and
  **Save** (calls `POST` or `PUT`). Validate is non-destructive.
- Tools tab embeds `<ToolSelector variant="include">` and
  `<ToolSelector variant="exclude">`, each with **source filter tabs**
  (All/Built-in/Plugin/MCP) and per-group select-all / clear actions.
- Delegates tab uses `--aw-agent-tint` for selected items so the AI-assigned
  visual identity is consistent with Oversight.
- Right column: `<AgentPreviewPanel>` chats against the draft via
  `POST /v1/ai-sdk/agent-previews/runs` (drafts run without saving).
- Saved agents expose a recent-runs drawer backed by `/v1/traces`; individual
  runs can be opened as NDJSON trace pages and saved into eval datasets when
  trace and eval routes are both enabled.
- History tab fetches `/v1/audit-log?resource=agents/{id}`, lists events,
  and offers Restore on each. Restore writes the selected snapshot back to
  `ConfigStore` but deliberately does not call the runtime hot-swap path; use a
  normal config save/PUT after review when the restored payload should become
  the active registry snapshot for later runs.

### MCP servers (`src/pages/mcp-servers-page.tsx`)

- The editor card embeds a **Live Status** section with four stat cells:
  *State*, *Handshake*, *Tools*, and *Restart* — but the fourth cell
  swaps to *Failures (since last ok)* with a warn/error tone when
  `consecutive_failures > 0`.
- `last_attempt_at` and `last_success_at` are shown as relative
  timestamps under the four-stat grid.
- Restart-policy fields are visually disabled when `enabled === false`.
- "Exposed tools" sub-table renders `tools[]` as a name + description grid.
  Per-tool latency lives on `/v1/agents/:id/runtime-stats`, not the MCP
  endpoint.

### Skill registry (`src/pages/skills-page.tsx`)

Read-only snapshot of `/v1/capabilities`.skills. Each card renders the
allowed tools, source paths, arguments, **and a "What the LLM sees"
prompt-injection preview** built from the real `SkillInfo` fields.

### Audit log (`src/pages/audit-log-page.tsx`)

Paged table with filter bar (`since` / `until` / `action` / `resource` /
`actor`). Click a row to open a side panel with full event details and a
"Restore this version" action when applicable.

### Datasets, eval runs, and reports

- **Datasets** (`src/pages/datasets-page.tsx`,
  `src/pages/dataset-detail-page.tsx`) manage `/v1/eval/datasets*` records and
  fixtures. Trace capture appends fixtures through
  `POST /v1/eval/datasets/:id/items`.
- **Eval runs** (`src/pages/eval-runs-page.tsx`,
  `src/pages/eval-run-detail-page.tsx`) list and inspect `/v1/eval/runs*`.
  Dataset detail pages can start new runs.
- **Eval reports** (`src/pages/eval-reports-page.tsx`) remain a client-side
  NDJSON report viewer for externally generated reports and baseline diffs.

Eval and trace payloads can contain prompts, tool arguments, and model
responses. The console renders privacy notices on those surfaces; access should
be scoped through the admin bearer token.

## Token system

The console consumes `@awaken/design-tokens`, a pnpm workspace package at
`packages/design-tokens/` that runs Style Dictionary v4 over DTCG JSON
sources. The shared `phase` / `chrome` / `agent` / `tone` subtrees are
parity-checked against teams' upstream JSON, so both products keep family
resemblance while every Awaken surface (admin console, www, future
playgrounds) imports the same token build by construction. Details in
`packages/design-tokens/README.md`.

## Disabled-feature notices

Three optional subsystems can be off without breaking the console:

| Subsystem | Endpoint | UI signal |
|---|---|---|
| Audit log | `/v1/audit-log` returns `503` | Yellow notice on dashboard activity timeline + audit log page renders the filter form but no rows |
| Runtime stats | `/v1/agents/runtime-stats` returns `503` | Banner above agents list + `n/a` in Inferences column |
| Config store | `/v1/system/info` returns `config_store_enabled: false` | System card shows `none` (neutral tone) |
| Trace routes/store | `/v1/traces*` returns `404`/`503` | Recent runs drawer explains that trace persistence is unavailable |
| Eval routes/store | `/v1/eval/*` returns `404`/`503` | Datasets, Eval Runs, model test, and trace-to-fixture controls show disabled notices |

## REST-only features (no console UI yet)

The following runtime surfaces are fully implemented on the server but
have **no admin-console screen** today. Drive them over HTTP with the
admin bearer token (`Authorization: Bearer <token>`).

| Area | Endpoints | Why no UI yet |
|---|---|---|
| Threads | `GET/POST /v1/threads`, `GET/PATCH/DELETE /v1/threads/:id`, `GET/POST /v1/threads/:id/messages` | The console is configuration-focused; thread browsing is served by the HTTP API |
| Runs | `GET/POST /v1/runs`, `GET /v1/runs/:id`, `GET /v1/threads/:id/runs`, `GET /v1/threads/:id/runs/{latest,active}` | The HTTP API is the source for execution records |
| Run control | `POST /v1/runs/:id/cancel`, `POST /v1/runs/:id/inputs`, `POST /v1/threads/:id/{cancel,interrupt}` | Use the REST endpoints for precise execution control |
| HITL decisions | `POST /v1/runs/:id/decision`, `POST /v1/threads/:id/decision` | Tool-call resume/cancel needs an active-runs feed first |
| Mailbox | `GET/POST /v1/threads/:id/mailbox` | Inter-agent dispatch is invisible in the browser today |
| Canonical events | `GET /v1/threads/:id/events`, `GET /v1/runs/:id/events`, plus `/stream` variants | The console uses trace/eval views for operator workflows; canonical event replay remains REST/SSE |
| Skill CRUD | `POST /v1/config/skills`, `PUT/DELETE /v1/config/skills/:id` | Console renders skills read-only; edit over REST when automation owns skill config |
| Config diagnostics | `GET /v1/config/diagnostics` | REST-only registry-wide validation report; no console screen yet |
| Permission preview | `GET /v1/agents/:id/permission-preview` | Wired in the agent editor's Tools tab as a side-effect but has no dedicated screen |

See [HTTP API](/awaken/reference/http-api/) for request/response shapes.
When extending the console, reuse these HTTP surfaces instead of creating a
separate operator API.

## Server data not exposed to the console UI

The console renders only data that the server exposes explicitly:

- There is no `/v1/agents/:id/active-runs` endpoint, so per-agent dashboards
  show rolling stats rather than "currently running / paused / blocked" panels.
- Eval reports are loaded from user-provided NDJSON files in the browser; saved
  eval datasets/runs use the `/v1/eval/*` API instead of this upload flow.
- Skill version history, file trees, and activation logs are not exposed, so
  skills are read-only in the console.
- The topbar notification bell has no server endpoint.
- MCP `/status` exposes connection health and restart counters, not per-tool
  latency or rolling error totals.
- Runtime stats expose aggregate totals and latency distributions for retained
  windows, not a per-agent time-series endpoint.

## Related

- [Use the Admin Console](/awaken/how-to/use-admin-console/) — operator user manual
- [HTTP API](/awaken/reference/http-api/) — endpoint reference
- [Enable Observability](/awaken/how-to/enable-observability/) — turn on
  runtime stats so the agents-list "Inferences" column starts working
