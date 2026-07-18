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

### D3 — The ADR-0009 follow-ons are exposed and exercised at the server (slice E)

The four supplements — reconcile (ADR-0011), dead-letter GC (ADR-0015), scheduled
wake (ADR-0020), and epoch supersession (ADR-0022) — are implemented in
`awaken-run-ingress` and their deep state machine is verified deterministically
**on the server's exact concrete persistence stack**, `SqliteDispatchStore` +
`SqliteCommitCoordinator`, by `crates/awaken-run-ingress/tests/sqlite_dispatch.rs`
(`durable_loop_runs_entirely_on_sqlite`, `dead_letter_budget_on_sqlite`,
`dead_letter_ttl_gc_on_sqlite`, `scheduled_delivery_due_on_sqlite`,
`supersession_on_sqlite`, …). Slice E makes each **reachable and exercised through
the server**, each with its own e2e:

| ADR | Server feature | e2e |
| --- | --- | --- |
| ADR-0022 | `POST /v1/durable/threads/:t/supersede` — a newest-wins turn (`SharedHost::supersede_turn` → `submit_superseding`); `superseded` is observable | `managed_supersede_e2e` — an aawaiting run is superseded end to end |
| ADR-0020 | the `schedule` server mode: a gate defers tool calls as `ScheduledAction`s; the durable worker performs them out of band | `managed_scheduled_e2e` — write→read performed autonomously, no confirmation |
| ADR-0011 | `POST /v1/durable/threads/:t/reconcile` — reclaim runnable work | `managed_durable_ops_e2e` — verb wired + fails closed off-durable |
| ADR-0015 | `POST …/reap`, `GET …/dead-letters`, `POST …/dead-letters/purge` | `managed_durable_ops_e2e` — verbs wired + fail closed |

The operational verbs live in `durable_ops.rs`, mounted on every server; each
fails closed with 400 unless `AWAKEN_INGRESS=durable`. Supersession and scheduled
wake are demonstrated with their full behavior over HTTP. Reconcile and
dead-letter/reap/purge are exposed as operable, fail-closed verbs and their deep
crash/lease state machine stays covered by the store-level tests above — a crashed
`running` dispatch (an unsettled lease) cannot be produced without an actual
process crash mid-execution, so it is proven at the store level rather than via a
timing-dependent HTTP kill. A standing per-session reconciler **daemon**
(`DispatchService::spawn_with_wake`) remains a scoped follow-up: recovery is
available on demand via `reconcile`, and an always-on daemon is warranted once a
deployment emits scheduled actions autonomously or needs out-of-band GC without an
operator call.

## Consequences

- The run-ingress layer now has an in-process composition root: durable turns run
  through the dispatch queue end-to-end, proven by `e2e/managed_durable_e2e.mjs`
  (durable submit through the worker, dispatch DB on disk, cross-restart
  continuity, startup `recover`).
- Direct delivery remains the default; existing behavior and all foreground e2e
  are unchanged. Durable HITL await→resume works because committed truth is the
  authority; the awaiting dispatch row is settled by the foreground resume path (a
  benign no-daemon simplification — see the deferred reconciler surface above).
- Exposing operational verbs / an autonomous reconciler daemon is a scoped
  follow-up, to be taken only alongside a feature that emits scheduled actions or
  requires out-of-band GC/supersession.
