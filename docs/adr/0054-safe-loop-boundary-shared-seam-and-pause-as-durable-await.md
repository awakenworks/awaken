# ADR-0054: The Safe Loop Boundary is a Shared Kernel Seam — Live-Inbox Drain and Operator Pause Reach Every Executor, and Pause is a Durable Await

- Status: Accepted
- Date: 2026-07-13
- Implemented: 2026-07-22 — the neutral boundary and `PauseSignal` live in
  `awaken-runtime-contract`; native and ACP attempts both evaluate it; durable
  worker contexts carry the live inbox and pause signal; manual pause and ACP
  tool permission commit ordinary resume tickets and re-enter through durable
  ingress. Unit, worker-recovery, and served-process E2E scenarios cover the
  fresh, pause, permission, resume, replacement, and stale-ticket paths.
- Amended: 2026-09-01 — Runtime `ActiveAttemptScope` is the sole LiveInbox
  lifecycle owner. It creates one fresh inbox only for an executor declaring
  `LiveInput::SafeBoundary`, registers it with the exact attempt generation, and
  closes it on return. Session and Worker contexts no longer retain or carry inbox
  entries across attempts; unconsumed live input is explicitly best-effort.
- Builds on: [ADR-0040](0040-server-durable-ingress-integration.md) (durable
  dispatch / lease-recovery: await = persisted, resumable); the live-inbox
  "drain-at-boundary → commit" discipline (`awaken-runtime-contract::live_inbox`);
  the ACP `RunExecutor` / `Supervisor` Run model (`awaken-run-executor-acp`,
  `awaken-protocol-acp`).

## Context

Two operator-facing capabilities are asymmetric across execution paths, and the
asymmetry has one root cause.

**The safe loop boundary is a native-engine-only concept.** The native engine
folds queued live input into the next Step at a *safe boundary* — the point in
its loop where the current Step produced no tool calls and the Run may either
continue, await, or end without violating the commit-at-boundary invariant. There,
and only there, it drains the run's `LiveInbox`, re-identifies each message with
the `{run_id}-inbox-{n}` id discipline, commits it into the transcript, and loops
(`awaken-runtime/src/engine/mod.rs`: boundary at the `calls.is_empty()` branch,
`drain_live_inbox`). The `drain_at_boundary()` API has exactly **one** consumer.

**LiveInbox is already backend-agnostic — the coupling is only in the wiring.**
`LiveInbox` lives in the neutral kernel (`awaken-runtime-contract::live_inbox`)
and imports only `Message`; it names no protocol, wire, or backend type.
`LiveInboxMessage { id, origin, message: Message }` carries a neutral `Message`,
and `MessageOrigin { Run, External }` is a pure provenance tag the runtime "takes
no position on". So a boundary drain that folds neutral messages is inherently
protocol-agnostic — which is precisely why one shared seam can serve every
executor, and why the gap below is a *missing wire*, not a protocol mismatch.

**The gap is broader than ACP — the whole durable path drops the inbox
(verified).** `RuntimeRunContext.live_inbox` is an `Option`, and the durable
execution context builder `request.rs::runtime_context` (used by the dispatch
worker for **every** durably-dispatched run — native *and* ACP) wires `reader`,
`stream_sink`, and `stream_checkpoint` but **never `live_inbox`**, so it defaults
to `None`. LiveInbox steer therefore works **only** on the direct-ingress native
path (`host.context()` wires the slot); it silently has no effect on any run the
worker drives. The ACP / external-CLI executor is doubly blind: `AcpRunExecutor::execute`
also performs a **single** `Supervisor::supervise` and touches the inbox not at
all (and creates, but never sends on, an `Injection` channel — mid-Run control
that cannot deliver a *prompt* into an opaque CLI).

