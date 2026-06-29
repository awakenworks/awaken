# Neutral Waist - Runtime Execution Ports

This doc covers the runtime-side execution boundary. It is not a new crate plan.
Use the existing runtime vocabulary first.

## Owning Context

| Item | Owner |
|---|---|
| Agent loop, typed state/tools, in-process tool execution, commit boundary | Runtime Core |
| Backend dispatch, durable buffering, routes | Dispatch / Server |
| Product event names and public DTOs | Product adapters |
| Credential mechanics and any future remote-agent execution | Product / future ADR (out of scope here) |

## Core Ports

| Port | Purpose | Rule |
|---|---|---|
| `AgentRuntime` | Execute a prepared run activation | Runtime entrypoint, not an HTTP/server facade |
| `RunActivation` / `RuntimeRunContext` | Immutable run input plus per-attempt live wiring | No product DTOs in either; no process-local handles in activation |
| `RunExecutor` / `LiveRunControl` | Narrow execution and live steering role views | Split execution from cancel/decision/wake authority |
| `RunResolver` / `CommitCoordinatorSource` | Resolve scoped plans and expose durable commit wiring | Resolution and commit ownership stay out of the execution role |
| `ToolExecutor` | Invoke the selected tool in-process through a neutral call/result port | The runtime invokes the tool by id; the port carries no authorization |
| `StreamSink` | Stream live runtime progress to callers | Facts still commit through `CommitCoordinator` |
| `EventReader` / `EventSubscriber` | Read or subscribe to committed durable event records | Cannot create or erase runtime truth |
| `ContinuationGuard` | Decide whether a natural-end run should continue | Async, replayable verdicts; no product outcome semantics |

These ports are the "waist": server and product code can adapt to them, but the
runtime core must not import server routes, public protocol names, or execution
drivers.

For the exact role split and the tool/plugin decision ladders, use
[runtime-interface-boundaries.md](runtime-interface-boundaries.md).

## Data-Only Config Edge

The config domain prepares serializable data:

```text
ResolvedSpec + catalog fingerprint -> runtime catalog validation -> live ResolvedRun
```

No live registry, factory object, pin id, scope object, or product DTO crosses
this edge. The runtime builds live execution objects from its own catalog and
fails closed if the fingerprint does not match.

This is the simple design rule that keeps in-process, server-hosted, and future
out-of-process execution on the same path.

## Backend Requirements

Backends advertise `BackendProfile`. Before a run starts, requirements derived
from the activation are checked against the profile:

- continuation support;
- decision/HITL support;
- frontend tool capability;
- protocol-specific endpoint support, after adapter translation.

Unsupported capability is a typed pre-execution failure, not a late runtime
surprise.

## Goal Continuation

Goal evaluation uses the runtime extension seam:

1. `ContinuationGuard` evaluates at the natural end of a run.
2. The guard returns a structured opaque verdict.
3. Replay reuses recorded verdicts and never re-grades.
4. `awaken-ext-goal` owns `GoalSpec`, grading, and classifications.
5. Product adapters map Anthropic Outcome or other public concepts onto the
   extension.

The runtime records "this run concluded with code/detail"; it does not know what
"satisfied" or "needs revision" means.

## Direct Development Guidance

When adding execution behavior:

1. Prefer an existing port in the table above.
2. Put protocol translation in an adapter before calling runtime code.
3. Keep public error/event names out of runtime errors.
4. Add a `BackendProfile` requirement if a backend capability matters.
5. Add a replay test if the behavior affects run termination or continuation.

## Non-Goals

- No new execution-transport framework before a future ADR introduces
  remote-agent execution.
- No product status names in runtime events.
- No server route state in `RunActivation`.
- No cancellation channels, stream sinks, or commit coordinator handles in
  `RunActivation`; those belong in `RuntimeRunContext`.
- No authorization grant implied by tool location, backend capability, or catalog
  visibility.

## Guardrails

G2, G3, G4, G10, and G11 in [INVARIANTS](../INVARIANTS.md).
