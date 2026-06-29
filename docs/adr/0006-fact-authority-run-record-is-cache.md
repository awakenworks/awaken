# ADR-0006: The Fact Log Is The Run's Truth; The Run Record Is A Cache

- Status: Accepted
- Date: 2026-06-29
- Depends on: ADR-0001, ADR-0005

## Context

Each commit appends a run-projection fact (the run's `Phase`, ADR-0005) to the
thread's append-only fact log, and also updates a convenience `RunRecord` read by
the run-store port. Two reads of "where does the run stand" therefore exist: the
latest committed fact, and the cached record. If the record were treated as an
independent authority it could drift from the log — the same single-authority
failure ADR-0005 removed inside the run, now one level up between the log and its
cache.

This ADR fixes which one is the truth, before a second coordinator backend
(durable storage) exists and has to answer the same question.

## Decision

### D1: The committed fact log is the read authority

A run's authoritative current phase is the latest run-projection fact in the
thread's fact log. Replay and projection reconstruct run state from the log, not
from any cached record. The log is append-only and ordered; an earlier `Waiting`
fact and a later `Ended` fact both remain, and the latest one wins.

### D2: The run record is a derived cache, not a second authority

The `RunRecord` exposed by the run-store port is a projection of the latest fact,
kept for cheap point reads. It must equal what replay derives from the log. It is
never written as an independent source and never carries a field that the log
does not.

### D3: The append fence is the committed fact count

Ordering and the append fence are properties of the committed log — the
monotonic commit sequence / committed-fact count — not of a cache row count. A
backend proves the fence against what it has committed to the log, however it
also materializes the cache.

### D4: One backend or many, the contract is the same

These rules are the commit contract any coordinator must satisfy. The in-memory
coordinator satisfies them today; a future durable backend is correct only if it
satisfies the same reads — log is authority, record equals the latest fact, fence
is the committed count — so the contract is stated independently of any one
implementation rather than discovered per backend.

## Consequences

- Replay and the run-store read agree by construction; a stale cache cannot be
  mistaken for the truth.
- A durable backend has an explicit acceptance target before it is written, with
  no new decision to make about which read wins.
- "Where does the run stand" has one answer (the log); the record is an
  optimization, removable without changing truth.

## References

- [runtime-behavior.md](../design/runtime-behavior.md) — `ThreadCommit`,
  `RunRecord`, and append-fence role catalog.
- [INVARIANTS.md](../INVARIANTS.md) — G1/G13 (commit boundary) and the fact-log
  authority guardrail.
- ADR-0005 — single stored authority inside the run (`Phase`/`EndCause`).