**There is no operator pause.** `LiveCommand` has only `Cancel{run_id}` and
`Wake{run_id, reason}` (`awaken-runtime-contract/src/control.rs`). An operator
cannot suspend an in-flight run and resume it later. Naively adding an in-flight
"freeze" (keep the run alive in memory, holding its lease, suspended) would
introduce a *fourth* live run state that the lease / recovery / fencing model does
not have — and a lease-holding-but-frozen owner is exactly the slow-but-alive
anti-pattern the durable-dispatch fencing work exists to prevent.

Both problems act at the **same place** (the safe boundary), and the ACP path is
blind to that place. This ADR makes the boundary a first-class shared seam, routes
both paths through it, and models pause in the *existing* await vocabulary rather
than as a new live state.

## Decision

### D1: The safe loop boundary is a shared kernel policy, not native-engine code

Introduce `awaken-runtime-contract::boundary` — the crate both the native engine
(`awaken-runtime`) and the ACP executor (`awaken-run-executor-acp`) already depend
on. It owns one decision function:

```rust
pub enum BoundaryOutcome {
    Continue { fold: Vec<Message> },              // fold these into the next Step
    Await { fold: Vec<Message>, reason: AwaitReason }, // commit these, then await
    Idle,                                         // no input, no pause → run-end guard
}

pub fn evaluate_boundary(
    ctx: &RuntimeRunContext, run_id: &RunId, transcript: &[Message],
) -> BoundaryOutcome;
```

Priority: **pause preempts queued input preempts idle.** The inbox is *always*
drained first, and on a pause the drained messages ride out with
`Await { fold, .. }` to be committed before awaiting. A successful caller commit
therefore omits no queued input. `LiveInbox` remains explicitly process-local and
best-effort: a caller commit failure after the drain may lose that attempt-local
fold, and lossless delivery must use the existing durable Session event ingress.
The
re-identification helper (`{run_id}-inbox-{n}`) moves here from
`engine/mod.rs::drain_live_inbox`, so the discipline is defined **once**.

`evaluate_boundary` decides *and consumes the inbox*; it does **not** commit or
await — those are the caller's, because each executor has its own commit mechanism.
This side effect is the boundary's defined, deterministic semantic, documented as
such (it is not a pure query). It must not grow an acknowledgement store, remote
relay, or retry queue beside durable ingress merely to strengthen this best-effort
handle.

### D2: The durable execution context wires the inbox, and both executors route through the seam

**Prerequisite (broader than ACP).** `request.rs::runtime_context` (the durable
worker's context builder) must wire the host's `LiveInbox` slot into
`ctx.live_inbox`, exactly as the direct path does in `host.context()`. Until it
does, the boundary seam has nothing to drain on the durable path — so this fix
lands *first*, and it repairs steer for durably-dispatched **native** runs too,
not only ACP. Because `LiveInbox` is neutral (Context), this is pure plumbing —
no protocol type crosses into the worker.

- **Native engine**: the `calls.is_empty()` branch calls `evaluate_boundary` and
  handles the three arms. `Continue` is behaviour-identical to today's
  drain→commit→continue; `Await(ManualPause)` is new; `Idle` falls through to the
  existing run-end guard. No behaviour change on the existing inbox path.
- **ACP executor**: `execute` gains a **Run-boundary loop**. After a Run
  commits, it calls `evaluate_boundary`; on `Continue` it commits the fold, makes
  it the next Run's prompt, and **relaunches the CLI** (ACP already relaunches
  per Run); on `Await` it commits and awaits; on `Idle` it ends. This is the
  native `continue` loop, expressed for an opaque backend. Steer now works for
  external CLIs **by construction**, not by remembering to wire it twice.

The `Supervisor::Injection` channel is **not** used for steer: `Injection` is
mid-Run control (cancel); steer is "the next Run's input", which belongs at the
executor boundary, not inside a Run against an opaque CLI.

The inbox remains a **process-local attempt handle**. Wiring it into a durable
worker context means that a locally hosted durable attempt reaches the same
boundary; it does not make the handle remotely addressable. A Coordinator that
has delegated execution to another Worker must therefore report the live inbox
as inactive instead of accepting input into an unreachable local queue. The
existing Session event ingress is the durable fallback: it owns the message,
persists it once, and lets durable ingress schedule its consumption. Adding a
second remote live-inbox relay or another durable steer queue is forbidden.

