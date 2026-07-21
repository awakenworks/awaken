# Managed Outcome test design

## Scope and oracle

The system under test begins at Managed `user.define_outcome`, crosses the
protocol adapter, Runtime Host Outcome controller, backend-neutral Run interface,
Worker and Judge Runs, durable Thread state, and returns projected evaluation
spans. Committed events and terminal HTTP results are the oracle; model intent is
not inferred from conversational prose.

## Techniques and cases

| Technique | Partition or boundary | Expected observation | Automated evidence |
| --- | --- | --- | --- |
| state-transition | defined → worker → evaluate → revise → evaluate → complete | paired spans at iterations `0,1`; stable Outcome id | `managed_outcome_lifecycle_e2e.mjs` |
| decision table | needs revision then satisfied | `needs_revision`, then `satisfied` | `managed_outcome_e2e.mjs` |
| decision table | every Grade remains unmet | last result `max_iterations_reached` | `managed_outcome_e2e.mjs` |
| boundary value | `max_iterations = 1` | exactly one Grade, then one ungraded acknowledgment | `managed_outcome_runtime_matrix_e2e.ts` |
| compatibility matrix | Worker `{Native, ACP}` × Judge `{Native, ACP}` | all four pairs have identical lifecycle semantics | `managed_outcome_runtime_matrix_e2e.ts` |
| error guessing | Judge tools configured | rejected before Judge execution | `awaken-runtime-host` unit test |
| syntax/negative partition | prose-wrapped, missing, empty, unknown-field Grade JSON | strict parser rejects | `awaken-ext-goal` and Host tests |
| recovery transition | crash after committed Worker Run | stable Run is reused without another inference | `outcome_controller` restart test |
| concurrency/stale write | stale aggregate version | transition rejected; evaluation not duplicated | `outcome_state` CAS tests |
| command transition | interrupt in live phase and repeat after terminal | first wins, terminal state is idempotent | Outcome domain and Host interrupt tests |

The backend matrix is exhaustive rather than pairwise-reduced because two binary
factors produce only four combinations. The Judge oracle uses backend-specific
explanations and the Worker oracle uses backend-specific transcript markers, so a
test cannot pass by silently falling back to Native execution.

## Coverage criterion

The release gate instruments the served Rust binary, executes the deterministic
TS E2E suites, and compares executable changed lines with LCOV. The strict result
must be greater than 95%; unit, Kani, and TLA+ checks complement but do not count
toward that E2E threshold.
