# ADR-0016: Durable Cancellation Intent

- Status: Accepted
- Date: 2026-06-30
- Amended: 2026-07-21
- Depends on: ADR-0005, ADR-0009, ADR-0013
- Amended: 2026-10-09 (reconciled with ADR-0022: dead-letter and superseded cancellation follow the code)

## Context

An in-flight run can receive a cooperative cancellation signal, while a queued or
awaiting run has no live token. Cancellation nevertheless has one required semantic:
once accepted, a process crash must not lose it or allow the run to execute again.

The original implementation deleted a pending/awaiting dispatch and then committed
the terminal `Cancelled` fact. A crash between those writes left neither a dispatch
to reconcile nor committed terminal truth. Running cancellation had the dual gap:
the live signal was not represented in durable delivery state.

## Decision

### D1: Persist intent before signalling or committing

`DispatchQueue::cancel(run_id)` atomically sets `cancel_requested` on a pending,
awaiting, or running row and returns its thread id. It is idempotent and retains the
row and pending input. For a running row it advances the epoch and releases the old
lease, immediately fencing the former owner. Unknown, dead-lettered, superseded, or
completed runs return `None`.

The store still owns delivery state only. `cancel_requested` says what the worker
must deliver; the authoritative run outcome remains a committed runtime fact.

### D2: Cancellation uses the ordinary claim and fencing path

A pending or awaiting cancellation is claimable without external input. Cancelling
a running row revokes its epoch, makes the cancellation immediately claimable, and
also leaves the exact physical-attempt identity on the same row. The existing
exact-claim renewal observes that locked row as `CancellationRequested` only for
that former physical owner and epoch, then projects it into Runtime's existing
cooperative token. A wrong owner or epoch observes `Lost`; the new worker that
claims the durable cancellation remains `Current` and renews normally. If that process has
already crashed, no timeout is required. `Claimed::cancellation_requested` travels
over the same local/remote dispatch boundary as the request and lease.

The worker handles cancellation before model/provider execution, Session attachment,
or sandbox materialization, commits `Cancelled` through `Runtime::cancel_run` and
the claim's monotonic epoch fence, then settles `Done`. Settlement removes the
dispatch and pending input only after terminal commit succeeds. The one exception is
transport authority for an already-published authenticated A2A task: claim admission
freezes only the selected remote publication's exact `RemoteAgentAuthorization`
binding, and the existing pinned materializer may open it only for that claim's
`tasks/get` / `tasks/cancel` operation. Provider fallbacks, advisors, Session MCP,
and sandbox credentials are not part of a cancellation claim.

An anonymous or non-remote cancellation keeps an empty binding set. A missing,
revoked, expired, wrong-holder, wrong-purpose, or stale-revision/epoch binding fails
before remote I/O; the worker never retries anonymously and never treats that failure
as evidence that the remote task stopped. The durable intent and unconfirmed physical
responsibility remain available for an authorized owner to recover.

Cancellation eligibility deliberately bypasses the run's execution capability
requirements and replaceable placement ranking. A cold host resolver builds a
control-only worker from the thread commit boundary and dispatch fence; it does not
decode, adopt, create, or probe the bound sandbox. Placement remains authoritative
for execution, but cannot prevent terminal control of that execution.

### D3: Recovery is idempotent across both crash windows

- Crash after intent, before commit: a worker claims the retained intent.
- Crash after terminal commit, before settle: recovery reads terminal committed
  truth, does not append a contradictory terminal, and settles the retained row.
- Crash of a running owner: epoch revocation exposes the same retained intent
  immediately, without waiting for lease expiry.

Cancellation requests are prioritized over ordinary pending/wake work and are not
discarded by a later superseding submission on the thread.

### D4: Live control is an accelerator, not authority

`LiveRunControlService` records durable intent first. It then signals an active run;
a standalone ingress may immediately claim a queued/awaiting intent, while a served
deployment only wakes its already-running process pool and returns once the intent is
durable. The pool remains the sole claim driver, so an HTTP/control caller never
becomes a competing synchronous Worker. A live-only inline run without a dispatch
row can still be signalled, but durable ingress never reports cancellation success
after only an in-memory notification.

The Dispatch renewal loop is the sole live signal bridge for durable attempts.
There is no cancellation registry, additional polling loop, or A2A-specific
signal path. A remote cancel delivery acknowledgement is not a stop receipt;
the exact remote task must subsequently be observed terminal before its physical
slot can be released.

## Consequences

- Accepted durable cancellation survives process and worker replacement.
- There is one execution/commit/settle path for queued, awaiting, and recovered
  running cancellation; no cancellation reconciliation outbox or second terminal
  writer is introduced.
- Terminal control does not require model-provider, Session, or sandbox material.
  Observing or stopping an authenticated A2A task does require a worker that can
  realize its exact current remote-authorization binding; lack of that authority
  retains the cancellation intent instead of weakening authentication.
- SQLite, PostgreSQL, and the in-memory executable specification share the same
  cancellation-intent behavior and crash-window tests.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5, G6, G13, G31.
- ADR-0005 — committed `RunState` / `EndCause` authority.
- ADR-0013 — monotonic lease-epoch commit and settlement fence.
- ADR-0022 — cancellation intent is not superseded by newer queued work.

## Amendment 2026-10-09: reconciled with ADR-0022

D1's list of states that return `None` is out of date. The code is authoritative
(`crates/contract/awaken-dispatch-contract/src/dispatch_transition.rs:516-574`):

- A superseded run is not cancellable (`Superseded => NotCancellable`, :549). It
  has no committed terminal fact; it is visible only through `superseded()`.
- A dead-lettered run is cancellable. Cancel sets the cancellation bit, the row
  stays dead-lettered, and requeue is forbidden for good (:522, :573;
  `.../dispatch/queue.rs:341-356`). A terminal Session fence uses this.