### D3: Pause is a durable await at the next safe boundary — never an in-flight freeze

Add `LiveCommand::Pause{run_id}` (breaks two exhaustive `match`es on `LiveCommand`
in `runtime.rs` — add the arm). Delivered to an **active** run, it sets a
`PauseSignal` on the context (modelled on the existing `cancellation`
`CancellationToken` seam; a new `Option` field on `RuntimeRunContext` via a
`.with_pause` builder — non-breaking). At the next safe boundary,
`evaluate_boundary` returns `Await { reason: AwaitReason::ManualPause }`; the
executor commits, writes a `ManualPause` await ticket, **releases the lease**, and
exits the loop. Fail-closed: delivered to a run that is not active → `NoSubscriber`.

**Reuse, don't invent.** `AwaitReason::ManualPause` **already exists** (a
reserved, currently-unused variant in `agent/awaiting.rs`) — pause was pre-modelled
in the await vocabulary; we wire it, we do not add a variant. The closed
`ResumeTicket` data model already fits a pause explicitly through
`AwaitTarget::Pause(PauseReason::Manual)`; no call or tool payload can be attached.

**ACP must gain awaiting (new capability, not a conflict).** `AcpRunExecutor::execute`
today only ever returns `RunState::Ended`; it has no resume-ticket machinery (that
lives in the native engine). For an ACP run to honour `Await`, the executor needs a
closed pause-target `ResumeTicket` constructor and a `RunState::Awaiting` return —
the data model supports it, but the code path is new.

There is **no suspended-but-alive state.** The in-flight loop, at a boundary, only
ever *commit-awaits* or *commit-continues*. Because the runtime commits at every
boundary, a durable await loses nothing that matters; a frozen-in-memory run would
only add lease-holding fragility.

### D4: Resume acts on an aawaiting run, through the durable-ingress port

A paused run is **awaiting** (not in the active registry), so Resume is a **durable
re-admission**, not a live signal: it re-enqueues the aawaiting run and the dispatch
worker re-drives it (for ACP this is a fresh CLI launch continuing from committed
truth). **This is genuinely new machinery — `LiveCommand::Wake` cannot serve it.**
`Wake` is live-only (`live_control.rs`: `NotActive → NoSubscriber`), so it targets
an *active* run to nudge its boundary; an awaiting `ManualPause` run has no live
subscriber and would `NoSubscriber`. Resume therefore re-enqueues through the
durable ingress; a `ManualPause` await is one the worker does **not** auto-claim
(no pending input, no lease expiry), so it stays awaiting until an explicit Resume.
The operator-facing surface
still exposes a symmetric `pause` / `resume` verb pair; `LiveRunControlService`
**routes by aggregate state** — `Running` → signal, `Awaiting(ManualPause)` → durable
re-drive, otherwise fail-closed — the same live-vs-durable routing it already does
for cancel.

### D5: Neutrality

Everything here is mechanical: the boundary folds input, awaits, or continues; a
`AwaitReason::ManualPause` is an await cause, not a governance verb. No product or
policy semantics enter the seam. The ACP CLI stays opaque — steer is injected only
as the next prompt and its content is never interpreted (anti-corruption).

### D6: ACP tool permission uses the same durable wait, not a held process

An ACP `session/request_permission` evaluated as `RequireConfirmation` is projected
to `AwaitReason::ToolPermission` with the policy correlation, ACP tool-call id, and
neutral `PendingTool`. The driver answers the opaque process's outstanding request
as cancelled, reaps that process, commits the ticket and negotiated ACP session id,
and releases the dispatch lease. It never holds a process or lease while waiting for
a person.

