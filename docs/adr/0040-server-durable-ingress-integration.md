# ADR-0040: Server Durable Ingress Integration and the ADR-0009 Supplement Disposition

- Status: Accepted
- Date: 2026-07-03
- Depends on: ADR-0009, ADR-0011, ADR-0015, ADR-0020, ADR-0022, ADR-0039

## Context

ADR-0009 shipped `DurableRunIngress` (the durable half of the `RunIngress` seam,
G5) inside `awaken-run-ingress`, and ADR-0011/0015/0020/0022 supplemented it with
an autonomous dispatch service, a crash-retry budget with dead-lettering,
scheduled actions, and epoch supersession. Until now none of this was reachable
from `awaken-server-local`: the server executed every turn by calling
`Runtime::start_turn` directly, so the run-ingress layer was live code with no
in-process composition root exercising it.

This ADR records how the local server was wired onto that layer (the integration
slices) and — importantly — what the four ADR-0009 follow-ons do and do **not**
require at the server level.

## Decision

### D1 — The turn path goes through the `RunIngress` seam (slice C)

`SharedHost` no longer calls `Runtime::start_turn`. A `SessionCtx` now holds
`runtime: Arc<Runtime>` plus an `ingress: Arc<dyn RunIngress>`, and a turn runs as
`runtime.prepare(...)` (install catalog + register snapshot + build the
activation) followed by `ingress.submit(activation, ctx)`. The default ingress is
`DirectRunIngress`, which executes inline on the same `runtime` — so this is
behavior-preserving (all foreground e2e unchanged). This makes the delivery seam,
not the runtime call, the single place a turn is dispatched, which is what lets
the durable half swap in without touching the turn path.

### D2 — `AWAKEN_INGRESS=durable` selects durable delivery (slice D)

When `AWAKEN_INGRESS=durable`, `SharedHost::build_ingress` constructs a
`DurableRunIngress` over a `SqliteDispatchStore` (a per-thread `*-dispatch.db`
under the store dir, or an in-memory queue when no store dir is set), sharing this
thread's `runtime` and `commit` (G6: one runtime, one commit boundary — the
dispatch layer adds durability, not a second commit mechanism). A durable turn is
delivered through `submit_background` (persist the accepted run to the queue,
then drive it via the dispatch worker), so the whole run-ingress path — enqueue →
claim → lease → worker execute → commit relay — runs on a normal turn. On session
(re)build the ingress runs `recover` for startup reconciliation.

**Restart-unique run ids.** The runtime's in-process id counter resets to 1 on a
process restart. The direct path tolerates a colliding id (it executes
unconditionally), but the dispatch worker's terminal-run guard ("never re-run a
committed, finished run") would mistake a fresh `run-1` for the already-committed
`run-1` and silently drop the turn. So the durable path stamps each activation
with a wall-clock + sequence run id, which cannot collide across restarts. This
is the concrete instance of the ADR-0009 note that "the durable path supplies
explicit, stable ids" rather than the ergonomic in-process counter.

### D3 — The ADR-0009 follow-ons need no new server code (slice E)

The four supplements — autonomous service/reconciler (ADR-0011), dead-letter GC
(ADR-0015), scheduled wake (ADR-0020), and epoch supersession (ADR-0022) — are
implemented in `awaken-run-ingress` and are already verified **on the server's
exact concrete persistence stack**, `SqliteDispatchStore` + `SqliteCommitCoordinator`,
by `crates/awaken-run-ingress/tests/sqlite_dispatch.rs`:

| ADR-0009 follow-on | Behavior | Test on the server's SQLite stack |
| --- | --- | --- |
| ADR-0011 | reconcile / recover an enqueued run; survive a fresh store handle | `durable_loop_runs_entirely_on_sqlite`, `sqlite_dispatch_opens_a_file_and_persists`, `renew_owned_leases_on_sqlite` |
| ADR-0015 | dead-letter after the retry budget; time-windowed GC | `dead_letter_budget_on_sqlite`, `dead_letter_ttl_gc_on_sqlite` |
| ADR-0020 | perform a scheduled action when due | `scheduled_delivery_due_on_sqlite` |
| ADR-0022 | newest submission supersedes stale pending work | `supersession_on_sqlite` |

Because the durable server (D2) composes exactly this stack, these are covered
without duplicating them as server-local tests. What is *not* yet built is an
operational **surface** for them — an HTTP verb to purge dead-letters, submit a
superseding run, or a per-session reconciler daemon lifecycle. That is deferred
deliberately: these are background/operational actions, not synchronous
request/response turns, so an HTTP shape for them is a product decision, not a
correctness gap. Spawning a per-session `DispatchService` daemon was also declined
for now — with no server model that emits scheduled actions, the daemon would add
a background task and lifecycle risk per session with no behavior the synchronous
`submit_background` drain does not already produce.

## Consequences

- The run-ingress layer now has an in-process composition root: durable turns run
  through the dispatch queue end-to-end, proven by `e2e/managed_durable_e2e.mjs`
  (durable submit through the worker, dispatch DB on disk, cross-restart
  continuity, startup `recover`).
- Direct delivery remains the default; existing behavior and all foreground e2e
  are unchanged. Durable HITL park→resume works because committed truth is the
  authority; the parked dispatch row is settled by the foreground resume path (a
  benign no-daemon simplification — see the deferred reconciler surface above).
- Exposing operational verbs / an autonomous reconciler daemon is a scoped
  follow-up, to be taken only alongside a feature that emits scheduled actions or
  requires out-of-band GC/supersession.
