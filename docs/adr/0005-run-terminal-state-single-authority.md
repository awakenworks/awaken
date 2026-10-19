# ADR-0005: Run Terminal State Is One Stored Authority

- Status: Accepted
- Date: 2026-06-29
- Depends on: ADR-0001
- Supersedes: the `Done` lifecycle payload that stored a terminal *business
  outcome* (`RunOutcome`) as a field, the flat `Lifecycle`
  (`Pending/Running/Awaiting/Completed/Failed/Cancelled`) status enum, and the
  free-form failure-reason string carried only in the lifecycle event payload.
- Amended by: ADR-0026 (`EndCause::Stopped`)
- Amended: 2026-10-09 (names and references: current `RunState`, `EndCause` and `Failure` variants)

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

Every way a Run ends — a natural text response, the Step ceiling, a cancellation, a
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

## Amendment: committed errors are their own step explanation (2026-08-12)

The Runtime Host's committed-step proof requires a natural terminal to include
committed assistant output. It does not require an `EndCause::Error(Failure)` to
also fabricate or persist an assistant message: the typed failure code and
message inside the committed `RunState` are already the sole durable
explanation. Protocols project their error event from that state. Treating an
otherwise valid error terminal as a proof failure would hide the authoritative
failure behind a second, less useful internal error and would reintroduce a
parallel transcript requirement for error truth.

## Amendment: terminal responses survive a later Run (2026-10-05)

A fast child report can start a successor on the coordinator's Thread after the
first Run commits but before its protocol caller reads the response. The later
Run is current state; it does not revoke the earlier terminal fact. Runtime
Host accepts the earlier response only when the same atomic recovery snapshot
contains its exact terminal lifecycle event and ordered message commit cursors.
It bounds the response at that event's source commit and checks the original
Run's output identity. Missing or malformed evidence still fails closed.
This extends the existing committed-step proof; it adds no status, receipt,
store, queue, or protocol-specific recovery path.

## Amendment 2026-10-09: names and references

D1/D2 list the original variants. The single-authority rule is unchanged; the
closed sets in code are now (`crates/contract/awaken-agent-contract/src/agent/run.rs:22-87`):

- `RunState = Running | Awaiting | Ended(EndCause)`. `Running` is committed at
  step boundaries; [ADR-0054](0054-safe-loop-boundary-shared-seam-and-pause-as-durable-await.md) relies on this set.
- `EndCause = NaturalEnd | MaxSteps | Cancelled | Stopped(String) | Error(Failure) | Indeterminate`.
  `Stopped` comes from [ADR-0026](0026-stop-policy.md). `Indeterminate` (outcome cannot be
  determined; never projected as success) is used by [ADR-0015](0015-crash-retry-budget-and-dead-letter.md),
  [ADR-0024](0024-daemon-lease-renewal.md) and [ADR-0044](0044-remote-hand-tool-executor-over-a-channel.md); no ADR introduced it on its own.
- `Failure = Inference { code, message } | CapabilityBound | StateConflict`.

## Amendment 2026-10-20: one inference retry budget

The existing `RetryPolicy` owns attempts and backoff for one logical inference
request. A permanent refusal or exhausted request ends the Run through the
same terminal commit. A second failed-step tolerance cannot restart that
budget or absorb a permanent refusal into a later natural success. Its Runtime
builder setting is removed rather than retained as an inert compatibility
path. Successful `MaxTokens` continuation retains its separate protocol budget;
unproven physical execution retains its independent recovery veto.

## Amendment 2026-10-20: admission origin is distinct from Provider health

The existing model error carries an explicit `Admission` source wrapper when
its owning application rejects the request before Provider inference. Its
underlying failure grade and original physical evidence remain independent.
It uses the same bounded request retry policy, but cannot affect Provider
health or cause Provider failover/cooldown. The existing circuit-breaker permit
accepts the typed error and owns that health projection instead of accepting
an adapter-selected retryable boolean.

The existing committed inference code retains this source through the closed
`model_admission_*` namespace. Managed protocol projection recognizes those
codes without changing its standard error envelope; Flow projects the owning
admission prerequisite separately from Provider credentials and Provider quota.
No execution outcome, recovery permission, journal or scheduler is introduced.

Environment failures retain the existing realization-contract projection as
their classification authority. The realization boundary derives its local
disposition from that projection rather than independently matching Provider
routing variants; adding a model routing decision cannot create a second
Environment failure table.

`ModelRequestGate` returns the existing typed model error instead of a string.
The Session adapter derives its grade from the existing local or authenticated
Control failure disposition. Runtime adds admission origin before Provider
entry, while an independent physical-quiescence veto still wins over either
admission success or refusal. Gate failure terminates the current attempt;
it does not introduce another request retry budget or a fake model audit event.
