# ADR-0003: Deferred-Work Mechanism Selection

- Status: Accepted
- Depends on: ADR-0001
- Supersedes: none

## Context

Three distinct mechanisms model "work that completes after the current step".
Their boundaries are clean, but the *selection* guidance was scattered across
several design docs with no single "which one do I use" entry point — which is
what led an earlier design to invent a redundant `BackgroundTask` umbrella type
(retired in ADR-0001 D3). This ADR is the missing navigation: it does not add a
mechanism, it picks among the existing ones.

## Decision

### D1: Use exactly one existing mechanism; do not add an umbrella type

| Mechanism | Use when | Owning design doc (this corpus) |
|---|---|---|
| `ScheduledAction` | defer work to a later phase **within the same run**; in-process | [runtime-behavior.md](../design/runtime-behavior.md) (scheduled and background work) |
| run resume ticket / resume decision | **suspend the run awaiting an external decision/result** (HITL, client tool, scheduled) | [runtime-behavior.md](../design/runtime-behavior.md) |
| durable run dispatch | hand work to a **durable cross-process queue** (retry, crash recovery) | [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) |

### D2: Shared rules across all three

The request commits before any durable wake; resume comes back through ingress;
duplicate wakes reconcile from the committed request, never an uncommitted one
(guardrails G5, G13). No new "deferred"/"background" type may be added that
re-implements these three — extend the matching mechanism instead.

### D3: Recovery belongs to the selected mechanism, not to `BackgroundTask`

`BackgroundTask` is not a runtime capability and therefore cannot be "resumed"
or recovered as an object. Recovery is supported only through the committed
runtime/server records owned by the chosen mechanism:

| Mechanism | Recovery evidence |
|---|---|
| `ScheduledAction` | committed request, correlation/idempotency key, run/thread binding, snapshot/catalog fingerprint, deadline |
| run resume ticket / resume decision | committed `ResumeTicket`, resume ticket, pending call/decision id, descriptor fingerprint, deadline |
| durable run dispatch | durable pending input, dispatch lease/claim state, outbox entries, committed facts/events |

Cancel and stop make the current run terminal. A late result for an old
correlation may be observed for cleanup or diagnostics, but it must not resume
or mutate that run. Continuing work after terminal cancel requires a new run or
new command with a new correlation/idempotency identity.

## Consequences

- A new deferred-work need maps onto one of three existing mechanisms; the
  `BackgroundTask` anti-pattern does not recur.
- Recovery is tested per mechanism: committed-request scan, wait/resume
  validation, outbox replay, dispatch reconciliation, and stale-result rejection.
- This ADR stays navigational: states, transitions, and tests live in the owning
  ADRs and types, not restated here (ADR-0001 D1).

## References

- ADR-0001 D3 (the retired `BackgroundTask` invention).
- INVARIANTS G5 (ingress delivery semantics), G13 (single-source commit).
