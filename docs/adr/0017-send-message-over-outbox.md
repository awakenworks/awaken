# ADR-0017: Agent Messaging Is Owned by Thread State, Not the Dispatch Outbox

- Status: Accepted
- Date: 2026-06-30
- Revised: 2026-09-01
- Depends on: ADR-0007, ADR-0013, ADR-0021

## Context

The former generic `send_message` host adapter read a target Thread's current
`ResumeTicket` and then staged a separately durable `PendingInput` in the
Dispatch outbox. Managed coordination also reused that infrastructure for an
Agent follow-up even though it already owned a deterministic fresh Run.

Those two operations do not share an aggregate or transaction. The Thread may
resume, end, or advance to another Run between the read and the stage. A later
Worker correctly refuses the stale ticket, but the already-bound message can be
stranded instead of becoming input for the Thread's next Run. Dispatch cannot
repair this by deciding which Run is awaiting: Run and ticket truth belongs to
the committed Thread.

Static composition review found no production installation of the generic
adapter. Managed coordination exposes the single `send_message` command. Its
source Thread already persists the call, execution state, recovery policy, and
result in `ActiveToolBatch` through `ThreadCommit.state`; adding a Dispatch
outbox record duplicates that durable request authority.

## Decision

### D1: The source Thread owns the Agent-message request

`send_message` remains an ordinary durable tool operation. Its committed
`ActiveToolBatch` is the request/recovery authority and its committed tool result
is the acceptance receipt. Replay uses the existing deterministic operation and
child-Run identities. There is no Agent-message table, relationship registry,
or Dispatch outbox intent.

The unsupported `OutboxMessageSender` read-then-stage adapter is removed.
Dispatch exposes no `awaiting_run` query and never selects a target Run from a
Thread status cache.

### D2: The target fresh Run owns the accepted message

Both spawn and follow-up freeze exactly one user message in the target
`RunActivation.input`. The deterministic `RunDispatch` carries that complete
activation through `enqueue_session_child`; backend Run-id idempotency rejects a
same-id/different-payload collision. When the Worker commits, the message becomes
ordinary target-Thread transcript truth through `ThreadCommit.messages`.

Dispatch therefore owns only delivery, claim, lease, retry, and settlement of
the already-frozen Run. It does not own Agent-message acceptance and does not
write a parallel `PendingInput`.

### D3: Keep external pending input separate

ADR-0021's unbound pending-input representation remains an ingress primitive for
input already accepted by an external application before a Run exists. It is
not the internal Agent messaging mechanism. A future generic model-visible
`send_message` implementation must be a Thread-scoped `StateCommand`/reducer
with commit-coupled recovery, following the same ownership shape as
`ActiveToolBatch`; it must not recreate the removed server adapter.

## Consequences

- Internal Agent messaging has one durable request owner: source Thread state.
- The target message has one content owner: the deterministic fresh activation,
  then the target Thread transcript after commit.
- Spawn and follow-up use one admission path and one idempotency boundary.
- Dispatch cannot resume an unrelated Awaiting Run or strand input on a stale
  ticket because it never performs that classification.
- External pending input and internal Agent coordination remain distinct
  concepts instead of sharing storage merely because both contain text.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1/G13 committed truth and one authority.
- ADR-0007 — builtin-tool extension ports.
- ADR-0013 — outbox and pending-input delivery.
- ADR-0021 — unbound Thread inbox semantics.
