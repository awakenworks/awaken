# ADR-0054: The Safe Loop Boundary is a Shared Kernel Seam — Live-Inbox Drain and Operator Pause Reach Every Executor, and Pause is a Durable Park

- Status: Proposed
- Date: 2026-07-13
- Builds on: [ADR-0040](0040-server-durable-ingress-integration.md) (durable
  dispatch / lease-recovery: park = persisted, resumable); the live-inbox
  "drain-at-boundary → commit" discipline (`awaken-runtime-contract::live_inbox`);
  the ACP `RunExecutor` / `Supervisor` turn model (`awaken-run-executor-acp`,
  `awaken-protocol-acp`).

## Context

Two operator-facing capabilities are asymmetric across execution paths, and the
asymmetry has one root cause.

**The safe loop boundary is a native-engine-only concept.** The native engine
folds queued live input into the next turn at a *safe boundary* — the point in
its loop where the current turn produced no tool calls and the run may either
continue, park, or end without violating the commit-at-boundary invariant. There,
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
all (and creates, but never sends on, an `Injection` channel — mid-turn control
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
both paths through it, and models pause in the *existing* park vocabulary rather
than as a new live state.

## Decision

### D1: The safe loop boundary is a shared kernel policy, not native-engine code

Introduce `awaken-runtime-contract::boundary` — the crate both the native engine
(`awaken-runtime`) and the ACP executor (`awaken-run-executor-acp`) already depend
on. It owns one decision function:

```rust
pub enum BoundaryOutcome {
    Continue { fold: Vec<Message> },              // fold these into the next turn
    Park { fold: Vec<Message>, reason: WaitingReason }, // commit these, then park
    Idle,                                         // no input, no pause → run-end guard
}

pub fn evaluate_boundary(
    ctx: &RuntimeRunContext, run_id: &RunId, transcript: &[Message],
) -> BoundaryOutcome;
```

Priority: **pause preempts queued input preempts idle.** The inbox is *always*
drained first (so no queued input is lost), and on a pause the drained messages
ride out with `Park { fold, .. }` to be committed before parking. The
re-identification helper (`{run_id}-inbox-{n}`) moves here from
`engine/mod.rs::drain_live_inbox`, so the discipline is defined **once**.

`evaluate_boundary` decides *and consumes the inbox*; it does **not** commit or
park — those are the caller's, because each executor has its own commit mechanism.
This side effect is the boundary's defined, deterministic semantic, documented as
such (it is not a pure query).

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
  drain→commit→continue; `Park(ManualPause)` is new; `Idle` falls through to the
  existing run-end guard. No behaviour change on the existing inbox path.
- **ACP executor**: `execute` gains a **turn-boundary loop**. After a turn
  commits, it calls `evaluate_boundary`; on `Continue` it commits the fold, makes
  it the next turn's prompt, and **relaunches the CLI** (ACP already relaunches
  per turn); on `Park` it commits and parks; on `Idle` it ends. This is the
  native `continue` loop, expressed for an opaque backend. Steer now works for
  external CLIs **by construction**, not by remembering to wire it twice.

The `Supervisor::Injection` channel is **not** used for steer: `Injection` is
mid-turn control (cancel); steer is "the next turn's input", which belongs at the
executor boundary, not inside a turn against an opaque CLI.

### D3: Pause is a durable park at the next safe boundary — never an in-flight freeze

Add `LiveCommand::Pause{run_id}` (breaks two exhaustive `match`es on `LiveCommand`
in `runtime.rs` — add the arm). Delivered to an **active** run, it sets a
`PauseSignal` on the context (modelled on the existing `cancellation`
`CancellationToken` seam; a new `Option` field on `RuntimeRunContext` via a
`.with_pause` builder — non-breaking). At the next safe boundary,
`evaluate_boundary` returns `Park { reason: WaitingReason::ManualPause }`; the
executor commits, writes a `ManualPause` park ticket, **releases the lease**, and
exits the loop. Fail-closed: delivered to a run that is not active → `NoSubscriber`.

**Reuse, don't invent.** `WaitingReason::ManualPause` **already exists** (a
reserved, currently-unused variant in `agent/waiting.rs`) — pause was pre-modelled
in the park vocabulary; we wire it, we do not add a variant. The `WaitingTicket`
data model already fits a pause: `pending_tool` and `call_id` are both `Option`,
so a ticket with `reason: ManualPause, pending_tool: None` is valid.

**ACP must gain parking (new capability, not a conflict).** `AcpRunExecutor::execute`
today only ever returns `Phase::Ended`; it has no waiting-ticket machinery (that
lives in the native engine). For an ACP run to honour `Park`, the executor needs a
no-tool `WaitingTicket` constructor and a `Phase::Waiting` return — the data model
supports it, but the code path is new.

There is **no suspended-but-alive state.** The in-flight loop, at a boundary, only
ever *commit-parks* or *commit-continues*. Because the runtime commits at every
boundary, a durable park loses nothing that matters; a frozen-in-memory run would
only add lease-holding fragility.

