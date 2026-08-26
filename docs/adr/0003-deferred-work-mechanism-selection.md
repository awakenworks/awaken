# ADR-0003: Deferred-Work Mechanism Selection

- Status: Accepted
- Depends on: ADR-0001
- Amended by: ADR-0076 for the narrow Session-owned background-tool execution
  capability; the prohibition on a generic deferred-work umbrella remains.

## Context

Four distinct mechanisms model "work that completes after the current step".
Their boundaries are clean, but the *selection* guidance was scattered across
several design docs with no single "which one do I use" entry point — which is
what led an earlier design to invent a redundant `BackgroundTask` umbrella type
(retired in ADR-0001 D3). This ADR is the missing navigation: it does not add a
mechanism, it picks among the existing ones.

## Decision

### D1: Use exactly one owning mechanism; do not add a generic umbrella type

| Mechanism | Use when | Owning design doc (this corpus) |
|---|---|---|
| `ScheduledAction` | defer work to a later phase **within the same run**; in-process | [runtime-behavior.md](../design/runtime-behavior.md) (scheduled and background work) |
| run resume ticket / resume decision | **suspend the run awaiting an external decision/result** (HITL, client tool, scheduled) | [runtime-behavior.md](../design/runtime-behavior.md) |
| durable run dispatch | hand work to a **durable cross-process queue** (retry, crash recovery) | [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) |
| background tool execution | explicitly detach one ordinary tool call from its origin Run while retaining Session-owned list/get/cancel | [ADR-0076](0076-session-owned-background-tool-execution.md) |

### D2: Shared rules across all four

The request commits before any durable wake; resume comes back through ingress;
duplicate wakes reconcile from the committed request, never an uncommitted one
(guardrails G5, G13). No new "deferred"/"background" type may be added that
re-implements these three — extend the matching mechanism instead.

### D3: Recovery belongs to the selected mechanism, not to `BackgroundTask`

No generic BackgroundTask is a runtime capability. The narrow BackgroundTask
read model in ADR-0076 belongs only to detached tool execution and cannot wrap
ScheduledAction, Run resume, child Runs, arbitrary application jobs, or Session
Work. Recovery is supported only through the committed records owned by the
selected mechanism:

| Mechanism | Recovery evidence |
|---|---|
| `ScheduledAction` | committed request, correlation/idempotency key, run/thread binding, snapshot/catalog fingerprint, deadline |
| run resume ticket / resume decision | committed `ResumeTicket`, resume ticket, pending call/decision id, descriptor fingerprint, deadline |
| durable run dispatch | durable pending input, dispatch lease/claim state, outbox entries, committed facts/events |
| background tool execution | committed namespaced Thread State, owner/epoch fence, frozen recovery policy, terminal task outcome |

Cancel and stop make the current run terminal. A late result for an old
correlation may be observed for cleanup or diagnostics, but it must not resume
or mutate that run. Continuing work after terminal cancel requires a new run or
new command with a new correlation/idempotency identity.

## Consequences

- A new deferred-work need maps onto one of four existing mechanisms; the
  `BackgroundTask` anti-pattern does not recur.
- Recovery is tested per mechanism: committed-request scan, wait/resume
  validation, outbox replay, dispatch reconciliation, and stale-result rejection.
- This ADR stays navigational: states, transitions, and tests live in the owning
  ADRs and types, not restated here (ADR-0001 D1).

## References

- ADR-0001 D3 (the retired `BackgroundTask` invention).
- INVARIANTS G5 (ingress delivery semantics), G13 (single-source commit).