Resume validates the ordinary `ResumeCommand` against that committed ticket. A
replacement executor loads the durable ACP session id and sends one explicit
continuation invocation; a one-shot resolver applies the decision only to the exact held
tool-call id. Any different or later request returns to current policy. This is the
same at-least-once recovery posture as manual pause: opaque in-process ephemera is
not durable, while correlation, authority, transcript, and session identity are.

## Consequences

- **Steer / redirect works for every locally reachable execution path** —
  direct-native, locally hosted durable-native, and locally hosted external-CLI —
  because all reach the one boundary seam over a neutral inbox. A remote Worker
  is not reachable through this process-local API; the adapter returns inactive
  and callers use durable Session event ingress without losing input.
- **Operators get pause / resume** without a new live run state: the Run aggregate
  stays `{Running, Awaiting, Ended}`; pause is a `Running → Awaiting` transition at a
  boundary, preserving the commit-at-boundary and lease/fencing invariants.
- **One definition of the boundary discipline** (drain + re-identify + decide),
  shared — a change to the invariant changes one place.
- **Pause / resume are asymmetric by design** (live port vs durable port), because
  the two verbs act on different aggregate states. This can surprise a caller
  expecting two symmetric `LiveCommand`s; mitigated by the one operator-facing verb
  pair with state-aware routing, and documented here.
- **ACP persistent-session mode**: a pause→await→resume relaunches the CLI, losing
  any in-CLI ephemeral state (committed truth is intact). This is the inherent
  at-least-once / await-recovery residual, consistent with the rest of the runtime.
- **ACP permission HITL is durable**: policy `Ask` no longer collapses to denial;
  process/worker replacement consumes the same committed `ResumeTicket`, with the
  decision scoped to the exact tool-call id.
- `evaluate_boundary` is a decide-and-consume function, not a pure query — a small,
  documented tension with intent-revealing purity, accepted for cohesion.

## Alternatives considered

- **Wire the inbox drain into the ACP executor directly, duplicating the native
  logic.** Rejected: re-creates the "remember to do it in both places" gap that
  caused this ADR; the invariant would live twice.
- **Steer via the `Supervisor::Injection` channel (mid-Run).** Rejected: an opaque
  CLI cannot take a new prompt mid-Run; steer is next-Run input, an executor-
  boundary concern.
- **Pause as an in-flight freeze (suspended-but-alive).** Rejected: introduces a
  fourth live state, holds the lease indefinitely, and fights the fencing model;
  yields no benefit over a durable await given commit-at-boundary.
- **Symmetric `LiveCommand::Pause` + `LiveCommand::Resume` both on the active
  registry.** Rejected: an awaiting (paused) run is not active, so `Resume` on the
  active registry would require keeping it alive — the rejected freeze.

## Migration (slices, each independently green, no stubs)

1. **Kernel seam** — `boundary.rs` (`BoundaryOutcome`, `evaluate_boundary`), move
   the re-identify helper in, add `PauseSignal` + `ctx.pause` builder. (No
   `AwaitReason` change — `ManualPause` already exists.) Kernel unit tests:
   three-arm decision, drain-first priority, pause preemption.
2. **Durable-context wiring (prerequisite, fixes native-durable steer too)** —
   `request.rs::runtime_context` wires the host `LiveInbox` slot into
   `ctx.live_inbox`. Test: a durably-dispatched **native** run drains an offered
   External message at the boundary.
3. **Native convergence** — the engine boundary calls `evaluate_boundary` (pure
   refactor). Guarded by the existing live-inbox tests.
4. **U1 — ACP boundary loop** — `execute` loops over Runs through the seam. New
   e2e: an External message offered to an ACP-fake run is delivered on the next
   Run; `acp_*` suites do not regress. (This is the operator-blocking slice.)
5. **U2 — Pause / Resume** — `LiveCommand::Pause` arm + `await(ManualPause)` + lease
   release; the ACP closed pause-target `ResumeTicket` constructor +
   `RunState::Awaiting`; durable `resume` re-enqueue; fail-closed `NoSubscriber`;
   k3d: pause then resume on another node.