### D4: Resume acts on a parked run, through the durable-ingress port

A paused run is **parked** (not in the active registry), so Resume is a **durable
re-admission**, not a live signal: it re-enqueues the parked run and the dispatch
worker re-drives it (for ACP this is a fresh CLI launch continuing from committed
truth). **This is genuinely new machinery — `LiveCommand::Wake` cannot serve it.**
`Wake` is live-only (`live_control.rs`: `NotActive → NoSubscriber`), so it targets
an *active* run to nudge its boundary; a parked `ManualPause` run has no live
subscriber and would `NoSubscriber`. Resume therefore re-enqueues through the
durable ingress; a `ManualPause` park is one the worker does **not** auto-claim
(no pending input, no lease expiry), so it stays parked until an explicit Resume.
The operator-facing surface
still exposes a symmetric `pause` / `resume` verb pair; `LiveRunControlService`
**routes by aggregate state** — `Running` → signal, `Parked(ManualPause)` → durable
re-drive, otherwise fail-closed — the same live-vs-durable routing it already does
for cancel.

### D5: Neutrality

Everything here is mechanical: the boundary folds input, parks, or continues; a
`WaitingReason::ManualPause` is a park cause, not a governance verb. No product or
policy semantics enter the seam. The ACP CLI stays opaque — steer is injected only
as the next prompt and its content is never interpreted (anti-corruption).

## Consequences

- **Steer / redirect works for every execution path** — direct-native (already),
  durable-native (repaired by the D2 wiring), and external-CLI — because all reach
  the one boundary seam over a neutral inbox. The fix is broader than "ACP": it
  closes the durable path's silent inbox drop, which affected native runs too.
- **Operators get pause / resume** without a new live run state: the Run aggregate
  stays `{Running, Parked, Ended}`; pause is a `Running → Parked` transition at a
  boundary, preserving the commit-at-boundary and lease/fencing invariants.
- **One definition of the boundary discipline** (drain + re-identify + decide),
  shared — a change to the invariant changes one place.
- **Pause / resume are asymmetric by design** (live port vs durable port), because
  the two verbs act on different aggregate states. This can surprise a caller
  expecting two symmetric `LiveCommand`s; mitigated by the one operator-facing verb
  pair with state-aware routing, and documented here.
- **ACP persistent-session mode**: a pause→park→resume relaunches the CLI, losing
  any in-CLI ephemeral state (committed truth is intact). This is the inherent
  at-least-once / park-recovery residual, consistent with the rest of the runtime.
- `evaluate_boundary` is a decide-and-consume function, not a pure query — a small,
  documented tension with intent-revealing purity, accepted for cohesion.

## Alternatives considered

- **Wire the inbox drain into the ACP executor directly, duplicating the native
  logic.** Rejected: re-creates the "remember to do it in both places" gap that
  caused this ADR; the invariant would live twice.
- **Steer via the `Supervisor::Injection` channel (mid-turn).** Rejected: an opaque
  CLI cannot take a new prompt mid-turn; steer is next-turn input, an executor-
  boundary concern.
- **Pause as an in-flight freeze (suspended-but-alive).** Rejected: introduces a
  fourth live state, holds the lease indefinitely, and fights the fencing model;
  yields no benefit over a durable park given commit-at-boundary.
- **Symmetric `LiveCommand::Pause` + `LiveCommand::Resume` both on the active
  registry.** Rejected: a parked (paused) run is not active, so `Resume` on the
  active registry would require keeping it alive — the rejected freeze.

## Migration (slices, each independently green, no stubs)

1. **Kernel seam** — `boundary.rs` (`BoundaryOutcome`, `evaluate_boundary`), move
   the re-identify helper in, add `PauseSignal` + `ctx.pause` builder. (No
   `WaitingReason` change — `ManualPause` already exists.) Kernel unit tests:
   three-arm decision, drain-first priority, pause preemption.
2. **Durable-context wiring (prerequisite, fixes native-durable steer too)** —
   `request.rs::runtime_context` wires the host `LiveInbox` slot into
   `ctx.live_inbox`. Test: a durably-dispatched **native** run drains an offered
   External message at the boundary.
3. **Native convergence** — the engine boundary calls `evaluate_boundary` (pure
   refactor). Guarded by the existing live-inbox tests.
4. **U1 — ACP boundary loop** — `execute` loops over turns through the seam. New
   e2e: an External message offered to an ACP-fake run is delivered on the next
   turn; `acp_*` suites do not regress. (This is the operator-blocking slice.)
5. **U2 — Pause / Resume** — `LiveCommand::Pause` arm + `park(ManualPause)` + lease
   release; the ACP no-tool `WaitingTicket` constructor + `Phase::Waiting`; durable
   `resume` re-enqueue; fail-closed `NoSubscriber`; k3d: pause then resume on
   another node.
