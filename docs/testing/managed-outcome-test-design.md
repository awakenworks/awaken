# Managed Outcome test design

## Scope and oracle

The system under test begins at Managed `user.define_outcome`, crosses the
protocol adapter, Outcome Runtime Extension controller, backend-neutral Run
interface, Worker and Judge Runs, durable Thread state, and returns projected
evaluation spans. Runtime Host supplies execution and persistence adapters but
does not own the state machine. Committed events and terminal HTTP results are
the oracle; model intent is not inferred from conversational prose.

This file is the single Outcome test-design and FMECA owner. The system-wide
cause/effect and FMECA documents may reference these rules but must not copy a
second Outcome matrix. `awaken-ext-goal` remains independently usable: it
depends only on `awaken-runtime-contract`; Managed, Runtime Host, HTTP, and a
concrete store are optional composition adapters.

## Structure and behavior views

| Boundary | Owns | Depends on | Must not own |
| --- | --- | --- | --- |
| `awaken-ext-goal` | definition, binding, active aggregate, phases, evaluations, stable ids, recovery | neutral Thread reader/coordinator, Run executor, Grader ports | Server session, Managed DTO/event cache, concrete store/backend |
| Runtime Host | backend/context selection, locks, neutral DTO projection | the extension controller and ordinary Run lifecycle | Outcome phase/status/continuation registry |
| Session application | protocol-neutral define/continue commands | one `SessionRuntime` | copied Outcome state |
| Managed adapter | input validation and committed event/span projection | Session application and lifecycle cursor | iteration, retry, active Outcome truth |

```text
define -> ext-goal creates/reloads the Thread aggregate -> Worker Run
  natural end -> grade -> revise/complete
  awaiting    -> return a successful external-input boundary
                    -> ordinary Run resume commits allow/deny/result
                    -> ext-goal reloads the same active aggregate
                    -> awaiting again, or grade -> revise/complete

process replacement -> rebuild controller from the same Thread ports -> same path
no active aggregate -> continue returns None and never invents a definition
```

The consistency fence is the Worker Thread commit. Protocol event projection is
disposable and may be rebuilt; it cannot advance the Outcome.

## Cause-effect graph and decision table

Causes: `C1` valid new definition; `C2` active persisted aggregate; `C3` Worker
natural end; `C4` Worker awaits a protected/client tool; `C5` matching ordinary
Run resume commits; `C6` resumed Worker awaits again; `C7` resumed Worker ends;
`C8` no active aggregate; `C9` resume identity/binding mismatches; `C10` process
is replaced after a commit. Constraints: `C3` and `C4` are exclusive at one Run
boundary; `C6` and `C7` are exclusive after one resume; `C8` excludes `C2`;
`C9` masks all resume effects.

Effects: `E1` create exactly one aggregate; `E2` grade/complete; `E3` expose
Awaiting without `session.error`; `E4` continue only the persisted aggregate;
`E5` expose the next Awaiting once; `E6` append evaluation spans and terminal
idle once; `E7` return `None` without execution; `E8` reject before side effects;
`E9` reuse stable Run/evaluation identities after replacement.

```text
C1 -> E1 -> (C3 -> E2) | (C4 -> E3)
C2 & C5 -> E4 -> (C6 -> E5) | (C7 -> E2 + E6)
C8 -> E7              C9 -> E8              C2 & C10 -> E9 -> E4
```

| Rule | Causes | Required effects | Automated evidence |
| --- | --- | --- | --- |
| O1 | C1,C3 | E1,E2,E6 | `outcome_iterates_until_satisfied` and Outcome controller lifecycle tests |
| O2 | C1,C4 | E1,E3; no evaluation/error | `outcome_hitl_awaits_without_failure_then_resumes_the_active_aggregate` |
| O3 | C2,C5,C6 | E4,E5; no duplicate tool event | same Managed real-kernel test, first allow |
| O4 | C2,C5,C7 | E4,E2,E6; allow or deny stays ordinary Run input | same Managed real-kernel test, second deny |
| O5 | C8 | E7 | `embedded_controller_resumes_without_any_server_application` and ordinary HITL E2E |
| O6 | C9 | E8 | Managed pending-tool admission mismatch tests |
| O7 | C2,C10 | E9,E4 | `restart_after_worker_commit_reuses_the_stable_run` |
| O8 | C1,C4,C5,C7 with no Server types | E1,E3,E4,E2 | `embedded_controller_resumes_without_any_server_application` |

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

## Outcome FMECA and elimination measures

Scores are residual risk after controls: severity `S`, occurrence `O`, and
detection difficulty `D` are 1–5; `RPN=S×O×D`. The mitigation column names the
authoritative elimination, not a synchronized fallback.

| ID | Failure mode and end effect | S/O/D · RPN | Authoritative elimination/detection/recovery | Decision evidence |
| --- | --- | --- | --- | --- |
| OF1 | Awaiting is mapped to a request/runtime failure; valid HITL Outcome emits `session.error` | 4/2/3 · 24 | typed `OutcomeDrive::Awaiting`; lifecycle projection consumes committed Run truth | O2 |
| OF2 | Run permission resume commits but Outcome controller is never driven again; aggregate remains `RunningWorker` forever | 4/2/4 · 32 | one `continue_outcome` command reloads `outcome/active`; no protocol continuation registry | O3,O4,O7 |
| OF3 | Outcome and lifecycle projection both emit the same messages/tools; UI sees duplicates and resume ids drift | 4/2/3 · 24 | consume committed lifecycle first; `projected_message_ids` filters report text; evaluation spans add only Outcome facts | O2–O4 |
| OF4 | Extension requires Runtime Host/Managed service, preventing embedded/CLI use and moving domain logic into composition | 4/2/2 · 16 | crate dependency fence plus public controller over neutral Run/Thread/Grader ports | O8 and crate-boundary gate |
| OF5 | Continue with no active aggregate reconstructs a guessed definition or ghost workflow | 4/1/3 · 12 | `resume_active -> None`; definition is created only by explicit define | O5 |
| OF6 | Repeated protected calls or deny result loses correlation, skips grading, or creates a second aggregate | 5/2/3 · 30 | every allow/deny/result uses ordinary pending Run identity; each boundary reloads the same active pointer | O3,O4,O6 |
| OF7 | Replacement after Worker commit repeats inference/effects or grades current rather than pinned snapshots | 5/1/3 · 15 | deterministic Run ids, immutable binding, version-guarded Thread state, committed-state replay | O7 plus snapshot-pinning tests |

Release handling is fail-closed: mismatched resume is rejected, persistence or
execution faults remain infrastructure errors, and no mitigation creates a
second status, transcript, cursor, or Outcome repository.

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
