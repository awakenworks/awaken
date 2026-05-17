---
title: "Introduction"
description: "Awaken — Rust agent runtime where the framework is itself the platform. Tools-first, live-tuned prompts, built-in tracing/eval/HITL."
---

**Awaken** is a production AI agent runtime written in Rust. The framework is the platform: when the server is up, tracing, replay, eval, permission gating, and an admin console are already running.

Three design rules drive everything else:

## 1 — Tools live in code, prompts live in config

Code defines tools (typed schemas, state writes, deferred loading). Spec/config holds agent system prompts, tool descriptions, reminders, skill catalogs, permission rules.

Editing config takes effect on the **next run**. No restart, no redeploy, no schema migration. MCP servers refresh automatically via the `tools/list_changed` notification; on-disk skill packages refresh via a `PeriodicRefresher` you start once at bootstrap. The runtime re-resolves from the latest published config snapshot on each new run.

## 2 — One config API, one admin console

`/v1/config/*` is the single source for agents, models, providers, plugins, MCP servers, skill packages, permissions, and trace history. The bundled admin console is one consumer; your CI can be another.

What the console writes, the runtime reads. There is no separate ops project to maintain.

## 3 — Observability/eval/HITL come with the server

Started services automatically expose:

- OpenTelemetry GenAI traces on every phase, tool, and LLM call (`awaken-ext-observability`).
- A persistent trace store the admin console queries directly.
- An eval framework with fixture replay, scoring, and baseline diffing (`awaken-eval`).
- Permission-gated HITL via mailbox suspend/resume.

These are not opt-in libraries. They are the runtime.

## Four capabilities that follow

The above three rules combine to give four properties most agent frameworks lack:

- **Snapshot isolation + deterministic replay.** Each phase reads an immutable `Snapshot`, emits a `MutationBatch`; `commit` applies atomically. Saved snapshots replay byte-for-byte — debug, regression-test, or re-run eval over past traffic without re-paying LLM cost.
- **One backend, four protocols.** Single runtime serves AI SDK v6, AG-UI (CopilotKit), A2A, and MCP HTTP from one process. Frontend choice does not propagate to agent code.
- **Permission gates as runtime primitives.** `Gate` phase runs between tool decision and tool execution; `Allow` / `Deny` / `Ask` rules match on name + arguments; `Ask` suspends through mailbox and resumes when answered.
- **Generative UI as streamed primitive.** Agents emit A2UI / JSON Render / OpenUI Lang documents on the same event stream as text. Frontend renders without per-tool glue.

## Crate map

| Crate | Description |
|-------|-------------|
| `awaken-contract` | Types, traits, state model, agent specs |
| `awaken-runtime` | Phase loop, plugin system, agent loop, builder |
| `awaken-server` | HTTP/SSE gateway + protocol adapters |
| `awaken-stores` | Storage backends: memory, file, Postgres, SQLite mailbox |
| `awaken-tool-pattern` | Glob/regex tool matching for permission and reminder rules |
| `awaken-ext-permission` | Permission plugin (allow/deny/ask) |
| `awaken-ext-observability` | OpenTelemetry traces + metrics |
| `awaken-ext-mcp` | MCP client integration |
| `awaken-ext-skills` | Skill package discovery and activation |
| `awaken-ext-reminder` | Declarative reminder rules |
| `awaken-ext-generative-ui` | A2UI / JSON Render / OpenUI Lang |
| `awaken-ext-deferred-tools` | Deferred tool loading with probabilistic promotion |
| `awaken` | Facade crate re-exporting core modules |

## Reading path

1. [Get Started](/get-started/) → [First Agent](/tutorials/first-agent/).
2. [Build Agents](/build-agents/) — tools, MCP, skills, reminders, HITL, UI.
3. [Serve & Integrate](/serve-and-integrate/) — AI SDK / CopilotKit / A2A / MCP frontends.
4. [State & Storage](/state-and-storage/), [Operate](/operate/) — production hardening.
5. [Design Philosophy](/explanation/philosophy/) — the "why" behind the three rules.
