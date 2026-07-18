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

### D3: Delivery to a non-awaiting thread is deferred

Because delivery is keyed to an aawaiting run's ticket correlation (ADR-0010), this
adapter delivers to a thread whose run is *waiting* (the HITL / inter-agent
handoff case). Unsolicited delivery to an idle thread — a fresh input the thread
consumes on its next run — needs new-input (not resume) semantics and a
thread-level pending queue independent of a ticket; that remains deferred.

## Consequences

- An agent's `send_message target_thread` durably delivers input to the run
  waiting on that thread, resuming it — the multi-agent handoff works end to end.
- The extension owns the model-visible tool; the host owns the adapter and the
  outbox; the boundary (ADR-0007) holds.
- `awaiting_run` is a new dispatch read port, proven across the three backends.
- Addressing is correct (thread, not run); unsolicited idle-thread delivery is a
  named, deferred extension.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1/G13 (committed truth; single source).
- ADR-0007 — the builtin-tool boundary and injected service ports.
- ADR-0013 — the outbox and relay this delivers over.
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  `thread_id` as the shard and consistency key.
