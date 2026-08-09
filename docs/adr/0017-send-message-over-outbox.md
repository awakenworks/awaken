# ADR-0017: send_message Backed by the Outbox, Addressed by Thread

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0007, ADR-0013

## Context

The `send_message` builtin tool (ADR-0007) lets an agent message another part of
the multi-agent system, over a neutral `MessageSender` port the host injects. The
durable substrate for cross-thread delivery — the outbox and relay — landed in
ADR-0013. What was missing was the host adapter joining the two, and the right
addressing unit.

## Decision

### D1: Address a thread, not a run

A message targets a **thread id**, not a run id. A run is one ephemeral execution
attempt; it is not a stable address. A thread is the durable, addressable unit and
the consistency/shard key of the whole delivery design (`thread_id` is "the shard
and consistency key"). The tool argument is `target_thread`; the adapter resolves
the run currently awaiting on that thread.

### D2: A host adapter bridges the tool port to the outbox

`OutboxMessageSender` implements the extension's `MessageSender`. On `send`, it
asks the dispatch store for the thread's aawaiting run (`awaiting_run(thread_id)`),
reads that run's committed resume ticket for its correlation, and stages a
`PendingInput` (the message as `ResumeResult::Input`) into the outbox. The daemon
then relays it to the run's pending input, which resumes the run with the message.
Delivery to a thread with no awaiting run fails closed.

### D3: Idle-thread delivery is the same outbox path

ADR-0021 completed the formerly deferred idle case without adding a parallel
delivery mechanism. The adapter stages a ticket-bound input when a run is
awaiting, otherwise an unbound input addressed only by thread; both use the same
outbox, relay, pending store, and settle-on-commit lifecycle.

### D4: The caller key is optional; durable identity is never optional

`idempotency_key` is an optional tool argument. When supplied, it identifies one
logical message within the sending Run, so distinct tool operations may
intentionally converge. When omitted (or blank), the runtime-owned durable tool
`operation_id` identifies the message. In both cases the source Run scopes the
identity; the same caller key in two Runs is not the same message.

The adapter fingerprints that identity into `message_id`. It deliberately does
not fingerprint content: an exact retry is a no-op, while reuse of one identity
with changed target, correlation, schedule, or content is rejected by the
outbox/pending store as an idempotency conflict. `send_message` declares durable
request recovery and fails closed outside runtime-owned operation context; it
never invents a process-local counter.

## Consequences

- An agent's `send_message target_thread` durably delivers input to the run
  waiting on that thread, resuming it — the multi-agent handoff works end to end.
- The extension owns the model-visible tool; the host owns the adapter and the
  outbox; the boundary (ADR-0007) holds.
- `awaiting_run` is a new dispatch read port, proven across the three backends.
- Addressing is correct (thread, not run), including durable idle-thread input.
- Callers may omit `idempotency_key`; retries remain stable across process
  replacement through runtime-owned Run/operation identity.
- One identity cannot silently accept another payload.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1/G13 (committed truth; single source).
- ADR-0007 — the builtin-tool boundary and injected service ports.
- ADR-0013 — the outbox and relay this delivers over.
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  `thread_id` as the shard and consistency key.
