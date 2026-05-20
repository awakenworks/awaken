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
| Dashboard stat cards | `/v1/capabilities` + per-namespace lists | None render on error |
| Reference graph | `/v1/config/{agents,models,providers}` | Empty state if any list fails |
| Health card | `/v1/config/providers` + `/v1/config/mcp-servers` | Per-row error displayed |
| Recent activity | `/v1/audit-log?limit=12` | "Audit log is disabled" notice on `503` |
| System card | `/v1/system/info` | Card hidden on error |
| Agents list inferences | `/v1/agents/runtime-stats` | Banner + `n/a` cells on `503` |
| Provider Test button | `POST /v1/providers/:id/test` | Toast with backend error text |
| Editor Validate button | `POST /v1/config/:ns/validate` | Toast with backend error text |
| Editor History tab | `/v1/audit-log?resource=agents/:id` | Empty list on `503` |
| Editor Restore action | `POST /v1/config/:ns/:id/restore` | Toast on failure |
| MCP Live Status card | `/v1/mcp-servers/:id/status` | "Loading…"/"Unavailable" |
| MCP Restart button | `POST /v1/mcp-servers/:id/restart` | Toast |

## Major UI surfaces

### Chrome (sidebar + topbar)

- **Sidebar** (`src/components/admin-sidebar.tsx`) — three named groups:
  Configure (Agents, Models, Providers, MCP Servers), Observe (Dashboard,
  Audit Log, Eval Reports, Skill Registry), Assistant (AI Assistant). Each
  item supports an optional health dot (`useNavHealth`) and count.
- **Topbar** (`src/components/admin-topbar.tsx`) — sticky breadcrumb
  derived from the current route via `lib/nav.ts#resolveBreadcrumbs`,
  ⌘K command palette trigger, notification bell stub (no endpoint yet),
  and the bearer-token status pill (clicks open the token modal).

### Dashboard (`src/pages/dashboard-page.tsx`)

- Six stat cards (Agents/Skills/Models/Providers/MCP/Tools) backed by
  real counts from `/v1/capabilities` plus the relevant lists.
- **Reference graph** (`components/ui/reference-graph.tsx`) — three-column
  SVG: agents → models → providers. Edges resolve `agent.model_id` and
  `model.provider_id` against the loaded lists. Dangling refs are silently
  dropped (the graph is descriptive, not validating).
- **Health card** — per-provider `has_api_key` + per-MCP `restart_policy`
  rendered as `Pill` tones.
- **Activity timeline** — last 8 audit events, formatted with
  `formatRelativeTime` (auto-detects seconds vs ms).
- **System card** — `version`, `uptime`, three boolean wiring flags from
  `/v1/system/info`.

### Agents list (`src/pages/agents-page.tsx`)

- `PageHeader` with eyebrow / count / description / "+ New Agent" CTA.
- `FilterBar` chips: model · plugin · modified-range, plus a sort pill.
- Plugin chips overflow at `+N` after 3 visible.
- "Inferences (24h)" cell consumes `inference_count`, `error_count`,
  `p95_inference_duration_ms` from `/v1/agents/runtime-stats`. When the
  registry is unconfigured the column shows `n/a` and a banner explains
  why.

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
- History tab fetches `/v1/audit-log?resource=agents/:id`, lists events,
  and offers Restore on each.

### MCP servers (`src/pages/mcp-servers-page.tsx`)

- The editor card embeds a **Live Status** section with four stat cells:
  *State*, *Handshake*, *Tools*, and *Restart* — but the fourth cell
  swaps to *Failures (since last ok)* with a warn/error tone when
  `consecutive_failures > 0`.
- `last_attempt_at` and `last_success_at` are shown as relative
  timestamps under the four-stat grid.
