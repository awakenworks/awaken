---
title: "Introduction"
description: "Awaken — build agent capabilities once in Rust, tune behavior live, and serve every client from the same runtime."
---

**Awaken** is a production AI agent backend written in Rust. Build tools, state,
and plugins once in code; tune agents, models, and prompts live through config;
then serve in-process apps, production APIs, protocol frontends, and the admin
console from the same runtime. Modules and plugins opt in where they own
storage, secrets, or policy.

Dependency snippets on this site follow the current main-branch API. Use the
git dependency shown in examples until the next crates.io release lands; use
the migration guide when upgrading from the published `0.5` line.

Three design rules drive everything else.

## 1 — Tools live in code, prompts live in config

Code defines tools (typed schemas, state writes, deferred loading). Spec/config
holds agent system prompts, tool descriptions, reminders, ToolSearch policy,
skill catalogs, explicit delegates, and permission rules.

Editing config takes effect on the **next run**. No restart, no redeploy, no schema migration. MCP servers refresh automatically via the `tools/list_changed` notification; on-disk skill packages refresh via a `PeriodicRefresher` you start once at bootstrap. The runtime re-resolves from the latest published config snapshot on each new run.

With audit and versioned-registry stores enabled, those edits are traceable
through record revisions and audit restore; published runtime snapshots are
immutable, and durable runs carry a `resolution_id` to reselect the same graph
for resume or replay.

## 2 — One config API, one admin console

`/v1/config/*` is the single mutation surface for agents, models, providers, model pools, MCP servers, skills, and plugin-backed policy sections. The bundled admin console is one consumer; your CI can be another.

What the console writes, the runtime reads. There is no separate ops project to maintain.

## 3 — Observability/eval/HITL are runtime modules

Started services can attach:

- OpenTelemetry GenAI traces on every phase, tool, and LLM call (`awaken-ext-observability`).
- A persistent trace store the admin console queries directly; trace HTTP routes are opt-in.
- An eval framework with fixture replay, scoring, and baseline diffing (`awaken-eval`).
- Permission-gated HITL via mailbox suspend/resume.

These are first-class runtime and server modules, not separate sidecars.

## Four capabilities that follow

The three rules combine to give four properties most agent frameworks lack:

- **Snapshot isolation + deterministic replay.** Each phase reads an immutable `Snapshot`, emits a `MutationBatch`; `commit` applies atomically. Saved snapshots replay byte-for-byte — debug, regression-test, or re-run eval over past traffic without re-paying LLM cost.
- **One backend, multiple protocol adapters.** One runtime serves AI SDK v6, AG-UI (CopilotKit), A2A, MCP HTTP, and ACP stdio from one process. Client protocol choice does not propagate to agent code.
- **Permission gates as runtime primitives.** `Gate` phase runs between tool decision and tool execution; `Allow` / `Deny` / `Ask` rules match on name + arguments; `Ask` suspends through mailbox and resumes when answered.
- **Generative UI as streamed primitive.** Agents emit A2UI / JSON Render / OpenUI Lang documents on the same event stream as text. Frontend renders without per-tool glue.

## Two programming modes

Awaken is useful as both a library and a service. Both modes use the same
`AgentRuntime`, `RunActivation`, `AgentSpec`, tools, plugins, and event stream;
the difference is who owns IO and configuration.

| Mode | How it runs | Use it when |
|---|---|---|
| In-process runtime | Your Rust process builds `AgentRuntime` with `AgentRuntimeBuilder`, registers tools/providers/plugins in code, and calls `runtime.run_to_completion(...)` or `runtime.run(..., EventSink)` directly. | CLI tools, local workers, tests, or application services that already own their IO boundary. |
| Server control plane | `awaken-server` stores an `Arc<AgentRuntime>`, queues work through mailbox-backed run dispatch, and exposes HTTP/SSE plus AI SDK, AG-UI, A2A, MCP, and ACP adapters. Normal `/v1/config/*` writes validate config, compile a candidate registry, and hot-swap the published snapshot for later runs. | Shared agent backends, browser frontends, managed providers/models/agents, auditability, HITL, eval, and operator control. |

In both modes, Rust code supplies executable capabilities (`Tool` impls,
plugins, provider factories, stores, backend factories); managed config supplies
agent behavior (prompts, tool description overrides, reminders, `model_id`, model
pools, allowed/excluded tools, plugin sections, MCP servers, skills, delegates,
permission rules). The admin console is the browser UI over server mode; it does
not replace the runtime. Server mode adds what a direct runtime caller otherwise
builds itself: HTTP/SSE, protocol adapters, mailbox dispatch, resumable
background runs, config publication, version restore, audit trails, and scoped
stores.

In-process mode is still a standard Tokio/`std` async library, **not** a
`no_std` or Tokio-free embedded target: `awaken-runtime` depends on Tokio for
timers, timeouts, and provider execution. The `*-contract` crates are `std`
type-surfaces only; MCP, Skills, Stores, Observability exporters, and Server are
the explicit IO layers.

## Crate map

| Crate | Description |
|-------|-------------|
| `awaken-runtime-contract` | Runtime-facing contracts: specs, tools, events, state, commit coordinator |
| `awaken-server-contract` | Server/store-facing contracts: queries, scoped stores, mailbox/outbox, staged commits |
| `awaken-runtime` | Phase loop, plugin system, agent loop, builder |
| `awaken-server` | HTTP/SSE gateway + protocol adapters |
| `awaken-stores` | Storage backends: memory, file, Postgres, SQLite mailbox |
| `awaken-tool-pattern` | Glob/regex tool matching for permission and reminder rules |
| `awaken-ext-permission` | Permission plugin (allow/deny/ask) |
| `awaken-ext-observability` | OpenTelemetry traces + metrics |
| `awaken-eval` | Fixture replay, scoring, and baseline diffing |
| `awaken-ext-mcp` | MCP client integration |
| `awaken-ext-skills` | Skill package discovery and activation |
| `awaken-ext-reminder` | Declarative reminder rules |
| `awaken-ext-generative-ui` | A2UI / JSON Render / OpenUI Lang |
| `awaken-ext-deferred-tools` | Deferred tool loading with probabilistic promotion |
| `awaken` | Facade crate re-exporting core modules |

## Reading path

1. [Get Started](/awaken/get-started/) → [First Agent](/awaken/tutorials/first-agent/).
2. [Develop Agents](/awaken/build-agents/) — implement tools, plugins, state, sub-agent calls, and UI streams in Rust.
3. [Tune & Operate](/awaken/operate/) — use the Admin Console or config API to manage prompts, models, MCP, skills, policies, traces, datasets, and evals.
4. [Serve & Integrate](/awaken/serve-and-integrate/) — AI SDK / CopilotKit / A2A / MCP / ACP clients.
5. [State & Storage](/awaken/state-and-storage/) — persistence and durable state.
6. [Design Philosophy](/awaken/explanation/philosophy/) — the "why" behind the three rules.
