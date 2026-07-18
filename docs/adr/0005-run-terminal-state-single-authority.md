# ADR-0005: Run Terminal State Is One Stored Authority

- Status: Accepted
- Date: 2026-06-29
- Depends on: ADR-0001
- Supersedes: the `Done` lifecycle payload that stored a terminal *business
  outcome* (`RunOutcome`) as a field, the flat `Lifecycle`
  (`Pending/Running/Awaiting/Completed/Failed/Cancelled`) status enum, and the
  free-form failure-reason string carried only in the lifecycle event payload.

## Context

A run that ended recorded *three* parallel notions of "how it ended":

- a flat `Lifecycle` status whose terminal variants were `Completed` / `Failed` /
  `Cancelled`, stored on the run fact;
- a `RunOutcome` "business outcome" the lifecycle table assigned to the `Done`
  state; and
- a free-form failure string placed in the lifecycle *event* payload, not in the
  committed fact.

Three problems follow. The terminal classification has **no single home**: a
consumer asking "is this an error — retry or give up?" must match loose status
variants, and the *reason* a run failed is a string in an event, recoverable only
by parsing. Two stored notions of the same fact (status and outcome) can
**drift**. And the failure reason living outside the committed authority means
replay and projection cannot reconstruct *why* a run ended from the fact log
alone.

## Decision

### D1: One stored authority — the run `RunState`

The committed run fact stores exactly one value for where a run stands:

```text
RunState = Awaiting | Ended(EndCause)
```

`Awaiting` is a pause; `Ended` carries the single terminal authority. No second
status, outcome, or error field is stored beside it. A run record therefore
cannot disagree with its own classification.

### D2: `EndCause` is the closed set of end mechanisms

```text
EndCause = NaturalEnd | MaxSteps | Cancelled | Error(Failure)
Failure  = Inference(detail) | CapabilityBound | StateConflict
```

`EndCause` records the *mechanism* a run ended by; the fault *kind* (and its
detail) lives inside `Error(Failure)`, in the committed authority — not in a
separate string. The set is closed: a new mechanism is a new variant with an
explicit review, not an open string.

### D3: Status, outcome, and the error flag are derived — never stored

Any coarser notion (a published `Completed/Incomplete/Cancelled/Failed`, a
running/waiting/done status, an `is_error` flag) is a *projection* of `EndCause`,
computed where a consumer needs it. None is persisted, so none can drift from the
authority. No projection type is materialized until a consumer exists for it
(today none does); the stored `RunState` is the only durable value.

### D4: One terminal exit

Every way a run ends — a natural text turn, the step ceiling, a cancellation, a
fault — funnels through a single commit boundary that writes the one `RunState`
authority and emits the finish event exactly once. There is no second
terminal-commit path that could double-emit or record a conflicting end.

### D5: Awaiting is not an end

`Awaiting` and `Ended` are mutually exclusive by construction: a paused attempt
carries its resume ticket (committed in the checkpoint's waiting slot, not on
the run fact), and an ended attempt carries its `EndCause`. The engine cannot
produce a state that disagrees with the presence of a ticket.

## Consequences

- "Is this an error?" is answered by matching `EndCause::Error`, not by parsing a
  string or reconciling two stored fields.
- The failure reason is durable: replay reconstructs *why* a run ended from the
  committed fact alone.
- Adding a terminal mechanism (e.g. a timeout cutoff) is a reviewed `EndCause`
  variant; a single stop reason such as `MaxSteps` stays a flat variant until a
  second cutoff justifies grouping it.
- The step ceiling is now an explicit end cause (`MaxSteps`), not silently reported
  as a natural completion.

## References

- [runtime-behavior.md](../design/runtime-behavior.md) — run lifecycle section and
  role catalog (`RunState` authority, derived projections).
- [INVARIANTS.md](../INVARIANTS.md) — the enforceable guardrail and its tests.
- ADR-0001 D3 (one internally consistent vocabulary).