- Restart-policy fields are visually disabled when `enabled === false`.
- "Exposed tools" sub-table renders `tools[]` as a name + description grid
  (per-tool latency lives on `/v1/agents/:id/runtime-stats`, not the MCP
  endpoint — see [Audit gap §B6](#known-gaps)).

### Skill registry (`src/pages/skills-page.tsx`)

Read-only snapshot of `/v1/capabilities`.skills. Each card renders the
allowed tools, source paths, arguments, **and a "What the LLM sees"
prompt-injection preview** built from the real `SkillInfo` fields.

### Audit log (`src/pages/audit-log-page.tsx`)

Paged table with filter bar (`since` / `until` / `action` / `resource` /
`actor`). Click a row to open a side panel with full event details and a
"Restore this version" action when applicable.

### Eval reports (`src/pages/eval-reports-page.tsx`)

100 % client-side: drag a NDJSON report onto the upload zone, optionally
add a baseline, see **case status tabs** (All / Passing / Failing /
Regressions / Newly fixed). No server persistence yet
(see [Audit gap §B2](#known-gaps)).

### ⌘K Command palette (`src/components/command-palette.tsx`)

- `CommandPaletteProvider` mounted inside `<AdminLayout>` so palette
  navigation can use `useNavigate`.
- Keyboard shortcut: `⌘K` / `Ctrl+K` to open, `Esc` to close.
- Sources: navigation tree (real `navGroups`), agents list (real
  `/v1/config/agents`), tool catalog (real `/v1/capabilities`), one
  literal action ("New agent").

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

## REST-only features (no console UI yet)

The following runtime surfaces are fully implemented on the server but
have **no admin-console screen** today. Drive them over HTTP with the
admin bearer token (`Authorization: Bearer <token>`).

| Area | Endpoints | Why no UI yet |
|---|---|---|
| Threads | `GET/POST /v1/threads`, `GET/PATCH/DELETE /v1/threads/:id`, `GET/POST /v1/threads/:id/messages` | The console is configuration-focused; thread browsing belongs to a planned "Runs" surface |
| Runs | `GET/POST /v1/runs`, `GET /v1/runs/:id`, `GET /v1/threads/:id/runs`, `GET /v1/threads/:id/runs/{latest,active}` | Same as above — see gap B1 |
| Run control | `POST /v1/runs/:id/cancel`, `POST /v1/runs/:id/inputs`, `POST /v1/threads/:id/{cancel,interrupt}` | Decision/interrupt UI is gated on B1 (active-runs endpoint shape) |
| HITL decisions | `POST /v1/runs/:id/decision`, `POST /v1/threads/:id/decision` | Tool-call resume/cancel needs an active-runs feed first |
| Mailbox | `GET/POST /v1/threads/:id/mailbox` | Inter-agent dispatch is invisible in the browser today |
| Skill CRUD | `POST/PUT/DELETE /v1/config/skills/:id` | Console renders skills read-only; full editor depends on B3 (version history) |
| Config diagnostics | `GET /v1/config/diagnostics` | Registry-wide validation report is fetched on dashboard load but not yet surfaced |
| Permission preview | `GET /v1/agents/:id/permission-preview` | Wired in the agent editor's Tools tab as a side-effect but has no dedicated screen |

See [HTTP API](/awaken/reference/http-api/) for request/response shapes.
These are the gaps to close when extending the console; do **not**
duplicate them by hand-rolling a separate operator tool.

## Known gaps (backend work required)

These need backend changes before the console can show real data:

- **B1** — no `/v1/agents/:id/active-runs` endpoint, so the per-agent
  dashboard cannot show "currently running / paused / blocked" panels,
  and the Runs/Decisions surfaces above can't be built honestly.
- **B2** — eval reports have no server persistence; everything is
  per-tab session memory.
- **B3** — skill version history, file tree, and activation log are not
  exposed; blocks the full skills editor.
- **B4** — no notification center endpoint, so the topbar bell is a stub.
- **B6 partial** — MCP `/status` does not track per-tool latency or a
  rolling 24h error count. The console derives "freshness" from
  `last_attempt_at` / `consecutive_failures` instead.
- **B9** — no time-series endpoint, so per-agent sparklines cannot be
  rendered honestly. The current cell shows totals + p95 instead.

## Related

- [Use the Admin Console](/awaken/how-to/use-admin-console/) — operator user manual
- [HTTP API](/awaken/reference/http-api/) — endpoint reference
- [Enable Observability](/awaken/how-to/enable-observability/) — turn on
  runtime stats so the agents-list "Inferences" column starts working
