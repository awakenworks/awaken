# awaken-server-local — Architecture Overview

`awaken-server-local` is the single-machine **assembly binary**: it composes the
neutral runtime kernel, a sandbox / tool-relay layer, and a Managed Agents
protocol adapter into one process that serves the minimal Managed Agents *runtime*
surface.

**Scope: single-machine only.** This repository ships in-process implementations
only. Distribution (remote tool relays, remote sub-agents, multi-node dispatch, the
management plane) is implemented in **separate repositories** through the seams in
§5; those seams are traits plus forward-compatible data shapes, and a distributed
implementation must not require changes to the kernel (C) or the adapter (A). This
repository contains no remote / multi-node / management-plane code.

This document is a **navigation map**, not an owner. Each component's authority is
owned by the linked design doc or ADR; this page only shows how they assemble and
how a request flows through them. Components marked 🔨 are designed but not yet
built; ✅ exist today.

## 1. Layers

```text
 A  Managed protocol adapter   (anti-corruption; the only layer that speaks Anthropic)
 B  Dispatch / server          (RunIngress: direct or durable; resume delivery)
 C  Runtime kernel             (minimal; invokes RawTool by id; parks/resumes; commits)
 D  Extensions                 (RawTools, permission gate, goal)
 E  Sandbox / tool relay       (host composition: per-environment isolation + MCP relay)
 F  Stores                     (commit coordinator, dispatch/inbox)
```

The kernel (C) is the domain center. Everything about *where/how* a tool runs,
*which* protocol is spoken, and *when* work is scheduled lives above or beside it —
never inside it.

## 2. Components and responsibilities

Each row lists what the component owns, what it deliberately does **not** know, and
the doc that owns its authority.

### C — Runtime kernel (`awaken-runtime` / `awaken-runtime-contract`) ✅

| Component | Owns | Does **not** know | Owner |
|---|---|---|---|
| `Runtime` + loop | consume `RunnableConfig`, run model/tool steps, invoke `RawTool` by id, park/resume, commit, terminate | relay, binding, placement, scheduling, protocol, session, outcome | [ADR-0034](../adr/0034-runtime-axis-model-and-orthogonality.md), [tool-and-capability.md](tool-and-capability.md) |
| `RawTool` / `Tool` ports | the neutral tool call boundary; `Tool` is the typed authoring API, **erased** to `RawTool` before the kernel | *where/how* a call runs (encapsulated in the impl) | [tool-and-capability.md](tool-and-capability.md), [ADR-0007](../adr/0007-runtime-owns-tool-execution.md) |
| `PermissionGate` / `ToolGateHook` | authorization: allow / deny / **suspend** | concrete rules; why a suspend was requested | [permission-policy-axis.md](permission-policy-axis.md) |
| `WaitingTicket` / `ResumeCommand` / `validate_resume` | park correlation + fail-closed resume validation | who supplies the answer, or its meaning | [runtime-behavior.md](runtime-behavior.md) |
| `ResolvedSpec` / `RunResolver` | the model-visible decision surface + fingerprint gate | graph resolution (done at compile), provenance/pin | [config-to-run-execution-flow.md](config-to-run-execution-flow.md), [ADR-0034](../adr/0034-runtime-axis-model-and-orthogonality.md) |
| `RunActivation` / `RuntimeRunContext` | neutral run input / per-attempt live wiring | protocol DTOs, durable data, placement | [runtime-interface-boundaries.md](runtime-interface-boundaries.md) |
| `run` / `run_to_completion` / `execute` / `resume` | execution entries; `run_to_completion` drives park→decide→resume in-process | external scheduling (owned by ingress / coordinator) | [ADR-0033](../adr/0033-in-process-run-driver.md) |
| `CommitCoordinator` / `EventRecord` / `StreamSink` | the single durable write boundary, after-commit events, live stream | protocol projection, public event names | [commit-fact-projection-taxonomy.md](commit-fact-projection-taxonomy.md) |

The kernel's entire tool vocabulary is `RawTool::invoke(call) -> ToolOutput` by id.
A typed `Tool` (erased), a native `RawTool`, a **relay** `RawTool`, a
client-executed call (a suspending gate), and a delegated sub-run all collapse to
that one uniform boundary — which is precisely why the kernel is relay-, binding-,
placement-, and scheduling-agnostic ([ADR-0034](../adr/0034-runtime-axis-model-and-orthogonality.md) D6).

