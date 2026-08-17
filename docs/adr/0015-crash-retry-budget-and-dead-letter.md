# ADR-0015: A Crash-Retry Budget and Committed Terminal Resolution

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009, ADR-0011

## Context

The daemon recovers a crashed run by reclaiming its expired lease and re-running
it (ADR-0011). Nothing bounded that: a *poison* run — one whose execution kills
the process every attempt — would be reclaimed on every poll, forever, across
restarts. That is the one unbounded loop left after crash-recovery and idempotent
consumption landed. The reference (`awaken-worktrees/goal`) bounds it with
`attempt_count` / `max_attempts` and a `DeadLetter` state.

## Decision

### D1: The budget counts only crash-recoveries, and a checkpoint refreshes it

The dispatch row carries `attempt_count`. It increments **only** on a recovery
re-claim (an expired-lease running row), never on a fresh claim or a normal
await/wake — so a long-lived run that legitimately awaits and wakes many times
never spends the budget. A successful `settle(Awaiting)` resets it to zero: a run
that reaches a checkpoint earned a fresh budget. So `attempt_count` is the count
of *consecutive crashes without progress*, which is exactly what a retry budget
should bound.

### D2: Retry exhaustion is a claimed terminal command

`claim_retry_exhausted(max_attempts, now)` atomically claims one strictly
expired `running` row whose `attempt_count >= max_attempts`. It advances the
ordinary lease epoch and bypasses execution placement and credentials because
the claim cannot execute the Run. The dispatch service routes that existing
`Claimed` value through the one `DispatchWorker` pre-execution terminal path,
which commits `Ended(Indeterminate)`, redelivers terminal observers, and applies
the ordinary fenced `Done` settlement and completion tombstone.

The store still owns only delivery and retry policy; it never writes or infers
Run outcome truth. If terminal commit or settlement fails, the row remains
leased. Once that lease strictly expires, the same command may claim it again
regardless of how far `attempt_count` has advanced. This makes terminalization
itself crash-recoverable without reopening execution.

The actual execution drainer checks for terminal-resolution work before every
ordinary claim: a standalone service, local process pool, or remote Worker pool
calls the same queue command and Worker terminalization method. Coordinator-only
maintenance does not compete for these claims. With no drainer the expired row
stays durable (never dead-lettered); after capacity returns, the first remote
Worker tick terminalizes it before claiming execution work.

### D3: Dead-letter is explicit operator quarantine only

`quarantine_retry_exhausted(max_attempts, now)` may move matching expired rows
to `DeadLetter` only when an operator explicitly asks to isolate them. It is not
called by a daemon, pool, or Worker claim route. `dead_letters()` lists these
manual quarantines and `requeue()` returns one to the queue at a fresh budget.
Quarantine is dispatch operations state, not a terminal Run outcome.

## Consequences

- A poison run commits `Ended(Indeterminate)` after `max_attempts`
  crash-recoveries instead of looping forever or disappearing into dispatch-only
  state; `DispatchServiceConfig.max_attempts` (default 5) tunes it.
- The budget is spent only by crashes, not by ordinary awaiting, so HITL or
  long-running runs are not penalised.
- Terminal resolution is claimed and epoch-fenced across memory, Postgres, and
  SQLite against one shared spec. A failed commit remains retryable through the
  same claim path.
- Dead-letter remains a manually requested held state with
  `dead_letters`/`requeue` operations; automatic services never create it.
- A run that *returns* a terminal error still settles `Done` immediately (no
  retry): the budget is for crashes that leave the dispatch unsettled, not for
  runs that fail cleanly.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5/G6/G35 (durable ingress and terminal truth).
- ADR-0011 — the recovery this bounds.
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  the dispatch failure/recovery boundary.
