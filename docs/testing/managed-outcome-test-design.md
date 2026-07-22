# Managed Outcome test design

## Scope and oracle

The system under test begins at Managed `user.define_outcome`, crosses the
protocol adapter, Outcome Runtime Extension controller, backend-neutral Run
interface, Worker and Judge Runs, durable Thread state, and returns projected
evaluation spans. Runtime Host supplies execution and persistence adapters but
does not own the state machine. Committed events and terminal HTTP results are
the oracle; model intent is not inferred from conversational prose.

## Techniques and cases

| Technique | Partition or boundary | Expected observation | Automated evidence |
| --- | --- | --- | --- |
| state-transition | defined → worker → evaluate → revise → evaluate → complete | paired spans at iterations `0,1`; stable Outcome id | `managed_outcome_lifecycle_e2e.mjs` |
| decision table | needs revision then satisfied | `needs_revision`, then `satisfied` | `managed_outcome_e2e.mjs` |
| decision table | every Grade remains unmet | last result `max_iterations_reached` | `managed_outcome_e2e.mjs` |
| boundary value | `max_iterations = 1` | exactly one Grade, then one ungraded acknowledgment | `managed_outcome_runtime_matrix_e2e.ts` |
| equivalence partition + boundary value | blank description/rubric; `max_iterations` at `0` and `21` | request rejected with 400 before any Worker/Judge Run | `managed_outcome_runtime_matrix_e2e.ts` |
| compatibility matrix | Worker `{Native, ACP}` × Judge `{Native, ACP}` | all four pairs have identical lifecycle semantics | `managed_outcome_runtime_matrix_e2e.ts` |
| error guessing | Judge tools configured | rejected by neutral capability narrowing before Judge execution | Runtime/ACP isolation tests |
| syntax/negative partition | prose-wrapped, missing, empty, unknown-field Grade JSON | strict parser rejects | `awaken-ext-goal` and Host tests |
| recovery transition | crash after committed Worker Run | stable Run is reused without another inference | Outcome extension controller restart test |
| concurrency/stale write | stale aggregate version on Worker Thread state | transition rejected; evaluation not duplicated | Outcome extension state-codec version-guard tests |
| interruption phase | Worker / Judge / final acknowledgment in flight | terminal `interrupted`; no later Grade | `managed_outcome_recovery_e2e.ts` |
| Judge decision/schema | terminal `failed` / malformed JSON | failed evaluation / fail-closed stable error | `managed_outcome_recovery_e2e.ts` |
| fault partition | Worker provider failure / Grader provider failure | infrastructure 5xx; never rubric `failed` | `managed_outcome_recovery_e2e.ts` |
| crash boundary | SIGKILL after Worker commit, during Judge inference | recover Judge; never repeat committed Worker | `managed_outcome_recovery_e2e.ts` |
| command transition | interrupt in live phase and repeat after terminal | first wins, terminal state is idempotent | Outcome domain and adapter interrupt tests |

Changed-line coverage uses the repository merge-base, not the mutable branch tip.
Audited non-API-reachable lines use the repository-wide
`scripts/ci/e2e_unreachable.toml` ledger: every range requires a reachability reason
and concrete lower-layer evidence, covered lines remain in the numerator, stale
entries fail the gate, and the total waived share is capped. Valid Outcome
orchestration, backend routing, Managed projection, interruption, and restart
paths remain in the E2E denominator.

The backend matrix is exhaustive rather than pairwise-reduced because two binary
factors produce only four combinations. The Judge oracle uses backend-specific
explanations and the Worker oracle uses backend-specific transcript markers, so a
test cannot pass by silently falling back to Native execution.

## Coverage criterion

The release gate instruments the served Rust binary, executes the deterministic
TS E2E suites, and compares executable changed lines with LCOV. The strict result
must be greater than 95%; unit, Kani, and TLA+ checks complement but do not count
toward that E2E threshold.