### D — Extensions

| Component | Owns | Status |
|---|---|---|
| `awaken-ext-builtin-tools` | `bash`/`read`/`write`/`edit`/`glob`/`grep` as in-process `RawTool`s | ✅ |
| relay `RawTool` | a `RawTool` whose `invoke` speaks MCP to this environment's relay; the kernel sees an ordinary tool | 🔨 |
| `awaken-ext-permission` | Claude-Code-style rules; a **client-executed tool is a suspending gate** | ✅ (client-tool gate 🔨) |
| `awaken-ext-goal` | `GoalSpec`, grader child-run, `GoalOutcome`; drives an **above-kernel** re-dispatch loop | 🔨 |

### E — Sandbox / tool relay (host composition; `awaken-sandbox-*`, `awaken-mcp-relay`) 🔨

| Component | Owns |
|---|---|
| `SandboxProvider` | build/tear down environments: `create(SandboxSpec) -> Environment`, `teardown(id)` |
| `Environment` (aggregate) | `IsolatedRoot` + relay address + `ToolAliasTable` + lifecycle |
| `IsolatedRoot` | resolve-under-root; escape fails closed (**the real isolation boundary**) |
| `ToolRelayServer` | a per-environment MCP server bound to one `IsolatedRoot` |
| `ToolExporter` | expose a native `RawTool` as an MCP tool (the inverse of an MCP client) |
| `ToolAliasTable` | `alias ⇄ mcp_name ⇄ native_id`; the presentation half feeds the descriptor, the routing half stays here |
| host composer | build each environment's relay `RawTool`s and compose them into that run's tool set |

Binding (native / relay / client-executed / delegation) is *which `RawTool` the
host composed*, plus a suspending gate — never a kernel field. Per-environment
isolation is a host composition choice (a per-environment tool set), not a kernel
feature.

### B — Dispatch / server (`awaken-run-ingress`) ✅

| Component | Owns |
|---|---|
| `RunIngress` (`Direct` / `Durable`) | synchronous vs durable delivery; durable supports park→later-resume |
| `LiveRunControl` | cancel / wake |
| `DispatchService` / worker | durable queue, lease, recovery |

### A — Managed adapter (`awaken-protocol-managed`, anti-corruption) 🔨

| Component | Owns |
|---|---|
| 3 endpoints (`events` POST / GET / stream) | the only layer that names Anthropic / `managed` vocabulary |
| inbound mapping | public event → `RunActivation` / `ResumeCommand` / `GoalSpec` / `LiveCommand` |
| outbound projection | committed `EventRecord` → public SSE event |
| public-id ledger | `evt_*` / `toolu_*` ⇄ neutral `correlation_id` |

Owned by [anthropic-alignment-and-sessions.md](anthropic-alignment-and-sessions.md)
and [protocol-adapter-boundaries.md](protocol-adapter-boundaries.md).

### F — Stores ✅

`SqliteCommitCoordinator` (commit + read ports), `RunDispatch` / `PendingInbox`
(durable delivery). Owned by the store ADRs.

## 3. End-to-end interaction flow

```text
① create session + environment
   client POST /v1/sessions          -> adapter mints a thread
   SandboxProvider::create(spec)      -> IsolatedRoot + ToolRelayServer(MCP up) + ToolAliasTable
   host composer builds relay RawTools (address+root baked in), composes them into the run
        (the kernel will see only "a set of RawTools" — never the relay)

② submit a message
   client POST .../events {user.message}
   adapter -> RunActivation (descriptors carry alias names) -> RunIngress
        · sync path:  run_to_completion(decide)
        · async HTTP: DirectRunIngress.submit -> may park

③ kernel loop (relay-agnostic)
   loop: infer -> model calls tool (by alias) -> PermissionGate decides
      ├ Allow -> execute_tool: runtime.tool(id).invoke(call)
      │          = relay RawTool -> MCP tools/call -> ToolRelayServer
      │          -> run native tool inside IsolatedRoot -> ToolOutput back
      ├ Ask (HITL) or client-executed -> gate returns Suspend
      │          -> commit WaitingTicket
      └ each step commits facts/events

④ HITL / custom tool (park -> answer -> resume)
   WaitingTicket -> adapter projects SSE:
      agent.tool_use (builtin awaiting confirm) / agent.custom_tool_use (client-executed)
   client POST .../events {user.tool_confirmation | user.custom_tool_result}
   adapter -> ResumeCommand::from_ticket(ticket, ResumeResult::{Decision|ToolResult}, now)
      -> validate_resume (six-field fail-closed) -> kernel continues

⑤ commit + projection
   CommitCoordinator commit -> EventRecord
   adapter project_event -> SSE: agent.message / agent.tool_* / session.status_idle{stop_reason}

⑥ outcome (above-kernel re-dispatch; no in-kernel guard)
   user.define_outcome -> GoalSpec (thread-scoped)
   a run terminates -> above-kernel coordinator reads committed facts -> grader child-run scores
      -> needs_revision: re-dispatch a round with feedback (bounded by max_iterations)
      -> satisfied/exhausted: project span.outcome_evaluation_* + status_idle

⑦ tear down
   session terminates -> SandboxProvider::teardown -> relay.shutdown + root cleanup
```

