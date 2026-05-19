---
title: "Design Philosophy"
description: "Three rules that shape Awaken, and four capabilities they unlock that other agent frameworks usually lack."
---

Awaken is structured around three rules. Each is a hard line, not a guideline. Together they produce four properties most agent frameworks lack.

## Rule 1 — Code is for tools; config is for prompts

A tool is a Rust artefact: typed input schema, optional state writes, optional deferred-loading hooks. Tools want compile-time checks. They change rarely.

Prompts, tool descriptions, reminders, permission rules, and skill catalogs are content. They change constantly. They want a fast feedback loop.

The two categories are kept strictly separate.

| Layer | Lives in | Reload trigger |
|---|---|---|
| Tools, plugins, schemas | Rust code | Build & deploy |
| Agent system prompts, tool descriptions | `AgentSpec` via config API | Next run |
| Permission rules (allow/deny/ask) | Plugin config | Next run |
| Reminder rules (tool patterns → messages) | Plugin config | Next run |
| Skill packages (YAML on disk) | Filesystem | `PeriodicRefresher` (opt-in via `start_periodic_refresh(interval)`) |
| MCP server tools | Remote MCP server | `tools/list_changed` notification (automatic) |

Where reload is automatic the runtime does the watching. Where it is opt-in (skills) one call to `start_periodic_refresh` from your bootstrap turns it on — you still don't write the watcher.

The inner loop most agent work spends time in — *tweak prompt → observe* — becomes a config-API round trip instead of a CI run.

## Rule 2 — One config plane, one admin console

`/v1/config/*` is the only mutation API for runtime state. Agents, models, providers, plugins, MCP servers, skill packages, permission rules, trace history all surface through it.

The admin console is one consumer of that API. CI pipelines are another. The runtime reads from the same source the console writes to.

There is no "ops UI" sub-project, no shadow YAML in production, no out-of-band cache that drifts from the running config.

## Rule 3 — The runtime is the platform

Booting the server enables, without configuration:

- OpenTelemetry GenAI traces per phase, per tool, per LLM call.
- A persistent trace store the admin console queries.
- An eval framework with fixture replay, scoring, baseline diffing.
- HITL via the permission gate + mailbox suspend/resume.

These are not optional libraries the user composes. They are the runtime. Day-one projects get the same surface the largest deployments use.

---

## Four properties that follow

### Snapshot isolation + deterministic replay

Each phase reads an immutable `Snapshot` and emits a typed `MutationBatch`. `commit` applies the batch atomically, even when tools ran in parallel.

Two consequences:

- **Parallel tools never corrupt state.** Each typed state key declares a `MergeStrategy` (`Exclusive`, `Commutative`). Merges are checked at compile time.
- **Any snapshot is a time machine.** Past runs replay byte-for-byte from saved state — debug incidents, regression-test, run eval over yesterday's traffic without re-paying LLM cost.

The common alternative is mutable shared state behind locks (or forced serialisation). Both fail silently the moment two plugins touch the same field.

### One backend, four protocols

The same `/v1/runs` is exposed as:

- **AI SDK v6** for Vercel `useChat()`
- **AG-UI** for CopilotKit (chat + generative UI + HITL)
- **A2A** for agent-to-agent calls
- **MCP HTTP** for Claude / Cursor / Zed

Runtime emits one `AgentEvent` stream; protocol adapters encode for each wire format. Switching frontends does not touch agent code; serving multiple does not multiply the runtime.

The common alternative is pick-one-protocol-and-port. That binds agent code to a frontend choice that may not survive next quarter.

### Permission gates as runtime primitives

Permission is not a UI prompt or a middleware hook. It runs in the typed `ToolGate` phase (a `Phase` enum variant in `awaken-contract/src/model/phase.rs`) between tool decision and execution — the runtime always enters that phase before any tool runs.

`awaken-ext-permission` matches each call against rules:

- `Allow` — proceed.
- `Deny` — short-circuit with a structured error.
- `Ask` — suspend the run via mailbox, persist the question, resume on response (web UI, Slack bot, CLI — your choice).

Rules combine glob/regex on the tool name with JSON-path expressions on arguments. Rules live in config (Rule 1 — they tune live).

The common alternative is exception-throwing tools + frontend dialogs. That locks HITL into one frontend and loses suspend/resume semantics for long-running flows.

### Generative UI as a streamed primitive

Agents emit declarative UI (A2UI components, JSON Render trees, OpenUI Lang documents) on the same `AgentEvent` stream as text. Protocol adapters forward to the frontend; frontends render without per-tool glue.

UI surfaces are first-class state — they have IDs, updates merge, subtrees are debuggable through the same trace store as any other tool output.

The common alternative is "tool returns JSON, frontend writes React per shape." That binds UI iteration to frontend deploys and breaks the live-tuning loop the moment UI is involved.

---

## See also

- [Architecture](/awaken/explanation/architecture/) — three-layer runtime structure
- [Run Lifecycle and Phases](/awaken/explanation/run-lifecycle-and-phases/) — the nine phases
- [State and Snapshot Model](/awaken/explanation/state-and-snapshot-model/) — merge strategies in depth
- [HITL and Mailbox](/awaken/explanation/hitl-and-mailbox/) — suspend/resume semantics
- [Design Tradeoffs](/awaken/explanation/design-tradeoffs/) — alternatives considered