## 4. Key boundaries (the invariants this assembly rests on)

1. **The kernel is relay-, binding-, placement-, and scheduling-agnostic.** A relay
   is encapsulated inside a `RawTool`; binding is which `RawTool` the host composed;
   client-executed is a suspending gate. ([ADR-0034](../adr/0034-runtime-axis-model-and-orthogonality.md) D6,
   [ADR-0007](../adr/0007-runtime-owns-tool-execution.md).)
2. **The kernel sees only `RawTool`.** `Tool` is the typed authoring API, erased to
   `RawTool` before registration; a tool's type survives only as descriptor schema
   data. ([tool-and-capability.md](tool-and-capability.md).)
3. **Isolation is structural, not checked.** A `ToolRelayServer` is physically bound
   to one `IsolatedRoot`; crossing environments is impossible, not merely denied.
   Aliases are naming, never authorization.
4. **Events are projections.** Every public event is projected from a committed fact;
   the adapter owns public ids; the kernel emits only neutral facts.
   ([commit-fact-projection-taxonomy.md](commit-fact-projection-taxonomy.md),
   [protocol-adapter-boundaries.md](protocol-adapter-boundaries.md).)

## 5. Distributed extension seams (implemented in separate repositories)

Each local implementation is the degenerate case of a distributed one. This
repository provides only the **trait**, a forward-compatible **data shape**, and
the **in-process** implementation of each seam; the distributed column is built in
other repositories and plugs in without changing the kernel (C) or the adapter (A).

| Seam | Local (this repo) | Distributed (other repo) |
|---|---|---|
| `RawTool` (remote relay) | native + loopback relay tool | a `RawTool` that dials a remote sandbox daemon — still just a tool |
| `RunIngress` | `DirectRunIngress` + single-node `DurableRunIngress` | multi-node dispatch / mailbox |
| `WakeSignal` | local | NATS / cross-node |
| `SandboxProvider` / `ToolRelayServer` | `local_workdir` + loopback MCP | docker / kubernetes + remote relay daemon |
| `SandboxSpec` / `Environment` data | single workdir mount, loose constraints | optional `mounts` / `writeback` / `constraints` fully used |
| `ToolExecutor` (port kept unused) | not used | exactly-once effect / dedupe / correlation |
| `ContinuationGuard` / grader | local grader child-run | remote grader |
| sub-agent | in-process child run | remote brain over ACP (launcher + cell) |
| stores | sqlite | postgres + protocol replay log + outbox |
| management API | (out of scope) | a separate config-store CRUD plane |

## 6. Status and ownership

- **Built (✅):** the kernel (C), extensions' builtin tools + permission (D), ingress
  (B), and stores (F) exist and are tested.
- **To build (🔨):** the sandbox / tool-relay layer (E), the managed adapter (A), and
  `awaken-ext-goal`. Their authority will be owned by planned ADRs (a Managed-adapter
  ADR and a Sandbox/MCP-relay ADR); until those land, this page names the components
  but does not own their catalogs.
- This document owns nothing beyond the assembly map; it links to the owners above.
- **Boundary rule:** this repository ships only the in-process side of every §5
  seam and contains no remote / multi-node / management-plane code; distributed
  implementations live in separate repositories and must not require changes to the
  kernel (C) or the adapter (A).

## Guardrails

G1, G3, G4, G5, G9, G10, G18, G19, G21 in [INVARIANTS.md](../INVARIANTS.md), plus the
sandbox/relay guardrails (kernel relay-agnosticism; structural isolation) to be
registered with the Sandbox/MCP-relay ADR.
