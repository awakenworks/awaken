# Formal proof trust: Kani harness inventory and run plan

The formal gate (`scripts/ci/check_formal.sh`) must finish on this host without
an inconclusive proof. This note records, for each of the 610 `#[kani::proof]`
harnesses on the branch, what the recorded runs say, what each harness that
could not finish needed and what was changed (phase 2, below), and the order in
which to rerun the 142 batches inside hourly build slots. Every number below
comes from one command:

```sh
python3 scripts/ci/formal_inventory.py @<transcript-list> [--table | --slots | --slot N | --obligations] \
  --first background_task.stale_fence --first activity- \
  --then fence_succession_is_exactly --then realization_generation_authority_requires \
  --then model_operation_history_preserves --then work_lease_deadlines_clamp
```

`formal_inventory.py` reads the derived plan (`formal_plan.py`), the audited
profiles (`formal_kani_options.py`), the obligation ledger, `git blame` author
times and any Kani transcripts it is given. The transcript list used here holds
the 233 logs on this host that contain `Checking harness` lines (under
`/build/scratch`, `/build/targets` and lane `target/` directories; `find ...
-name '*.log' | xargs grep -l 'Checking harness'`). Plan digest at the time of
writing: `sha256:9e07b7d7…e5ee` (`formal_plan.py kani --tier full --digest`).
No proof was run for phase 2; every changed or new harness is class d (or b)
until its slot below has run.

## Current nanos-image proof (2026-11-02)

The phase-2 inventory below is historical. Derive the current selections with
`python3 scripts/ci/formal_plan.py kani --tier full`; source classification or a
focused result does not replace the strict full formal gate.

The existing `unix_nanos_of_epoch_ms_is_strictly_monotone` harness now checks
the complete image of one production conversion over every `u64`, including
zero and `MAX`. Its unit oracle is
`(wire / 1000) * 1_000_000_000 + (wire % 1000) * 1_000_000`, with both terms
promoted to `u128` before multiplication. Euclidean division makes this exactly
`u128::from(wire) * 1_000_000`; the largest image is below 2^84. This stronger
equality implies the original strict-order bridge. The oracle resembles the
standard library's unit decomposition; it is not another independent
implementation. The native conversion test retains its linear oracle.

Production time code and the wire domain are unchanged. Expiry `Ord` has the
independent compositional check described below. The two successor covers still require `wire < u64::MAX`,
because `MAX` has no successor; separate zero/MAX covers exercise the full
image's endpoints. The harness selects the existing `solver-z3-v1` profile,
with all checks and the original 1,800-second ceiling retained.

The short Z3 comparison on frozen staged-source hash `5365f2f9c7cc` completed
succession and this nanos harness, satisfying all four nanos covers and the
vacuity check of the two completed results. The three-harness invocation
reached its requested 300-second limit in the independent Expiry lemma; that
is neither a complete batch PASS nor evidence of a 1,800-second failure.
The source-derived nanos batch and final-source full formal qualification
retain their separate receipts; the obligation rerun markers are unchanged.

## Current Expiry ordering formulation

The existing `expiry_from_epoch_ms_is_strictly_monotone` harness checks two
links rather than constructing two adjacent millisecond conversions. First,
every `u64` wire, including zero and `MAX`, must construct the same complete
Expiry as `Expiry::at(UtcTime::from_epoch_ms(wire))`. Second, the real Expiry
comparison over two independent canonical `Duration` coordinate pairs must
equal their integer lexicographic comparison. Both seconds inputs range over
all `u64`; only the `u32` nanos inputs are bounded below 1,000,000,000, the
valid representation of every Duration. No wire or seconds bound is added.
The two explicit nanos assumptions define only that canonical representation;
they do not assume an ordering or exclude any valid Duration. They replace
surjective modulo generators over precisely the same legal domain.

The composition uses the actual single-field Expiry/UtcTime wrappers,
`Duration::new` preserving canonical inputs, and the actual `as_nanos`
projection `P(s,n) = u128(s) * 1_000_000_000 + u128(n)`. This projection is an
order embedding: equal seconds compare their nanos, and a one-second increase
exceeds the largest possible reversed nanos difference by at least one.
Its largest value is below 2^94. The full-width nanos-image harness already
checks `P(from_epoch_ms(w)) = u128(w) * 1_000_000`; together these links prove
the original strict successor law and the order of every millisecond pair.
The image harness carries the same two obligation anchors, without a second
formula or proof owner. A changed wrapper or canonical representation requires
this composition to be reviewed again; an opaque image equality alone would
not prove the actual Ord implementation.

The original default two-harness run on clean `a191b5ee` reached its requested
300-second diagnostic limit with succession complete and Expiry unfinished.
That is not a failure of the original 1,800-second ceiling. The compositional
candidate still requires its own source-derived complete batches and vacuity
check; neither this argument nor a focused PASS qualifies the full formal gate.
The modulo-generator diagnostic was deliberately stopped at 311.4888s/raw143
with two of three results complete; it did not exhaust the 1,800-second budget.
The direct canonical-bound diagnostic was stopped at 346.601s/raw143 for the
solver comparison, again without exhausting that budget. The existing Ord
harness now reuses `solver-z3-v1`, joining the nanos harness in its derived
Z3 batch; succession stays in the default batch. Inputs, checks, covers and
the 1,800-second ceiling are unchanged by that profile selection.

## Current fixed-target checkpoint formulation

The retained scalar formulation is the planner table plus its full-width
successor replay and the independent stale-fence harness. The table draws
`deadline`, `target` and `now` independently from `{0, 1, 2, 3}`.
A single per-triple map fixes zero and maps its at most three positive
classes to their ranks `1, 2, 3`; this covers all 26 realizable
joint zero/weak-order classes, not merely separate pairwise comparisons.
The production plan copies the target, so input and output use that same
injective map and its inverse. Revision, phase, attempt presence and the
original matching-tenure epoch bound are unchanged; replay keeps its
full-width inputs and both live/expired covers, and stale-fence keeps its
foreign-coordinate/full-epoch domain and four covers.

The only time-image dependency is the existing
`unix_nanos_of_epoch_ms_is_strictly_monotone` at its lease-contract owner,
now also linked to the BackgroundTask obligation by its original anchor.
The independent Expiry `Ord` lemma is not a precondition for this checkpoint
correspondence. No proof or formula is duplicated and no domain or solver
change is made to the retained replay or stale harness.

The table retains real allocation, String trim/comparison and policy Drop
with the existing per-harness `#[kani::unwind(16)]`; unwinding assertions
stay enabled. Its fixed holder/task are 14/4 bytes and Parallel constructs
no resource vector. The original complete default eight-harness run on
staged source `8c0cffae65b90763e1bed19795475433c454bf9c` / model
`bb1a6a8aa1437645b4b5448f7a069da5267de808` completed in 22.91109016s,
raw0 with original vacuity0 and table6/replay2/stale4 covers. Its short300s
ceiling is inside the original 1,800s limit. This establishes the retirement
precondition, not a full-formal PASS or a diagnosis of one particular loop.
The earlier MAX-basis run was deliberately cancelled at 789.2708s/raw143
with 4/8 completed and vacuity0; it did not reach the1,800s budget and must
not be labeled its timeout. Earlier short diagnostics likewise do not
establish a 1,800s failure. The historical raw packets remain unchanged.
The existing `run_evidence = "needs_rerun"` marker is not promoted here;
the new source-derived nanos singleton must be revalidated after its added link.
The earlier combined CaDiCaL/Z3 resource-bound runs and the historical
inventory below remain history, not counterexamples or current passes.

## Current required Rust witnesses and property parsing

All 74 Rust witness calls, including the late Eval witness, use the existing
`_rust_test_execution.sh` / `check_rust_test_execution.py` authority. It builds,
lists and executes the same authored graph, rejects an empty whole selection,
and requires every listed named outcome without ignored or skipped cases.
The Host Loom, CLI source-conversion and Coordinator rollout callers own their
2/1/9 required-case floors. The obsolete App selector and the former Managed
rollout selector each selected zero tests on clean `015acb1c`; the original
required executor rejected both empty selections before execution. No second
runner, case inventory or dependency graph is introduced.

The interrupted strict full run on that source retained 33 completed Kani
results and is Not-passed. Its original vacuity check found four unreachable
array-bounds assertions in the material-input proof helper's absent-predecessor
branches; the target production-equality assertion itself was SUCCESS. Fixed
array destructuring removes those proof-only indexed reads while preserving
the four-slot domain, partition helper and original assertions. The same helper
returns its single production-call result; four covers at the existing harness
witness those actual output partitions. New source must still be compiled and proved.

The original property parser also skipped real multiline descriptions and
qualified names with spaces: it read 7,361 of 19,934 Check records in those
33 files. The same parser now reads complete record boundaries and rejects
empty, malformed or incomplete regular input. Its assertion/cover rules and
deliberate-unreachable/production exceptions remain unchanged. Pure replay
parses all 19,934 records and retains the same four findings; this does not
qualify the changed Vault proof or replace a new complete formal run.

## Current shared proof-helper postconditions

The strict run on clean `2a2eae7a` is Not-passed. Its 55 retained completed
results contain nine deleted-admission literal partitions whose shared oracle
has unreachable branch-local assertions. The two oracles now check total
postconditions at a common assertion: exact outcome fields and valid first
index, plus the complete borrowed input frame. The original resolver, 35
harnesses, 32 literal domains, 3174/96-row Native tables and replay/Drop remain;
three Native negative controls corrupt single fields and require rejection.

An independent original batch confirms one additional unreachable Complete
assertion in the service-lifecycle helper. Its original harness now updates the
independent expected model after finish and exercises that same helper, retaining
the completion condition plus phase and empty-task checks. The Catalog risk's
original batch and vacuity passed and its source is unchanged. Candidate source
checks and static reviews do not replace new complete affected batches or the
final strict full run; no vacuity exception, new domain bound or budget is added.

## Current total outcomes and lazy-entropy observations

The strict full run on clean `3d1795c3` stopped with raw75 after 7,412.93s:
the next session-configuration batch was not admitted when available memory
fell below the existing 8GiB Cargo floor. It is Not-passed. All 450 completed
regular results and the verified failed artifact bundle are retained outside
the Cargo target. The original checker reports four harnesses with unreachable
proof-code assertions, separately from that resource refusal.

The Dispatch proof now compares the complete exact-claim outcome with its
independent expected DeadLetter before inspecting the payload. Its cancellation
preservation, non-requeue and non-claim assertions remain. The adjacent equality
of an execution-admission call with itself is removed: the existing physical-slot
and thread-peer proofs already own those execution-admission rules. Production
reducers, wire types and admission rules are unchanged.

The two Vault no-entropy proofs use a persistent invocation flag instead of
incrementing a counter in a factory that must never execute. Any invocation
still fails; the separate exactly-once proofs retain their counters. The
config-resolver proof combines every original outcome and payload condition
into one total relation; unsupported error variants still make it false.
These corrections retain the existing input domains and production decision
owners. All three
complete source-derived affected batches and their original vacuity checks,
plus the owning Native tests, must pass before normal commit. This focused
qualification cannot replace a fresh strict full run or the all13 gates.

## Phase 2: what changed

| Change | Harnesses | Argument |
|---|---|---|
| Expiry order lemma | new `expiry_from_epoch_ms_is_strictly_monotone` (awaken-lease-contract) | For every `u64` x < MAX, `Expiry::from_epoch_ms(x) < Expiry::from_epoch_ms(x + 1)` (one symbolic value). By transitivity of the total order on `Expiry` the map is an order embedding: `from(x).cmp(&from(y)) == x.cmp(&y)` for every pair. The proposal in phase 1 was a lemma `expires_at_unix_ms(from_epoch_ms(x)) == min(x, i64::MAX)`; the code no longer clamps on that path (`Expiry::to_epoch_ms` round-trips every `u64`, and only `to_persisted_ms` clamps, which no succession judgement reads), and the succession law reads the `Expiry` order, not the millisecond read-back, so the order embedding is the lemma the law needs |
| Liveness order lemma | new `unix_nanos_of_epoch_ms_is_strictly_monotone` (awaken-lease-contract) | The same for the Unix-nanosecond image that `fence_is_live_at`, `Expiry::is_live_at` and `is_expired_at` compare: strictly monotone on consecutive milliseconds, so `fence_is_live_at(e, n) == (n < e)` for every pair |
| Lease succession from representatives | `fence_succession_is_exactly_renewal_or_epoch_advance` | `fence_succession` reads the two expiries only through `successor >= predecessor`; the oracle reads the wire values only through `>=` and `<`. By the expiry lemma both depend on the pair only through which of <, =, > relates it, and the other coordinates are drawn independently. Expiries come from {0, 1, u64::MAX}, whose pairs realise all three relations at the bottom ((0, 1), (0, 0), (1, 0)) and against the top ((1, MAX), (MAX, MAX), (MAX, 1)). The reason is stated in the harness |
| Generation authority from representatives | `realization_generation_authority_requires_every_axis` | The same argument: the predicate reads the expiries only through `Lease::succession`. The four identity booleans and the symbolic versions and epochs are unchanged |
| Ordinal representatives | `model_operation_history_preserves_distinct_subjects` | Checked before changing: `fold_recorded_operation` reads ordinals only through `binary_search_by_key` (`Ord::cmp`) and stores them unchanged; the oracle reads them through `==`, `>`, `<` and `max`, which returns one of its inputs. Ordinals come from {1, 2, u64::MAX}, realising <, =, > and the top value; no split was needed |
| Work-lease split | `work_lease_deadlines_clamp_without_wrapping` (now `lease_expiry` alone, nightly) and new `work_lease_ttl_selection_is_exact` (`effective_ttl_seconds`, `db_millis`, pr) | The persisted deadline is `lease_expiry(now, effective_ttl_seconds(..))`; the selected TTL is one `u64`, so with `lease_expiry` exact for every `u64` pair the second evaluation was an instance of the first. The unit table `lease_deadline_boundary_table` still checks the composition |
| E split, prepared | new `fixed_target_heartbeat_planner_outcomes_are_exact` and `fixed_target_heartbeat_replay_of_the_planned_successor_acknowledges` beside the unchanged combined harness | The table harness makes one planner call; the replay harness needs only `target > deadline` and the planned successor (the table proves the successor is the predecessor with deadline `target`, and the tenure does not include the deadline). The epoch is fixed at `Epoch::FIRST`: the fence is built from the same attempt, so tenure equality holds for every epoch, and the stale-fence harness draws every `u64` epoch. Decision after slot 1: if the combined harness finishes, delete the split pair (it is then a duplicate); if it does not, delete the combined harness |
| Duplicates removed | see class c below | Each kept harness proves an exact formula that implies the removed one; the removed harness's `@obligation` lines were already on the kept harness or on other harnesses of the same obligation, so the coverage checks are unchanged |
| Narrowed domains widened | `realization_renewal_requires_nonregressed_expiry`, `realization_renewal_expires_at_exact_boundary`, `bounded_deadline_passes_once_it_is_reached` | Phase 1 said "a comparison costs the same at full width". It does not on these paths: liveness goes through `Duration::from_millis` (a `u64` division) and `as_nanos` (a `u128` multiplication), which is why they were narrowed to `u16`. They now draw from {0, 1, u64::MAX} under the liveness lemma, which covers every `u64` pair, instead of every `u16` pair |
| Vacuity | `kani::cover!` in every changed harness and in E's; the gate refuses vacuous results | See "Vacuity" below |
| Obligations | 65 obligations carry `run_evidence = "needs_rerun"` | See "Obligations awaiting a rerun" below |

No harness in a file another lane owns was changed. The only file outside
`formal_*` that the vacuity gate needed is `scripts/ci/check_formal.sh` (the
`--output-into-files` flag on the batch runner and the check after the batch
loop) and the three expected command lines in `test_formal_run_evidence.py`
and `test_formal_profile.py`. `test_formal_run_evidence.py` also selected "the"
Z3 batch although the plan holds two (background-task and protocol-managed);
it now selects the protocol-managed one its expectation names.

## What the records show

* The last complete formal gate passed on 2026-10-09 (source `080e05f5`): 592
  harnesses, 16,436 s wall time, 2.0 h of it in CBMC. No full run has passed
  since; the next one, that evening, stopped in `awaken-lease-contract`.
* `check_formal.sh` deletes the transcript of every batch that passes, so a
  green gate leaves no per-harness time. The run times below come from lane
  logs that happened to keep their output.
* A harness is matched by function name, which is unique across the plan, so a
  run recorded before a module or crate move still counts. `git blame` then says
  whether the body (or a package-bound loop profile) was edited after that run.
  A comment edit counts as an edit: the kept harnesses whose comment now states
  what a removed duplicate proved are class d for that reason.

## Classes

| Class | Meaning | Phase 1 | Now |
|---|---|---:|---:|
| a | the last run of the current text passed with every cover satisfied, and its slowest pass is under 900 s (half the 1,800 s batch deadline) | 537 | 532 |
| b | the last run did not finish, failed or was vacuous, or a pass used at least 900 s | 5 | 3 |
| c | proves an obligation another harness already proves; merge | 5 | 0 |
| d | never run, or edited (body or loop profile) after its last run | 63 | 75 |

The three in class b are the fixed `fence_succession…` and
`realization_generation_authority…` (their last runs did not finish, so they
stay b until a run of the new text finishes) and the combined
`fixed_target_heartbeat…`, which has not been rerun. `model_operation_history…`
and `work_lease_deadlines…` are d: their last runs passed, only slowly.

### b: did not finish within the gate, and the fix

| Harness (package) | Evidence | Cause | Fix (phase 2) |
|---|---|---|---|
| `fixed_target_heartbeat_acknowledges_or_extends_only_a_live_current_tenure` (awaken-ext-background-task, `solver-z3-v1`) | CaDiCaL and Z3 both reached 1,800 s without a result (2026-10-28); the fixture was then changed to move instead of clone and has not been run | two calls of the production planner over five free `u64`s plus a `String` worker id; the second call (successor replay) doubles the formula | unchanged except covers and a closed-outcome assertion in place of its `panic!` arm; runs in slot 1. The split pair is ready beside it (table above) |
| `fence_succession_is_exactly_renewal_or_epoch_advance` (awaken-lease-contract) | passed in 991 s on 2026-10-09, then did not finish within 1,800 s three times on a later source the same day | each `Expiry::from_epoch_ms` builds a `Duration` (`u64` division by 1,000); two symbolic expiries carry it through every branch | expiries from {0, 1, u64::MAX} under the expiry lemma |
| `realization_generation_authority_requires_every_axis` (awaken-session-execution-contract) | passed in 1,298 s and 1,401 s; three other runs did not finish (2026-10-28) | the same expiry path, plus five `String` coordinates per reference | the same representatives; identity booleans, versions and epochs unchanged |
| `model_operation_history_preserves_distinct_subjects` (awaken-runtime-contract, `solver-kissat-v1`) | 397 s on 2026-10-09; slowest pass 947 s | a two-element `Vec` fold with `unwind(8)`, `String` bindings and two free nonzero `u64` ordinals | ordinals from {1, 2, u64::MAX} |
| `work_lease_deadlines_clamp_without_wrapping` (awaken-work-store, nightly) | 592 s; slowest pass 1,266 s | `check_deadline` evaluated `lease_expiry` twice against a `u128` oracle | split into the TTL selection and one `lease_expiry` proof |

**Batch 24** (`awaken-session-contract`, `unwind-memcmp-65-v1`,
eight `deleted_admission_one_retained_conflict_*` harnesses): the passes recorded
range from 40 s to 380 s each, and the sum of the slowest is about 1,835 s with
start-up, above the deadline. It passed on 2026-10-09 when the host was quiet.
Run it in a quiet slot. If it times out, have `formal_plan.py` put at most four
harnesses in each batch of a loop-profiled selection, not eight.

The out-of-memory class is gone from the plan. The activity "exact-exact" pair
proofs that peaked at 55–58 GiB were replaced on 2026-09-20 by the two-slot
proofs (batch 114, 1–11 s each). The real `BTreeSet` stays only in the
single-insert height proofs (batches 25–26, 26–62 s).

### c: duplicates, removed in phase 2

| Removed | Kept harness that already proves it | Why |
|---|---|---|
| awaken-session-executor `application::session_realization_renewal_failure_disposition_is_total_exact_and_fail_closed` | awaken-session-execution-contract `realization_ownership_loss_proof_is_total_and_exact` | both prove `proves_current_realization_cannot_continue`; the removed copy covered 8 of the 9 variants (it never built `Internal`), so it was vacuous for the ninth. Its other two obligations keep the three `realization_lease` harnesses and the control-permit harnesses |
| awaken-runtime-contract `capture::kani_proofs::non_full_capture_fails_closed_before_redaction` | `capture_projection_admits_content_only_at_full` | same function and domain; its assertions follow from the kept harness's exact equalities |
| awaken-ext-mcp `sensitivity_markers_can_never_widen_a_redacted_projection` | `sensitive_marker_never_projects_payload_publicly` | the kept harness proves the exact formula (`Redacted` iff any marker), which is monotone; its comment now says so |
| awaken-executable-agent-contract `every_duplicated_snapshot_pin_axis_is_binding` | `executable_snapshot_pin_is_complete_exact_and_non_mixing` | concrete instances of the kept harness's symbolic exact characterisation |
| awaken-deployment-config `durable_dispatch_admission_is_monotonic_in_persistence_evidence` | `durable_dispatch_admission_is_exact_and_commit_store_independent` | the exact formula `!durable || postgres || dir || injected` (over both backend variants) is monotone in every evidence bit; its comment now says so |

The other 17 duplicate-candidate groups that `formal_inventory.py` marks
`G<n>` were reviewed in phase 1 and are separate aspects of one kernel or
deliberate domain partitions, for example the 32 `deleted_admission_*` and 30
`session_network_*` harnesses, which split one domain so that each batch
finishes.

### d: needs a run (75)

* Never recorded (11): the six of phase 1
  (`claim_admits_session_mcp_credentials_only_when_every_requirement_is_admitted`,
  `declared_build_packages_never_expand_runtime_hosts`,
  `retained_original_never_authorizes_exact_physical_removal`,
  `driver_wait_never_extends_or_discards_the_original_retry_hint`,
  `known_returns_only_admit_materialization_and_never_physical_reentry`,
  `recovery_control_plan_never_selects_invocation_admission`) and the five
  added in phase 2 (the two lemmas, `work_lease_ttl_selection_is_exact` and
  the two E split harnesses).
* Loop profile repinned after the last run (7): the three activity height
  proofs (rebound to `awaken_session_contract` by 5d1b379f7d), the three erasure
  proofs and the resume validator (moved to artifact-bound prototypes by
  0c4222ac17). A prototype that matches no function, or more than one, in the
  harness artifact stops the run with an error. It cannot pass vacuously, but
  it can fail.
* Body edited after the last run (57): the 50 of phase 1 (among them E's
  `stale_fenced_heartbeat_and_finish_have_no_mutation_plan`, which now also
  carries covers) plus seven phase 2 edits: `model_operation_history…` and
  `work_lease_deadlines…` (from class b), the three widened harnesses, and the
  two kept harnesses whose comment took over a removed duplicate's claim. Among
  them is
  `operation_replay_binds_stored_thread_and_preserves_identity_precedence`
  (607 s after five unfinished attempts, so watch it).

## Vacuity

Kani counts an assertion that no input reaches as a pass, and a
`kani::cover!` that no input satisfies does not change the verdict. Phase 2
makes both visible and makes the gate refuse them:

* **Covers.** Every harness changed in phase 2 and E's harnesses carry
  `kani::cover!` for their key branches, at least one success and one refusal
  path: each succession outcome (reassignment, renewal at an equal and at a
  later expiry, refusal for a shortened expiry, a foreign subject, another
  holder), each order relation in the widened harnesses, each checkpoint
  outcome (ACK, apply, expired, zero target, revision overflow, no attempt) and
  each stale command, the replay both before and after the successor's
  deadline, R1–R3 of the ordinal fold, D4/D5 of the lease expiry, and both
  carry cases of the two lemmas. E's combined harness also lost its `panic!`
  arm: an arm no input reaches is an unreachable assertion, so the closed set
  of outcomes is now asserted once before the match.
* **The gate.** `check_formal.sh` runs every batch with `-Z unstable-options
  --output-into-files`, so Kani writes each harness's regular-format result to
  `result_output_dir/<harness>` while the terminal stays terse. After the batch
  loop, `formal_vacuity.py check result_output_dir` refuses the run (stage
  `kani-vacuity`) when any cover is not `SATISFIED` or any `assertion` in proof
  code (a `#[kani::proof]` function or an item under `#[cfg(kani)]`) is
  `UNREACHABLE`. An `unreachable!()` arm is accepted, since it says so on
  purpose, and so is production code that one harness of a deliberate domain
  split does not reach. Result files with a finding are kept in the artifact
  directory (`kani-vacuity/`); the directory is removed otherwise.
  `formal_vacuity.py --self-test` holds the decision table and runs in the
  gate's self-test block.
* **The inventory.** A transcript block that passed with `k of m cover
  properties satisfied`, k < m, is a `vacuous` run and puts its harness in
  class b.

Not yet done: covers in the other class-d harnesses, and in class a. The gate
already refuses their unreachable assertions; the first full run with the new
gate will say which harnesses have `panic!` arms that no input reaches (the
phase-1 sample found none inside proof code). Each such arm should become
`unreachable!()` or a closed-set assertion, as in E's harness.

## Obligations awaiting a rerun

An obligation is backed when every harness that serves it has a passing run of
its current text (class a). 65 `kernel_proved` obligations are not: 32 have no
backed harness at all (phase 1 counted 30 on that reading; the obligations of
the two kept harnesses whose comment changed joined it) and 33 have at least
one harness that is not. Phase 1 counted only the first reading; a claim with
one unrun harness is not backed either, so both are marked. Each now carries `run_evidence = "needs_rerun"` beside
its status (`formal_ledger.py`); `check_formal_coverage.py` accepts the marker
only on a `kernel_proved` row and prints the count ("kernel-proved awaiting a
rerun"). The marker does not change the status, the coverage ratio or the
verified-profile qualification; a release still needs a full passing gate,
which is the run that backs every claim.

`formal_inventory.py @<list> --obligations` derives the same set from the
transcripts and prints, per obligation, `needs rerun` (marked, still
unbacked), `mark needs_rerun` (unbacked but not marked) or `clear marker`
(marked, now backed), so after each slot the markers are added or removed from
evidence rather than by hand.

## Completing the gate on this host

* **Memory.** Run one CBMC at a time (`-j 1`, the default) under a ceiling below
  the host's RAM, for example `systemd-run --user --scope -p MemoryMax=48G`. An
  out-of-memory batch then stops in minutes and the gate reports `memory-bound`,
  instead of the run thrashing the shared host. No harness in the current plan
  has a recorded out-of-memory run.
* **Time.** Keep the 1,800 s batch ceiling (`formal_budget.py`). Apart from the
  class-b harnesses and the two slow passes now in class d, no harness has a
  recorded pass above 607 s. The plan is estimated at about 4 h of CBMC, plus
  about 90 s of `cargo kani` start-up for each of the 142 batches.
* **Build lane.** Each batch is one `cargo kani` compile behind the single cargo
  admission lane. A slot holds about 50 minutes of estimated work, so it ends
  inside its hour even after queueing.
* **Receipt.** A release still needs one uninterrupted `check_formal.sh --tier
  full` run, which took 4.6 h on 2026-10-09. Schedule it only after the slots
  below leave no harness in class b or d and no obligation marked.

## Kani run batches for the hourly slots

Batch numbers are 0-based lines of `python3 scripts/ci/formal_plan.py kani --tier
full` at the plan digest above. `formal_inventory.py @<same list> --slot N`
with the `--first` and `--then` options above prints the same lines. Slot 1
holds E's harnesses and the rebound activity profiles; then the class d
batches; then the batches of the class b harnesses fixed in phase 2; then
class a. Run each slot as:

```sh
selection="--first background_task.stale_fence --first activity- \
  --then fence_succession_is_exactly --then realization_generation_authority_requires \
  --then model_operation_history_preserves --then work_lease_deadlines_clamp"
rm -rf result_output_dir
python3 scripts/ci/formal_inventory.py @<transcript-list> --slot N $selection |
  while read -r package argv; do
    scripts/ci/kani.sh -p "$package" --output-format terse --verbose \
      -Z unstable-options --output-into-files -j 1 $argv
  done 2>&1 | tee target/formal-slots/slot-N.log
python3 scripts/ci/formal_vacuity.py check result_output_dir --keep target/formal-slots/vacuity-N
```

Then add the log to a separate report list and run `formal_inventory.py` on it
(with `--obligations` to update the markers). Keep the baseline list unchanged
for `--slot`, so the slots stay fixed.

| Slot | Est. | Batches | Content |
|---|---:|---|---|
| 1 | 43 min | 8 25 26 67 | E: the combined fixed-target harness (8), the stale-fence harness and the split pair (67); the rebound activity height profiles (25, 26). Decide the E split from batch 8 |
| 2 | 49 min | 6 7 12 14 16 17 18 31 44 47 50 52 53 54 57 59 70 | class d: profile-repinned erasure/resume, never-run and edited harnesses |
| 3 | 43 min | 61 92 104 105 106 107 111 112 114 115 120 123 126 133 134 137 138 | class d, including the widened realization-lease harnesses (111) |
| 4 | 12 min | 135 | class d (rest); may share an hour with another short job |
| 5 | 41 min | 15 75 | fixed class b: model-operation history (15); lease fence succession and the two lemmas (75) |
| 6 | 45 min | 124 136 | fixed class b: generation authority (124); work-lease split (136, includes the nightly proof) |
| 7 | 50 min | 0 1 2 3 4 5 9 10 11 13 19 20 21 28 | class a: profiled selections first |
| 8 | 49 min | 22 23 24 27 29 30 32 33 34 35 36 37 38 39 40 41 | class a, including at-risk batch 24 |
| 9 | 49 min | 42 43 45 46 48 49 51 55 56 58 60 62 63 64 65 66 68 69 71 72 73 74 76 77 78 79 80 81 | class a |
| 10 | 49 min | 82–91 93–103 108 109 110 113 | class a |
| 11 | 25 min | 116 117 118 119 121 122 125 127–132 139 140 141 | class a |

Slots 5 and 6 are estimated from the old (slow) passes and reserved at the
deadline. A batch that does not finish there is recorded `resource-bound` and
keeps its harness in class b. The lemmas in batch 75 are new; if one of them
does not finish, the harnesses that rely on it are not backed either, so the
fallback is the two-value form of the lemma (`from(x).cmp(&from(y)) ==
x.cmp(&y)`) or, for the liveness lemma, restoring `u16` with the cost reason
stated.

## Appendix: per-harness inventory

Generated by `formal_inventory.py @<transcript-list> --table`. The cost column
uses the slowest pass of the current text: S under 60 s, M under 300 s, L under
900 s, XL at or above 900 s or unfinished; `~` marks a static guess from the
profile, tier, unwind and heap use. `G<n>` is a duplicate-candidate group.

| # | package | harness | obligations | profile/tier | batch | last run | cost | class |
| 1 | awaken-acp-contract | `acp_capability_is_detected_exactly_after_a_verified_observation` | worker.acp_capability_is_detected_only_after_a_verified_handshake | - | 28 | passed 0s | S | a: passed 0s |
| 2 | awaken-agent-config | `compile::processing_geography::processing_geography_requires_exact_evidence_from_every_candidate` | model_geography.every_attempt_candidate_requires_exact_processing_evidence | - | 29 | passed 0s | S | a: passed 0s |
| 3 | awaken-agent-config | `store::management_effect_confirmation_requires_positive_exact_readback` | resource_binding.draft_audit_and_effect_are_atomic<br>resource_binding.pending_effect_survives_until_readback<br>resource_binding.replay_is_idempotent_and_identity_stable | - | 29 | passed 0s | S | a: passed 0s |
| 4 | awaken-agent-config | `store::management_effect_enqueue_is_exact_replay_or_absent_insert` | resource_binding.draft_audit_and_effect_are_atomic<br>resource_binding.replay_is_idempotent_and_identity_stable | - | 29 | passed 0s | S | a: passed 0s |
| 5 | awaken-agent-config | `store::management_audit_call_identity_is_stable_and_tuple_exact` | audit.call_identity_is_stable | - | 29 | passed 0s | S | a: passed 0s |
| 6 | awaken-agent-config | `store::management_audit_admission_requires_exact_intent_and_closed_state` | audit.business_action_follows_audit_record<br>audit.stable_call_replay_does_not_repeat_business_write<br>management_http.no_business_write_without_durable_audit<br>resource_binding.draft_audit_and_effect_are_atomic | - | 29 | passed 0s | S | a: passed 0s |
| 7 | awaken-agent-config | `store::publication_revision_decision_is_target_safe_fail_closed_and_replay_first` | config.publication_revision_is_scoped_by_execution_target_and_exact_replay | - | 29 | passed 0s | S | a: passed 0s |
| 8 | awaken-agent-config | `store::publication_registration_requires_exact_commit_receipt` | config_activation.stale_publication_never_installs | - | 29 | passed 0s | S | a: passed 0s |
| 9 | awaken-agent-contract | `agent::delegation::verification::only_unsettled_relationships_occupy_a_parallel_slot` | delegation.depth_cycle_parallel_total | - | 30 | passed 0s | S | a: passed 0s |
| 10 | awaken-agent-contract | `agent::delegation::verification::every_relationship_effect_has_one_documented_precondition` | delegation.relationship_transition | - | 30 | passed 0s | S | a: passed 0s |
| 11 | awaken-agent-contract | `agent::delegation::verification::delegation_admission_requires_every_budget_and_lineage_guard` | delegation.depth_cycle_parallel_total | - | 30 | passed 0s | S | a: passed 0s |
| 12 | awaken-agent-contract | `agent::delegation::verification::recovered_registry_requires_every_scalar_invariant` | delegation.depth_cycle_parallel_total | - | 30 | passed 0s | S | a: passed 0s |
| 13 | awaken-agent-contract | `agent::delegation::verification::child_invocation_requires_one_exact_open_relationship` | delegation.relationship_identity | - | 30 | passed 0s | S | a: passed 0s |
| 14 | awaken-agent-contract | `agent::delegation::verification::cancellation_delivery_is_enabled_only_by_durable_intent` | delegation.cancel_intent | - | 30 | passed 0s | S | a: passed 1s |
| 15 | awaken-agent-contract | `agent::run::kani_proofs::ended_is_absorbing_for_every_next_state` | run.lifecycle_absorption<br>store_history.terminal_run_is_final_for_every_backend_history<br>tool_result.terminal_run_rejects_late_result | - | 30 | passed 0s | S | a: passed 1s |
| 16 | awaken-agent-contract | `agent::state::kani_proofs::run_identity_binding_is_exactly_run_scoped` | thread_state.run_identity_binding | - | 31 | passed 0s | S | a: passed 0s |
| 17 | awaken-agent-contract | `agent::state::kani_proofs::exclusive_state_conflict_requires_every_exact_precondition` | thread_state.exclusive_batch_conflict | - | 30 | passed 0s | S | a: passed 0s |
| 18 | awaken-agent-contract | `agent::state::kani_proofs::state_materialization_effect_is_total_and_exact` | thread_state.materialization_effect | - | 31 | passed 0s | S | a: passed 0s |
| 19 | awaken-agent-contract | `event::stream_terminal::proofs::stream_terminal_projection_is_exact_fail_closed_and_absorbing` | ui_stream.terminal_projection_is_exact_fail_closed_and_absorbing | - | 31 | passed 0s | S | a: passed 0s |
| 20 | awaken-agent-contract | `fingerprint::hex_proofs::fnv_hex_output_preserves_every_u64` | session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity | fingerprint-fnv-hex-v1/nightly | 0 | passed 157s | M | a: passed 254s |
| 21 | awaken-agent-contract | `fingerprint::hex_proofs::sha256_hex_output_preserves_every_digest_byte` | remote_worker.operation_identity_maps_to_one_hash_and_append | fingerprint-sha256-hex-v1/nightly | 1 | passed 81s | M | a: passed 144s |
| 22 | awaken-agent-contract | `thread::commit::operation::verification::operation_identity_replays_only_the_same_run_and_payload` | remote_worker.acknowledged_projection_is_monotonic<br>remote_worker.committed_truth_is_coordinator_owned<br>remote_worker.operation_identity_maps_to_one_hash_and_append<br>store_history.exact_retry_and_every_rejection_are_atomic_stutters<br>store_history.receipt_refines_one_exact_durable_record<br>store_history.recovery_preserves_committed_time_and_receipt_identity | - | 31 | passed 0s | S | a: passed 0s |
| 23 | awaken-agent-contract | `thread::commit::operation::verification::operation_replay_binds_stored_thread_and_preserves_identity_precedence` | remote_worker.committed_truth_is_coordinator_owned<br>remote_worker.operation_identity_maps_to_one_hash_and_append<br>store_history.exact_retry_and_every_rejection_are_atomic_stutters<br>store_history.receipt_refines_one_exact_durable_record<br>store_history.recovery_preserves_committed_time_and_receipt_identity | - | 31 | passed 607s | L | d: harness edited after last run |
| 24 | awaken-agent-contract | `thread::commit::operation::verification::operation_append_requires_exact_prefix_and_nonwrapping_version` | remote_worker.committed_truth_is_coordinator_owned<br>remote_worker.operation_identity_maps_to_one_hash_and_append<br>store_history.concurrent_compare_and_swap_has_one_winner_per_thread_version<br>store_history.exact_retry_and_every_rejection_are_atomic_stutters<br>store_history.receipt_refines_one_exact_durable_record | - | 31 | passed 1s | S | a: passed 2s |
| 25 | awaken-agent-contract | `thread::commit::operation::verification::applied_receipt_is_exact_and_names_one_successor_record` | store_history.exact_retry_and_every_rejection_are_atomic_stutters<br>store_history.receipt_refines_one_exact_durable_record<br>store_history.recovery_preserves_committed_time_and_receipt_identity | - | 31 | passed 0s | S | a: passed 1s |
| 26 | awaken-agent-contract | `thread::commit::staged::kani_proofs::wire_accepts_exactly_the_legal_dispositions` | run.ticket_coherence | - | 31 | passed 0s | S | a: passed 0s |
| 27 | awaken-agent-contract | `toolset::verification::toolset_policy_exact_override_replaces_only_the_matching_default` | tool_policy.managed_agent_and_mcp_defaults_are_source_exact_and_configurable | - | 32 | passed 0s | S | a: passed 1s |
| 28 | awaken-agent-contract | `toolset::verification::tool_policy_selector_fails_closed_for_unregistered_widened_or_mismatched_calls` | tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls | - | 32 | passed 0s | S | a: passed 0s |
| 29 | awaken-agent-contract | `toolset::verification::autonomy_is_a_total_order_whose_ceiling_test_is_exactly_rank_comparison` | tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls | - | 32 | passed 0s | S | a: passed 0s |
| 30 | awaken-agent-contract | `toolset::verification::every_requirement_sits_at_exactly_one_autonomy_level` | tool_policy.selector_fails_closed_for_unregistered_widened_or_mismatched_calls | - | 32 | passed 0s | S | a: passed 0s |
| 31 | awaken-authz-enforce | `session_decision_collapse_allows_only_explicit_allow` | authz.decision_collapse_fail_closed | - | 33 | passed 0s | S | a: passed 0s |
| 32 | awaken-authz-enforce | `session_method_mapping_is_total_and_read_only_for_exact_safe_methods` | authz.method_mapping_total | - | 33 | passed 0s | S | a: passed 0s |
| 33 | awaken-authz-enforce | `org_admission::organization_bucket_refill_is_exact_and_bounded` | managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time | solver-kissat-v1 | 2 | passed 0s | S | a: passed 0s |
| 34 | awaken-authz-enforce | `org_admission::organization_bucket_saturated_refill_is_canonical_and_bounded` | managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time | solver-kissat-v1 | 2 | passed 0s | S | a: passed 0s |
| 35 | awaken-authz-enforce | `org_admission::organization_bucket_refill_clamp_preserves_capacity_invariant` | managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time | solver-kissat-v1 | 2 | passed 13s | S | a: passed 13s |
| 36 | awaken-authz-enforce | `org_admission::organization_bucket_consumption_is_exact_and_non_over_admitting` | managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time | solver-kissat-v1 | 2 | passed 0s | S | a: passed 0s |
| 37 | awaken-config-resolver | `executable_models::brokered_and_direct_model_readiness_require_their_exact_access_evidence` | model_readiness.brokered_and_direct_routes_require_exact_access_evidence | - | 34 | passed 0s | S | a: passed 0s |
| 38 | awaken-config-resolver | `stores::agent_input_revision_decision_is_exact_non_wrapping_and_replay_first` | agent_inputs.applied_revision_is_exact_successor<br>agent_inputs.rejected_revision_does_not_mutate<br>agent_inputs.same_revision_same_content_is_idempotent | - | 34 | passed 0s | S | a: passed 0s |
| 39 | awaken-config-service | `publication::publication_resolution_invokes_exactly_one_owner` | config.inference_access_resolved_once_at_publication | - | 35 | passed 0s | S | a: passed 0s |
| 40 | awaken-config-service | `tool_catalog::verification::only_the_reserved_scope_selects_admin_catalog_membership` | tool_policy.scoped_catalog_admin_membership_is_exact_and_non_disclosing | - | 35 | passed 0s | S | a: passed 0s G1 |
| 41 | awaken-config-service | `tool_catalog::verification::every_non_reserved_scope_selects_strictly_global_membership` | tool_policy.scoped_catalog_admin_membership_is_exact_and_non_disclosing | - | 35 | passed 0s | S | a: passed 0s G1 |
| 42 | awaken-config-store | `publication_source_admission_requires_present_exact_revision` | config_activation.stale_publication_never_installs | - | 36 | passed 0s | S | a: passed 0s |
| 43 | awaken-config-store | `config_revision_admission_is_exact_and_nonwrapping` | config.generation_is_monotonic<br>config.stale_writer_never_applies | - | 36 | passed 2s | S | a: passed 3s |
| 44 | awaken-config-store | `management_effect_retirement_requires_present_matching_receipt` | resource_binding.pending_effect_survives_until_readback<br>resource_binding.replay_is_idempotent_and_identity_stable | - | 36 | passed 0s | S | a: passed 0s |
| 45 | awaken-control | `authz::profiles::verification::runtime_member_role_contains_exactly_run_read` | authz.hosted_workspace_and_runtime_member_grants_are_exact | - | 37 | passed 0s | S | a: passed 0s |
| 46 | awaken-control | `authz::roles::verification::run_backed_route_policy_uses_exact_run_actions` | authz.run_backed_protocol_routes_use_the_exact_run_action_namespace | - | 37 | passed 0s | S | a: passed 0s |
| 47 | awaken-control | `authz::roles::verification::application_route_policy_never_enters_the_service_guard` | authz.application_protocol_is_scoped_by_application_guard_not_service_guard | - | 37 | passed 0s | S | a: passed 0s |
| 48 | awaken-control | `authz::roles::verification::credential_ingress_role_contains_only_apikey_and_provider_connect_authority` | authz.credential_ingress_role_is_exact_workspace_apikey_and_provider_connect_only | - | 37 | passed 0s | S | a: passed 0s |
| 49 | awaken-control | `authz::roles::verification::agent_publisher_role_contains_only_workspace_model_read_and_skill_authority` | authz.agent_publisher_role_is_exact_workspace_agent_model_read_and_skill_only | - | 37 | passed 0s | S | a: passed 0s |
| 50 | awaken-control | `authz::roles::verification::hosted_admin_role_contains_exactly_its_seven_workspace_authorities` | authz.hosted_workspace_and_runtime_member_grants_are_exact | - | 37 | passed 0s | S | a: passed 0s G2 |
| 51 | awaken-control | `authz::roles::verification::hosted_builder_role_has_writes_without_apikey_or_model_administration` | authz.hosted_workspace_and_runtime_member_grants_are_exact | - | 37 | passed 0s | S | a: passed 0s G2 |
| 52 | awaken-control | `authz::roles::verification::workspace_member_role_contains_exactly_read_only_workspace_authorities` | authz.hosted_workspace_and_runtime_member_grants_are_exact | - | 38 | passed 0s | S | a: passed 0s G2 |
| 53 | awaken-control | `authz::roles::verification::legacy_workspace_binding_migration_is_idempotent_and_authority_exact` | authz.legacy_workspace_binding_migration_is_authority_preserving_and_idempotent | - | 37 | passed 0s | S | a: passed 1s |
| 54 | awaken-credential-contract | `attempt::credential_realization_receipt_requires_every_exact_binding_coordinate` | credential_effect.boundary_preserves_exact_materialization_authority | - | 39 | passed 2s | S | a: passed 2s |
| 55 | awaken-credential-contract | `attempt::attempt_delivery_requires_unique_exact_binding_envelope_and_unchanged_authority` | credential.attempt_delivery_is_exact_epoch_bound_and_authority_preserving | - | 39 | passed 0s | S | a: passed 0s |
| 56 | awaken-credential-contract | `delivery_plan::credential_effect_delivery_policy_is_total_and_never_mixes_route_authority` | credential_effect.repository_git_delivery_excludes_runtime_host_and_mixed_route_authority | - | 39 | passed 0s | S | a: passed 0s |
| 57 | awaken-credential-contract | `formal::durable_credential_material_source_has_no_inline_plaintext_class` | worker.durable_dispatch_contains_no_plaintext_secret | - | 39 | passed 0s | S | a: passed 0s |
| 58 | awaken-credential-contract | `formal::environment_credential_custody_selects_exactly_one_authorized_profile` | environment.cloud_native_credential_custody_selection_is_exact | - | 39 | passed 0s | S | a: passed 0s |
| 59 | awaken-credential-contract | `formal::credential_envelope_issuance_accepts_exactly_the_complete_claim` | credential_envelope.issuance_requires_exact_claim | - | 39 | passed 0s | S | a: passed 0s |
| 60 | awaken-credential-contract | `formal::worker_plaintext_holder_projection_is_exact_and_non_widening` | credential.worker_plaintext_holder_projection_is_exact_and_non_widening | - | 39 | passed 0s | S | a: passed 0s |
| 61 | awaken-credential-contract | `http_effect::verification::named_material_shape_is_exact_and_non_widening` | credential.http_effect_usage_binds_exact_fields_and_placements | - | 39 | passed 0s | S | a: passed 0s |
| 62 | awaken-credential-contract | `managed_rollout::rollout_update_revision_is_monotonic_and_covers_event` | credential.managed_rollout_abstract_ack_requires_every_target_adoption | solver-kissat-v1 | 3 | passed 0s | S | a: passed 0s |
| 63 | awaken-credential-contract | `managed_rollout::identity_proofs::managed_rollout_identity_requires_every_payload_field` | credential.managed_rollout_event_id_replay_requires_exact_payload_and_colliding_ack_is_rejected | solver-minisat-v1/nightly | 4 | passed 227s | L | a: passed 362s |
| 64 | awaken-credential-contract | `observation_admission::worker_reference_id_is_derived_from_real_refs` | worker_credential.exact_revision_and_current_epoch_required | - | 40 | passed 0s | S | a: passed 1s G3 |
| 65 | awaken-credential-contract | `observation_admission::worker_reference_revision_comparison_is_full_width` | worker_credential.exact_revision_and_current_epoch_required | - | 40 | passed 0s | S | a: passed 0s G3 |
| 66 | awaken-credential-contract | `observation_admission::exact_worker_reference_maps_every_observation_state` | worker_credential.expired_observation_never_executes | - | 40 | passed 0s | S | a: passed 1s |
| 67 | awaken-credential-contract | `realization_capabilities::verification::mcp_delivery_selection_has_only_the_explicit_pair_classes` | credential.mcp_delivery_mode_is_frozen_and_never_falls_back<br>credential.mcp_delivery_selection_is_exact_and_fail_closed | - | 40 | passed 0s | S | a: passed 0s |
| 68 | awaken-credential-contract | `realization_capabilities::verification::mcp_delivery_receipt_requires_exact_binding_holder_and_mechanism` | credential.mcp_delivery_mode_is_frozen_and_never_falls_back<br>credential.mcp_delivery_receipt_requires_exact_binding_holder_and_mechanism | - | 40 | passed 0s | S | a: passed 0s |
| 69 | awaken-credential-contract | `realization_capabilities::profile_admission::verification::credential_profile_admission_requires_one_complete_material_tuple` | credential.material_profile_admission_preserves_complete_tuples | - | 40 | passed 1s | S | a: passed 3s |
| 70 | awaken-credential-materializer | `platform_relay::verification::platform_relay_local_gate_admits_only_exact_platform_relay` | credential.platform_relay_requires_exact_holder_workspace_usage_and_target | - | 41 | passed 0s | S | a: passed 0s |
| 71 | awaken-credential-vault | `catalog::workspace_proofs::managed_credential_insert_checks_actual_workspace_and_parent` | credential.vault_workspace_authority_admission_is_exact_and_non_widening | - | 43 | passed 8s | S | a: passed 23s |
| 72 | awaken-credential-vault | `catalog::verification::injection_location_represents_exactly_the_three_usable_states` | credential.injection_location_has_exactly_three_usable_states_and_partial_updates_preserve_validity | - | 42 | passed 0s | S | a: passed 0s |
| 73 | awaken-credential-vault | `catalog::verification::injection_location_partial_update_is_exact_and_never_allows_both_disabled` | credential.injection_location_has_exactly_three_usable_states_and_partial_updates_preserve_validity | - | 42 | passed 0s | S | a: passed 0s |
| 74 | awaken-credential-vault | `catalog::verification::managed_vault_workspace_admission_is_exact_and_non_widening` | credential.vault_workspace_authority_admission_is_exact_and_non_widening | - | 42 | passed 0s | S | a: passed 0s |
| 75 | awaken-credential-vault | `catalog::verification::managed_credential_insert_requires_every_aggregate_invariant` | credential.managed_vault_child_admission_requires_active_parent_capacity_and_unique_key | - | 42 | passed 0s | S | a: passed 0s |
| 76 | awaken-credential-vault | `catalog::verification::managed_vault_replacement_requires_exact_revision_successor` | credential.managed_vault_replacement_rejects_stale_and_non_successor_revisions | - | 42 | passed 0s | S | a: passed 0s |
| 77 | awaken-credential-vault | `catalog::verification::managed_vault_delete_is_monotonic_identity_bound_and_absorbing` | credential.managed_vault_delete_fences_children_waits_for_rollout_and_is_absorbing | - | 42 | passed 0s | S | a: passed 0s |
| 78 | awaken-credential-vault | `catalog::verification::managed_credential_replacement_requires_both_fences_and_exact_successor` | credential.managed_mutation_requires_exact_source_and_child_successor_fences | - | 42 | passed 0s | S | a: passed 0s |
| 79 | awaken-credential-vault | `catalog::verification::deleted_managed_credential_is_absorbing` | credential.managed_deleted_lifecycle_is_absorbing | - | 42 | passed 0s | S | a: passed 0s |
| 80 | awaken-credential-vault | `verification::provider_scope_never_widens_endpoint_scope` | credential.provider_scope_widening_requires_exact_revision_provider_and_endpoint | - | 46 | passed 0s | S | a: passed 2s |
| 81 | awaken-credential-vault | `verification::disabled_credential_pool_members_are_never_eligible` | credential_pool.disabled_member_is_ineligible | - | 46 | passed 0s | S | a: passed 0s |
| 82 | awaken-credential-vault | `verification::credential_cooldown_boundary_is_exact_and_inclusive` | credential_pool.cooldown_boundary_is_exact | - | 46 | passed 0s | S | a: passed 0s |
| 83 | awaken-credential-vault | `verification::exhausted_credentials_are_unavailable_at_every_time` | credential_pool.exhausted_member_is_unavailable | - | 46 | passed 0s | S | a: passed 0s |
| 84 | awaken-credential-vault | `verification::a_pool_with_no_enabled_available_member_fails_closed` | credential_pool.empty_eligible_set_fails_closed | - | 45 | passed 0s | S | a: passed 1s |
| 85 | awaken-credential-vault | `local_material::new_materialization_admits_only_active_credential_status` | credential_effect.boundary_preserves_exact_materialization_authority<br>worker.revoked_credential_blocks_new_materialization | - | 43 | passed 0s | S | a: passed 0s |
| 86 | awaken-credential-vault | `local_material::broker::kani_proofs::exact_vault_revision_accepts_only_a_positive_matching_or_unpinned_source` | credential_reference.exact_revision_never_resolves_another_revision | - | 43 | passed 0s | S | a: passed 0s |
| 87 | awaken-credential-vault | `repo::managed_rollout_ack_requires_converged_progress` | credential.managed_rollout_abstract_ack_requires_every_target_adoption<br>credential.managed_rollout_ack_requires_converged_progress | - | 44 | passed 0s | S | a: passed 0s |
| 88 | awaken-credential-vault | `repo::managed_rollout_delivery_acknowledges_only_exact_convergence` | credential.managed_rollout_failure_or_busy_session_remains_pending | - | 44 | passed 0s | S | d: harness edited after last run |
| 89 | awaken-credential-vault | `repo::managed_vault_delete_fence_allows_only_absorbing_child_delete` | credential.managed_vault_delete_fences_children_waits_for_rollout_and_is_absorbing | - | 44 | passed 0s | S | a: passed 0s |
| 90 | awaken-credential-vault | `repo::publication_proofs::managed_creation_begin_rejects_every_published_identity` | credential.managed_creation_begin_rejects_every_published_source_or_child_identity | - | 45 | passed 2s | S | a: passed 4s |
| 91 | awaken-credential-vault | `repo::publication_proofs::managed_rollout_projection_preserves_committed_identity` | credential.managed_creation_publishes_source_and_child_atomically<br>credential.managed_rollout_outbox_is_atomic_with_exact_pair_publication | - | 45 | passed 34s | M | a: passed 80s |
| 92 | awaken-credential-vault | `repo::publication_proofs::source_mutation_publication_is_exact_revision_cas` | credential.rotation_revision_cas | - | 45 | passed 10s | S | a: passed 26s |
| 93 | awaken-credential-vault | `repo::managed_creation_pair_requires_every_binding_axis` | credential.managed_creation_pair_requires_exact_workspace_source_and_initial_lifecycle | - | 43 | passed 0s | S | a: passed 1s |
| 94 | awaken-credential-vault | `repo::managed_mutation_operation_shape_is_closed_and_delete_is_absorbing` | credential.managed_mutation_requires_exact_source_and_child_successor_fences | - | 43 | passed 0s | S | a: passed 0s |
| 95 | awaken-credential-vault | `repo::cleanup::credential_reclamation_never_deletes_shared_or_live_predecessor_material` | credential.failed_create_compensates_material<br>credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed<br>credential.restart_recovery_compensates_unpublished_material | - | 43 | passed 0s | S | a: passed 0s |
| 96 | awaken-credential-vault | `repo::cleanup::credential_cleanup_requires_exact_authority_and_phase_truth` | credential.failed_create_compensates_material<br>credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed<br>credential.material_is_always_published_or_intent_tracked<br>credential.restart_recovery_compensates_unpublished_material | - | 43 | passed 0s | S | a: passed 0s |
| 97 | awaken-credential-vault | `repo::inventory::credential_inventory_never_orphans_committed_pending_or_foreign_material` | credential.material_is_always_published_or_intent_tracked<br>credential_inventory.committed_material_is_preserved<br>credential_inventory.orphans_are_reported_and_retained_without_an_atomic_claim<br>credential_inventory.pending_intent_material_is_preserved | - | 43 | passed 0s | S | a: passed 0s |
| 98 | awaken-credential-vault | `repo::material_mutation::material_input_proofs::credential_material_input_facts_without_predecessor` | credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed<br>credential.managed_material_admission_predicate_requires_owner_and_namespace_match | minisat-unwind-memcmp-16-v1/nightly | 5 | passed 122s | L | a: passed 305s |
| 99 | awaken-credential-vault | `repo::material_mutation::credential_material_attempt_requires_owner_and_exact_physical_namespace` | credential.managed_material_admission_predicate_requires_owner_and_namespace_match<br>credential.material_is_always_published_or_intent_tracked<br>credential.visible_row_always_has_material | - | 44 | passed 0s | S | a: passed 0s |
| 100 | awaken-credential-vault | `repo::material_mutation::phase_proofs::credential_material_phase_transitions_preserve_exact_writer_fence` | credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges | nightly | 44 | passed 77s | M | a: passed 118s |
| 101 | awaken-credential-vault | `repo::material_mutation::phase_proofs::takeover_nonwriting_or_live_never_generates_token` | credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges | - | 45 | passed 1s | S | a: passed 2s |
| 102 | awaken-credential-vault | `repo::material_mutation::phase_proofs::takeover_rejects_expired_successor_before_entropy` | credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges | - | 45 | passed 1s | S | a: passed 1s |
| 103 | awaken-credential-vault | `repo::material_mutation::phase_proofs::takeover_exhausted_epoch_cannot_wrap` | credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges | - | 45 | passed 1s | S | a: passed 2s |
| 104 | awaken-credential-vault | `repo::material_mutation::phase_proofs::takeover_success_preserves_attempt_and_cannot_restore_original_writer` | credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges<br>credential.restart_recovery_compensates_unpublished_material | nightly | 45 | passed 20s | S | a: passed 30s |
| 105 | awaken-credential-vault | `repo::material_mutation::phase_proofs::credential_material_recovery_action_is_total_and_expiry_exact` | credential.failed_create_compensates_material<br>credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges<br>credential.material_is_always_published_or_intent_tracked<br>credential.restart_recovery_compensates_unpublished_material<br>credential.visible_row_always_has_material | - | 44 | passed 0s | S | a: passed 4s |
| 106 | awaken-credential-vault | `repo::material_mutation::phase_proofs::expired_writing_recovery_reaches_abort_cleanup` | credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges | - | 44 | passed 0s | S | a: passed 1s |
| 107 | awaken-credential-vault | `repo::material_mutation::phase_proofs::ready_recovery_reaches_published_cleanup` | credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges | - | 44 | passed 0s | S | a: passed 1s |
| 108 | awaken-data-governance-contract | `decision::kani_proofs::capture_decision_is_the_exact_meet_and_never_exceeds_any_layer` | capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed | - | 47 | passed 0s | S | d: harness edited after last run |
| 109 | awaken-data-governance-contract | `decision::kani_proofs::unreadable_workspace_narrowing_closes_capture` | capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed | - | 47 | passed 0s | S | d: harness edited after last run |
| 110 | awaken-data-governance-contract | `decision::kani_proofs::capture_reason_names_the_exact_binding_cause` | capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed | - | 47 | passed 0s | S | d: harness edited after last run |
| 111 | awaken-data-subject-application | `erasure_verification::erasure_progress_selects_exact_first_pending_target` | erasure.each_target_is_counted_once | erasure-progress-selection-v1/nightly | 7 | passed 10s | S | d: loop profile repinned after last run |
| 112 | awaken-data-subject-application | `erasure_verification::erasure_progress_bounded_count_adds_selected_target_once` | erasure.each_target_is_counted_once | nightly | 48 | passed 9s | S | a: passed 9s G4 |
| 113 | awaken-data-subject-application | `erasure_verification::erasure_progress_near_max_count_preserves_exact_overflow_boundary` | erasure.each_target_is_counted_once | nightly | 48 | passed 10s | S | a: passed 11s G4 |
| 114 | awaken-data-subject-application | `erasure_verification::erasure_progress_record_preserves_other_target_rows_and_flags` | erasure.each_target_is_counted_once | nightly | 48 | passed 12s | S | a: passed 12s |
| 115 | awaken-data-subject-application | `erasure_verification::erasure_progress_recorded_target_is_not_selected_again` | erasure.each_target_is_counted_once | nightly | 48 | passed 16s | S | a: passed 19s |
| 116 | awaken-data-subject-application | `erasure_verification::erasure_checkpoint_action_requires_all_targets_and_accountability` | erasure.accountability_stamp_follows_all_targets<br>erasure.completion_requires_accountability_stamp | erasure-progress-selection-v1/nightly | 7 | passed 13s | S | d: loop profile repinned after last run |
| 117 | awaken-data-subject-application | `erasure_verification::erasure_checkpoint_successor_preserves_all_completed_targets` | erasure.checkpoint_never_forgets_completed_targets | erasure-checkpoint-successor-v1 | 6 | passed 7s | S | d: loop profile repinned after last run |
| 118 | awaken-data-subject-application | `verification::any_withdrawal_vetoes_full_content_capture` | consent.any_withdrawal_vetoes_full_capture | - | 48 | passed 0s | S | a: passed 0s |
| 119 | awaken-data-subject-application | `verification::consent_upsert_leaves_exactly_one_row_for_the_incoming_purpose` | consent.upsert_retains_one_purpose_row | - | 48 | passed 0s | S | a: passed 0s |
| 120 | awaken-data-subject-application | `verification::erasure_withdrawal_is_absorbing_and_idempotent` | consent.erasure_withdrawal_is_absorbing | - | 48 | passed 0s | S | a: passed 0s |
| 121 | awaken-data-subject-application | `verification::revision_advance_is_strict_or_explicitly_exhausted` | data_subject.revision_is_monotonic<br>data_subject.revision_is_strict_or_exhausted | - | 48 | passed 0s | S | a: passed 0s |
| 122 | awaken-data-subject-application | `verification::stale_data_subject_writer_is_never_admitted` | data_subject.stale_writer_never_applies | - | 49 | passed 0s | S | a: passed 1s |
| 123 | awaken-deployment-config | `verification::container_hand_residency_recovery_mapping_is_total_exact_and_non_widening` | worker.sandbox_tool_recovery_is_a_hard_claim_axis | - | 50 | passed 0s | S | a: passed 0s |
| 124 | awaken-deployment-config | `verification::durable_dispatch_admission_is_exact_and_commit_store_independent` | run_ingress.durable_dispatch_deployment_admission_is_exact_store_independent_and_evidence_monotonic | - | 50 | passed 0s | S | d: harness edited after last run |
| 125 | awaken-deployment-config | `verification::sandbox_support_projection_is_total_exact_and_evidence_bound` | sandbox.deployment_support_projection_is_total_exact_and_evidence_bound | - | 50 | passed 0s | S | d: harness edited after last run |
| 126 | awaken-deployment-contract | `schedule::minute_timestamp_projection_is_exact_and_bounded` | scheduled_work.minute_timestamp_projection_is_exact_bounded_and_in_domain | - | 51 | passed 21s | S | a: passed 36s G5 |
| 127 | awaken-deployment-contract | `schedule::minute_timestamp_projection_has_an_exact_input_domain` | scheduled_work.minute_timestamp_projection_is_exact_bounded_and_in_domain | - | 51 | passed 0s | S | a: passed 0s G5 |
| 128 | awaken-dispatch | `clock::store_deadline_is_monotonic_saturating_and_never_wraps` | run_ingress.store_deadline_is_monotonic_saturating_and_never_wraps<br>session_run.reservation_ttl_uses_store_clock | - | 52 | passed 0s | S | a: passed 0s |
| 129 | awaken-dispatch | `dispatch::completed_identity_proofs::agent_message_completed_tombstone_replays_only_exact_dispatch_identity` | session.agent_message_completed_target_allows_late_source_receipt_without_reexecution<br>session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters | - | 52 | passed 0s | S | a: passed 0s |
| 130 | awaken-dispatch | `dispatch::claim_binding_proofs::dispatch_claim_preserves_every_frozen_binding` | delegation.independent_child_scheduling_lease<br>remote_worker.execution_requires_claimed_snapshot<br>session_run.recovery_claim_is_admission_only<br>worker.retry_preserves_pinned_binding | - | 52 | passed 0s | S | d: harness edited after last run |
| 131 | awaken-dispatch | `inbox_mutation::pending_input_mutation_plan_is_exact_revision_fenced_and_nonwrapping` | ingress.pending_mutation_plan_requires_exact_revision_and_nonwrapping_edit | - | 52 | passed 0s | S | a: passed 0s |
| 132 | awaken-dispatch-contract | `dispatch::commit_epoch_completion_requires_observed_release` | ingress.commit_epoch_atomic_local<br>ingress.commit_epoch_atomic_remote_custom<br>remote_worker.stale_epoch_never_mutates_truth | - | 53 | passed 0s | S | a: passed 0s |
| 133 | awaken-dispatch-contract | `dispatch::commit_epoch_guard_admits_only_complete_exact_claim` | ingress.commit_epoch_atomic_remote_custom<br>remote_worker.committed_truth_is_coordinator_owned<br>remote_worker.execution_requires_claimed_snapshot<br>remote_worker.single_live_claim_owner<br>remote_worker.stale_epoch_never_mutates_truth | - | 53 | passed 0s | S | a: passed 0s |
| 134 | awaken-dispatch-contract | `dispatch::attempt_binding::physical_binding_cannot_change_on_admission_retry` | session_execution.one_physical_attempt_per_thread | - | 53 | passed 0s | S | a: passed 0s |
| 135 | awaken-dispatch-contract | `dispatch::credential_admission::claim_credential_operation_is_total_and_terminal_dominates` | credential_effect.boundary_preserves_exact_materialization_authority | - | 53 | passed 0s | S | a: passed 0s |
| 136 | awaken-dispatch-contract | `dispatch::credential_admission::mcp_requirement_route_admits_only_bearer_relay_or_workload_injection` | credential.mcp_claim_admits_only_when_every_requirement_is_installed | - | 53 | passed 0s | S | d: harness edited after last run |
| 137 | awaken-dispatch-contract | `dispatch::credential_admission::claim_admits_session_mcp_credentials_only_when_every_requirement_is_admitted` | credential.mcp_claim_admits_only_when_every_requirement_is_installed | - | 53 | never | ~S | d: never recorded |
| 138 | awaken-dispatch-contract | `dispatch::credential_admission::claim_credential_binding_revalidation_rejects_missing_widened_reordered_or_stale_wire` | credential_effect.boundary_preserves_exact_materialization_authority | - | 53 | passed 14s | S | d: harness edited after last run |
| 139 | awaken-dispatch-contract | `dispatch::model::absent_deadline_never_passes_for_any_now` | run_ingress.a_deadline_passes_once_it_is_reached | - | 53 | passed 0s | S | a: passed 0s G6 |
| 140 | awaken-dispatch-contract | `dispatch::model::bounded_deadline_passes_once_it_is_reached` | run_ingress.a_deadline_passes_once_it_is_reached | - | 54 | passed 28s | S | d: harness edited after last run G6 |
| 141 | awaken-dispatch-contract | `dispatch::model::lease_renewal_observation_preserves_claim_and_attempt_fences` | run_ingress.renewal_projects_cancel_only_to_exact_physical_attempt | - | 54 | passed 0s | S | a: passed 0s |
| 142 | awaken-dispatch-contract | `dispatch::model::pre_start_no_effect_ack_requires_fresh_install_admission` | session_execution.pre_start_cancel_ack_requires_fresh_slot_install | - | 54 | passed 0s | S | a: passed 0s |
| 143 | awaken-dispatch-contract | `dispatch::model::session_replacement_proofs::only_awaiting_or_already_superseded_is_replacement_safe` | run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing | - | 54 | passed 0s | S | a: passed 0s |
| 144 | awaken-dispatch-contract | `dispatch::pending_settlement_plan_is_the_exact_dispatch_projection` | ingress.pending_input_exactly_once | - | 54 | passed 0s | S | a: passed 0s |
| 145 | awaken-dispatch-contract | `dispatch_transition::proofs::quarantine_admission_never_widens_settlement_or_execution` | session_execution.one_physical_attempt_per_thread | - | 55 | passed 0s | S | a: passed 0s |
| 146 | awaken-dispatch-contract | `dispatch_transition::proofs::quarantined_cancelled_row_can_never_re_enter_execution` | session_execution.one_physical_attempt_per_thread | - | 55 | passed 0s | S | a: passed 0s |
| 147 | awaken-dispatch-contract | `dispatch_transition::proofs::retired_epoch_cannot_admit_another_execution` | session_execution.one_physical_attempt_per_thread | - | 55 | passed 0s | S | a: passed 0s |
| 148 | awaken-dispatch-contract | `dispatch_transition::proofs::physical_attempt_slot_installs_and_clears_only_exact_authority` | remote_worker.execution_requires_claimed_snapshot<br>remote_worker.single_live_claim_owner<br>session_execution.one_physical_attempt_per_thread<br>worker_credential.exact_revision_and_current_epoch_required<br>worker_credential.expired_observation_never_executes | - | 55 | passed 0s | S | a: passed 0s |
| 149 | awaken-dispatch-contract | `dispatch_transition::proofs::thread_physical_peer_vetoes_begin_and_handoff` | session_execution.one_physical_attempt_per_thread | - | 56 | passed 0s | S | a: passed 0s |
| 150 | awaken-dispatch-contract | `dispatch_transition::proofs::physical_attempt_slot_never_normalizes_a_torn_owner_epoch_pair` | remote_worker.single_live_claim_owner<br>session_execution.one_physical_attempt_per_thread | - | 55 | passed 0s | S | a: passed 0s |
| 151 | awaken-dispatch-contract | `dispatch_transition::proofs::stale_dispatch_claim_cannot_modify_authoritative_state` | ingress.settle_fence<br>run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing | - | 56 | passed 0s | S | a: passed 0s |
| 152 | awaken-dispatch-contract | `dispatch_transition::proofs::relinquish_requires_exact_current_claim_and_preserves_retry_budget` | run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget | - | 55 | passed 0s | S | a: passed 0s |
| 153 | awaken-dispatch-contract | `dispatch_transition::proofs::failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion` | run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion | - | 55 | passed 0s | S | a: passed 0s |
| 154 | awaken-dispatch-contract | `dispatch_transition::proofs::exact_dispatch_settlement_is_terminal_or_awaiting_only` | ingress.settle_fence<br>run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing<br>session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement<br>session_execution.initial_event_path_reaches_idle_only_after_full_settlement | - | 55 | passed 0s | S | a: passed 0s |
| 155 | awaken-dispatch-contract | `dispatch_transition::proofs::dispatch_cancel_revokes_old_epoch_and_is_idempotent` | run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing | - | 54 | passed 0s | S | a: passed 0s |
| 156 | awaken-dispatch-contract | `dispatch_transition::proofs::dispatch_claim_mints_exactly_one_epoch_and_never_reopens_closed_rows` | delegation.independent_child_scheduling_lease<br>ingress.epoch_reclaim<br>ingress.single_owner<br>remote_worker.single_live_claim_owner<br>run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing<br>session_run.recovery_claim_is_admission_only | - | 54 | passed 0s | S | d: harness edited after last run |
| 157 | awaken-dispatch-contract | `dispatch_transition::proofs::dispatch_maintenance_transitions_are_closed_and_exact` | run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing | - | 54 | passed 0s | S | a: passed 0s |
| 158 | awaken-dispatch-contract | `run_dispatch::dispatch_admission_shape_keeps_child_affinity_independent` | delegation.independent_child_scheduling_lease | - | 56 | passed 0s | S | a: passed 0s |
| 159 | awaken-dispatch-execution | `attempt_termination::native_coverage_must_be_committed_strictly_before_model_entry` | model_operation.coverage_commit_strictly_precedes_model_entry | - | 57 | passed 0s | S | a: passed 0s |
| 160 | awaken-dispatch-execution | `attempt_termination::native_attempt_termination_requires_every_closed_surface_fact` | sandbox.k8s_process_observation_requires_complete_exit | - | 57 | passed 0s | S | d: harness edited after last run |
| 161 | awaken-dispatch-execution | `pool::drain_admission_proofs::pool_draining_state_is_absorbing_and_claim_closed` | worker_drain.closed_generation_is_absorbing<br>worker_drain.no_claim_after_acknowledgement<br>worker_drain.no_claim_attempt_after_registry_drain<br>worker_drain.registry_transition_follows_local_fence | - | 57 | passed 0s | S | a: passed 0s |
| 162 | awaken-dispatch-execution | `pool::drain_error_proofs::resolution_retry_admission_matches_only_the_persisted_deadline` | run_ingress.request_local_resolution_retry_honors_persisted_deadline | - | 57 | passed 0s | S | a: passed 0s |
| 163 | awaken-dispatch-execution | `pool::drain_error_proofs::shared_error_gate_admits_exactly_one_probe_after_the_deadline` | run_ingress.pool_retry_gate_admits_one_probe_and_uses_bounded_monotonic_backoff | - | 57 | passed 0s | S | a: passed 0s |
| 164 | awaken-dispatch-execution | `pool::drain_error_proofs::probe_release_is_exact_and_preserves_backoff` | run_ingress.pool_retry_gate_admits_one_probe_and_uses_bounded_monotonic_backoff | - | 57 | passed 1s | S | a: passed 1s |
| 165 | awaken-dispatch-execution | `pool::drain_error_proofs::drain_error_backoff_is_bounded_monotonic_and_saturating` | run_ingress.pool_retry_gate_admits_one_probe_and_uses_bounded_monotonic_backoff | - | 57 | passed 1s | S | a: passed 1s |
| 166 | awaken-dispatch-execution | `process_dispatch::drain_completion_proofs::drain_completion_requires_owned_tasks_and_physical_quiescence` | worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence<br>worker_drain.grace_expiry_retains_cancellation_observer | - | 57 | passed 1s | S | a: passed 1s |
| 167 | awaken-dispatch-execution | `reconciliation::page_proofs::terminal_scan_page_preserves_budget_cursor_and_cancellation` | remote_worker.terminal_scan_is_bounded_and_preserves_authority | - | 58 | passed 1s | S | a: passed 1s |
| 168 | awaken-dispatch-execution | `recovery_projection::verification::recovery_snapshot_admission_requires_exact_claim_identity` | remote_worker.recovery_projection_is_committed_prefix | - | 58 | passed 1s | S | a: passed 1s |
| 169 | awaken-dispatch-execution | `recovery_projection::verification::acknowledged_projection_is_exact_monotonic_or_a_noop` | remote_worker.acknowledged_projection_is_monotonic<br>remote_worker.recovery_projection_is_committed_prefix | - | 58 | passed 0s | S | a: passed 1s |
| 170 | awaken-dispatch-execution | `service::maintenance_schedule_exists_only_for_the_owning_adapter` | remote_worker.maintenance_scheduler_respects_adapter_ownership | - | 58 | passed 0s | S | a: passed 0s |
| 171 | awaken-dispatch-execution | `session_envelope::proofs::dispatched_resource_manifest_requires_exact_nonempty_scope` | session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin | - | 59 | passed 0s | S | d: harness edited after last run |
| 172 | awaken-dispatch-execution | `session_worker_resolver::claim_failure::session_realization_control_disposition_projects_exact_worker_effect` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 59 | passed 0s | S | a: passed 0s |
| 173 | awaken-dispatch-execution | `session_worker_resolver::claim_failure::a_deferral_is_due_no_later_than_its_owners_hint_or_the_backoff` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 59 | passed 1s | S | d: harness edited after last run |
| 174 | awaken-dispatch-execution | `worker::pending_signal_is_deliverable_only_without_consuming_commit` | remote_worker.pending_signal_is_consumed_by_commit<br>tool_result.terminal_run_rejects_late_result | - | 58 | passed 0s | S | a: passed 0s |
| 175 | awaken-dispatch-execution | `worker::claimed_snapshot_execution_requires_exact_identity` | remote_worker.execution_requires_claimed_snapshot | - | 58 | passed 0s | S | a: passed 0s |
| 176 | awaken-durable-projection | `projection_refresh_domain_permit_is_exclusive` | executable_projection.concurrent_refresh_is_serialized_per_domain | - | 60 | passed 1s | S | a: passed 2s |
| 177 | awaken-durable-projection | `projection_install_reconciliation_preserves_cursor_and_monotonic_authority` | executable_projection.cursor_never_leads_installed_projection<br>executable_projection.installed_projection_never_leads_authority | - | 60 | passed 0s | S | a: passed 0s |
| 178 | awaken-environment-contract | `registration_intent_proofs::environment_registration_intent_is_exact_idempotent_and_recoverable` | environment.registration_intent_is_atomic_recoverable_and_idempotent | - | 61 | passed 0s | S | a: passed 0s |
| 179 | awaken-environment-contract | `network_projection_verification::declared_build_packages_never_expand_runtime_hosts` | environment.prepared_packages_do_not_grant_runtime_registry_access | - | 61 | never | ~L | d: never recorded |
| 180 | awaken-executable-agent-catalog | `installed_lifecycle_revision_never_rolls_back` | config_activation.installed_generation_never_rolls_back | - | 62 | passed 0s | S | a: passed 0s |
| 181 | awaken-executable-agent-contract | `session_profile::kani_proofs::requested_agent_profile_accepts_only_the_same_positive_revision` | agent_profile.requested_revision_never_follows_mutable_current | - | 63 | passed 0s | S | a: passed 0s |
| 182 | awaken-executable-agent-contract | `snapshot_pin::kani_proofs::executable_snapshot_pin_is_complete_exact_and_non_mixing` | executable_snapshot.pin_is_complete_exact_and_non_mixing | - | 63 | passed 0s | S | a: passed 0s |
| 183 | awaken-executable-environment-contract | `image_realization::verification::environment_image_build_claim_is_exact_non_wrapping_and_blocked_is_inert` | environment_image_build.single_claim_exact_reclaim_and_stale_fence | - | 64 | passed 0s | S | a: passed 0s |
| 184 | awaken-executable-environment-contract | `image_realization::verification::environment_image_build_mutation_requires_exact_claim_and_admissible_command` | environment_image_build.single_claim_exact_reclaim_and_stale_fence | - | 64 | passed 0s | S | a: passed 0s |
| 185 | awaken-executable-environment-contract | `session_work::verification::only_queued_work_is_claimable` | work.single_active_claim | - | 64 | passed 0s | S | a: passed 0s |
| 186 | awaken-executable-environment-contract | `session_work::verification::only_active_work_accepts_lease_extension` | work.heartbeat_compare_and_set | - | 64 | passed 0s | S | a: passed 0s |
| 187 | awaken-executable-environment-contract | `session_work::verification::work_lease_mutation_requires_present_exact_authority_and_matching_condition` | work.heartbeat_compare_and_set | - | 65 | passed 0s | S | a: passed 0s |
| 188 | awaken-executable-environment-contract | `session_work::verification::session_work_release_requires_exact_owner_and_epoch` | session_work.release_requires_exact_owner_and_epoch | - | 64 | passed 0s | S | a: passed 0s |
| 189 | awaken-executable-environment-contract | `session_work::verification::stop_is_absorbing_for_every_work_state` | work.terminal_lease_clear | - | 65 | passed 0s | S | a: passed 0s |
| 190 | awaken-executable-environment-contract | `session_work::verification::first_heartbeat_is_authorized_exactly_once` | work.pre_heartbeat_worker_identity | - | 64 | passed 0s | S | a: passed 0s |
| 191 | awaken-executable-environment-contract | `session_work::verification::matching_heartbeat_rejects_every_other_receipt` | work.heartbeat_compare_and_set | - | 64 | passed 0s | S | a: passed 0s |
| 192 | awaken-executable-environment-contract | `session_work::session_utc_projection_domain_is_total_exact_and_idempotent` | session_time.utc_projection_domain_is_total_exact_and_idempotent | - | 64 | passed 0s | S | a: passed 0s |
| 193 | awaken-execution-composition | `sandbox_source::acp_mcp_projection_has_one_exact_authority` | acp.fixed_official_launch_preserves_process_local_mcp_routes | - | 66 | passed 0s | S | a: passed 0s |
| 194 | awaken-ext-background-task | `model::background_execution_admission_is_exact_fenced_live_and_phase_monotone` | background_task.single_fenced_attempt | - | 67 | passed 0s | S | a: passed 0s |
| 195 | awaken-ext-background-task | `model::stale_fenced_heartbeat_and_finish_have_no_mutation_plan` | background_task.stale_fence_never_finishes_or_heartbeats | - | 67 | passed 4s | S | d: harness edited after last run |
| 196 | awaken-ext-background-task | `model::fixed_target_heartbeat_acknowledges_or_extends_only_a_live_current_tenure` | background_task.stale_fence_never_finishes_or_heartbeats | solver-z3-v1 | 8 | unfinished | XL | b: last run unfinished, text changed since |
| 197 | awaken-ext-background-task | `model::fixed_target_heartbeat_planner_outcomes_are_exact` | background_task.stale_fence_never_finishes_or_heartbeats | - | 67 | never | ~M | d: never recorded |
| 198 | awaken-ext-background-task | `model::fixed_target_heartbeat_replay_of_the_planned_successor_acknowledges` | background_task.stale_fence_never_finishes_or_heartbeats | - | 67 | never | ~M | d: never recorded |
| 199 | awaken-ext-background-task | `model::cancellation_is_monotone_and_terminal_states_are_absorbing` | background_task.cancellation_absorption | - | 67 | passed 0s | S | a: passed 0s |
| 200 | awaken-ext-background-task | `model::never_replay_recovery_requires_a_durable_remote_continuation` | background_task.never_replay_and_attempt_exhaustion_fail_closed<br>background_task.remote_continuation_survives_wait_cancel_and_reclaim | - | 67 | passed 0s | S | a: passed 0s |
| 201 | awaken-ext-background-task | `model::expired_reclaim_plan_advances_epoch_or_terminates` | background_task.expiry_reclaim_advances_epoch_or_terminates<br>background_task.never_replay_and_attempt_exhaustion_fail_closed | - | 67 | passed 0s | S | a: passed 1s |
| 202 | awaken-ext-background-task | `supervisor::process_completion_fold_retries_exact_candidate_and_stale_is_inert` | background_task.process_completion_is_retryable_and_stale_completion_is_inert | - | 67 | passed 2s | S | a: passed 2s |
| 203 | awaken-ext-compact | `fold::verification::fold_point_preserves_the_requested_suffix` | compaction.fold_preserves_suffix | - | 68 | passed 0s | S | a: passed 0s |
| 204 | awaken-ext-compact | `fold::verification::fold_point_is_present_exactly_when_triggered_with_nonempty_prefix` | compaction.trigger_is_total | - | 68 | passed 0s | S | a: passed 0s |
| 205 | awaken-ext-compact | `plugin::artifact_verification::compaction_artifact_requires_exact_identity_coverage_and_content` | compaction.artifact_identity_coverage_and_content_fail_closed | - | 68 | passed 0s | S | a: passed 0s |
| 206 | awaken-ext-goal | `outcome::proofs::applying_a_grade_obeys_decision_and_budget` | outcome.grade_transition_obeys_budget | - | 69 | passed 0s | S | a: passed 0s |
| 207 | awaken-ext-goal | `outcome::proofs::terminal_outcomes_are_absorbing` | outcome.terminal_absorption | - | 69 | passed 0s | S | a: passed 0s |
| 208 | awaken-ext-mcp | `router::proofs::notification_admission_is_exact_and_unknown_fails_closed` | mcp.notification_admission_is_exact_and_unknown_fails_closed | - | 70 | passed 1s | S | a: passed 2s |
| 209 | awaken-ext-mcp | `sensitive::proofs::sensitive_marker_never_projects_payload_publicly` | mcp.sensitive_marker_projection_is_never_public_or_widened | - | 70 | passed 0s | S | d: harness edited after last run |
| 210 | awaken-ext-mcp | `transport::mcp_call_admission_requires_both_accepting_observations` | credential.mcp_adopted_revocation_removes_accepting_generation_before_new_calls | - | 70 | passed 0s | S | a: passed 0s |
| 211 | awaken-ext-memory | `extraction::controller::verification::memory_extraction_unknown_is_terminal_and_never_retryable` | memory_extraction.unknown_execution_never_reenters_retry | - | 71 | passed 46s | M | a: passed 66s |
| 212 | awaken-ext-memory | `plugin::memory_recall_contribution_is_exact_and_disabled_is_inert` | memory.recall_config_contributes_authority_exactly_when_enabled | - | 71 | passed 0s | S | a: passed 0s |
| 213 | awaken-ext-permission | `kani_proofs::retired_mode_compiles_into_one_exact_unmatched_default` | permission.extension_policy_merge_is_exact_and_fail_closed | - | 72 | passed 0s | S | a: passed 0s |
| 214 | awaken-ext-permission | `kani_proofs::a_matching_deny_decides_the_whole_fold_whatever_else_matched` | permission.extension_policy_merge_is_exact_and_fail_closed | - | 72 | passed 5s | S | a: passed 7s |
| 215 | awaken-ext-permission | `kani_proofs::the_fold_returns_a_behavior_some_matching_rule_actually_carried` | permission.extension_policy_merge_is_exact_and_fail_closed | - | 72 | passed 4s | S | a: passed 6s |
| 216 | awaken-ext-permission | `kani_proofs::matched_deny_is_absolute_and_only_stricter_non_deny_replaces_authority` | permission.extension_policy_merge_is_exact_and_fail_closed | - | 72 | passed 0s | S | a: passed 0s |
| 217 | awaken-ext-state-machine | `machine::kani_proofs::tool_and_event_transitions_never_cross_trigger_kinds` | workflow_state.transitions_are_exact_typed_terminal_and_fail_closed | - | 73 | passed 0s | S | a: passed 0s |
| 218 | awaken-ext-state-machine | `machine::kani_proofs::state_transition_requires_every_exact_precondition` | workflow_state.transitions_are_exact_typed_terminal_and_fail_closed | - | 73 | passed 0s | S | a: passed 0s |
| 219 | awaken-ext-state-machine | `machine::kani_proofs::result_transition_and_authored_fallback_are_exact` | workflow_state.transitions_are_exact_typed_terminal_and_fail_closed | - | 73 | passed 0s | S | a: passed 0s |
| 220 | awaken-ext-state-machine | `machine::kani_proofs::terminal_state_classification_is_exact` | workflow_state.transitions_are_exact_typed_terminal_and_fail_closed | - | 73 | passed 0s | S | a: passed 0s |
| 221 | awaken-ext-state-machine | `machine::kani_proofs::unknown_transition_trigger_and_result_fail_closed` | workflow_state.transitions_are_exact_typed_terminal_and_fail_closed | - | 73 | passed 0s | S | a: passed 0s |
| 222 | awaken-file-store | `object::kani_proofs::object_store_configuration_finite_kernel_accepts_exactly_compatible_shapes` | object_store.configuration_is_secret_free_and_provider_exact | - | 74 | passed 2s | S | a: passed 3s |
| 223 | awaken-file-store | `object::kani_proofs::object_store_configuration_derives_s3_region_from_raw_representatives` | object_store.configuration_is_secret_free_and_provider_exact | - | 74 | passed 47s | M | a: passed 67s G7 |
| 224 | awaken-file-store | `object::kani_proofs::object_store_configuration_derives_bucket_prefix_from_raw_representatives` | object_store.configuration_is_secret_free_and_provider_exact | - | 74 | passed 34s | S | a: passed 54s G7 |
| 225 | awaken-file-store | `object::kani_proofs::object_store_configuration_derives_gcs_coordinates_from_raw_representatives` | object_store.configuration_is_secret_free_and_provider_exact | - | 74 | passed 46s | M | a: passed 69s G7 |
| 226 | awaken-file-store | `object::kani_proofs::object_store_configuration_classifies_bounded_ascii_blankness` | object_store.configuration_is_secret_free_and_provider_exact | - | 74 | passed 0s | S | a: passed 0s |
| 227 | awaken-file-store | `object::kani_proofs::object_store_configuration_classifies_bounded_ascii_prefix` | object_store.configuration_is_secret_free_and_provider_exact | - | 74 | passed 1s | S | a: passed 2s |
| 228 | awaken-file-store | `object::kani_proofs::object_store_configuration_classifies_bounded_dot_segments` | object_store.configuration_is_secret_free_and_provider_exact | - | 74 | passed 2s | S | a: passed 5s |
| 229 | awaken-lease-contract | `fence::verification::expiry_from_epoch_ms_is_strictly_monotone` | sandbox.effect_fence_succession_is_renewal_or_epoch_advance<br>session_realization.generation_authority_requires_every_immutable_axis | - | 75 | never | ~S | d: never recorded |
| 230 | awaken-lease-contract | `fence::verification::unix_nanos_of_epoch_ms_is_strictly_monotone` | run_ingress.a_deadline_passes_once_it_is_reached<br>session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence | - | 75 | never | ~S | d: never recorded |
| 231 | awaken-lease-contract | `fence::verification::fence_succession_is_exactly_renewal_or_epoch_advance` | sandbox.effect_fence_succession_is_renewal_or_epoch_advance | - | 75 | unfinished | XL | b: last run unfinished, text changed since |
| 232 | awaken-managed-session-events | `order::canonical_event_order_never_depends_on_wall_time` | session.runtime_projection_historical_pending_precedes_terminal_projection<br>session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix<br>session_time.event_order_is_independent_of_wall_time | solver-kissat-v1 | 9 | passed 0s | S | a: passed 0s |
| 233 | awaken-managed-session-events | `order::historical_pending_order_precedes_every_admitted_terminal` | session.runtime_projection_historical_pending_precedes_terminal_projection | solver-kissat-v1 | 9 | passed 0s | S | a: passed 0s |
| 234 | awaken-managed-session-events | `transcript::runtime_tool_projection_phase_is_total_distinct_and_fail_closed` | session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed | solver-kissat-v1 | 9 | passed 0s | S | a: passed 0s |
| 235 | awaken-mcp-server-core | `formal::one_request_has_at_most_one_final_response` | mcp.at_most_one_final_response | - | 76 | passed 0s | S | a: passed 0s |
| 236 | awaken-mcp-server-core | `formal::notifications_never_have_a_jsonrpc_response` | mcp.notification_has_no_response | - | 76 | passed 0s | S | a: passed 0s |
| 237 | awaken-mcp-server-core | `formal::final_response_is_progress_absorbing` | mcp.final_stops_progress | - | 76 | passed 0s | S | a: passed 0s |
| 238 | awaken-mcp-server-core | `formal::rejected_requests_never_enter_the_host` | mcp.rejected_request_skips_host | - | 76 | passed 0s | S | a: passed 0s |
| 239 | awaken-mcp-server-core | `formal::cancellation_never_produces_a_success_result` | mcp.cancellation_has_no_success_result | - | 76 | passed 0s | S | a: passed 0s |
| 240 | awaken-mcp-server-core | `http::verification::sessionless_http_lifecycle_routes_each_method_to_its_exact_action` | mcp.sessionless_stream_lifecycle | - | 76 | passed 0s | S | a: passed 0s |
| 241 | awaken-memory-store | `repository::memory_head_update_admission_preserves_stale_and_idempotent_heads` | memory.cas_has_one_winner_per_generation<br>memory.idempotent_write_does_not_advance_generation<br>memory.stale_cas_never_clobbers_live_content | - | 77 | passed 0s | S | a: passed 0s |
| 242 | awaken-memory-store | `repository::memory_conditional_delete_admission_is_exact` | memory.absent_conditional_delete_is_idempotent<br>memory.matched_conditional_delete_removes_exact_head<br>memory.stale_conditional_delete_never_removes_live_head | - | 77 | passed 0s | S | a: passed 0s |
| 243 | awaken-memory-store | `repository::memory_rename_replace_preserves_source_identity` | memory.rename_replace_preserves_source_identity | - | 77 | passed 0s | S | a: passed 0s |
| 244 | awaken-protocol-a2a | `encoder::proofs::a2a_task_state_projection_is_total_exact_and_non_strengthening` | protocol_adapter.a2a_task_state_projection_is_total_exact_and_non_strengthening | - | 78 | passed 0s | S | a: passed 0s |
| 245 | awaken-protocol-acp | `jsonrpc::handshake::acp_prompt_admission_requires_exact_completed_handshake` | acp.prompt_is_never_sent_before_bounded_handshake_completion | - | 79 | passed 0s | S | a: passed 0s |
| 246 | awaken-protocol-acp | `jsonrpc::mcp::acp_session_load_preserves_the_exact_mcp_projection` | acp.session_load_preserves_the_exact_mcp_projection | - | 79 | passed 0s | S | a: passed 0s |
| 247 | awaken-protocol-acp | `jsonrpc::permission::acp_permission_effect_plan_preserves_authority_order` | acp.boundary_preserves_permission_authority_and_effect_order | - | 79 | passed 0s | S | a: passed 0s |
| 248 | awaken-protocol-acp | `jsonrpc::permission::acp_permission_tool_identity_normalization_is_exact` | acp.permission_tool_identity_normalization_is_exact | - | 79 | passed 0s | S | a: passed 0s |
| 249 | awaken-protocol-acp | `jsonrpc::permission::acp_permission_projection_is_total_exact_and_non_widening` | acp.permission_outcome_never_widens_resolved_authority | - | 79 | passed 0s | S | a: passed 0s |
| 250 | awaken-protocol-acp | `jsonrpc::wire::proofs::acp_read_ahead_admission_never_overflows_or_exceeds_either_bound` | acp.read_ahead_admission_is_bounded_without_arithmetic_overflow | - | 79 | passed 0s | S | a: passed 0s |
| 251 | awaken-protocol-acp | `permission_consensus_is_a_conservative_order_independent_semilattice` | acp.mcp_alias_permission_consensus_never_widens_authority | - | 79 | passed 0s | S | a: passed 0s |
| 252 | awaken-protocol-ag-ui | `encoder::proofs::ag_ui_terminal_category_mapping_is_total_and_exact` | ui_stream.terminal_projection_is_exact_fail_closed_and_absorbing | - | 80 | passed 0s | S | a: passed 0s |
| 253 | awaken-protocol-ai-sdk | `encoder::proofs::ai_sdk_terminal_category_mapping_is_total_and_exact` | ui_stream.terminal_projection_is_exact_fail_closed_and_absorbing | - | 81 | passed 0s | S | a: passed 0s |
| 254 | awaken-protocol-managed | `common::headers::idempotency_key_scan_transition_is_exact_and_invalid_absorbing` | management_http.idempotency_key_admission_is_exact_and_fail_closed | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 255 | awaken-protocol-managed | `common::headers::idempotency_key_length_and_summary_policy_is_exact` | management_http.idempotency_key_admission_is_exact_and_fail_closed | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 256 | awaken-protocol-managed | `common::headers::idempotency_key_admission_rejects_empty_overlong_and_every_invalid_byte` | management_http.idempotency_key_admission_is_exact_and_fail_closed | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 257 | awaken-protocol-managed | `resources::memory_stores::memory_time_filter_compares_visible_milliseconds_without_merging_them` | session_time.utc_projection_domain_is_total_exact_and_idempotent | solver-z3-v1 | 13 | passed 0s | S | a: passed 0s |
| 258 | awaken-protocol-managed | `routes::user_profiles::user_profile_capability_and_field_projection_is_total_exclusive_and_exact` | managed_user_profiles.capability_and_field_projection_is_total_exclusive_and_exact | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 259 | awaken-protocol-managed | `state::events::inbound_admission_receipt_is_stable_across_processing_races` | session_event_batch.idempotent_receipt_is_admission_stable | solver-kissat-v1 | 12 | passed 0s | S | a: passed 0s |
| 260 | awaken-protocol-managed | `state::events::inbound_order_is_fixed_before_effect_observation` | session_event_batch.idempotent_receipt_is_admission_stable | solver-kissat-v1 | 12 | passed 0s | S | d: harness edited after last run |
| 261 | awaken-protocol-managed | `state::events::committed_projection::publication::projection_refresh_failure_stutters_and_success_preserves_one_pair` | session.runtime_projection_failed_refresh_stutters_warm_state<br>session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 262 | awaken-protocol-managed | `state::events::committed_projection::publication::projection_source_stamp_is_exact_for_the_complete_observed_thread_prefix` | session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 263 | awaken-protocol-managed | `state::events::committed_projection::refresh_entrypoint::logical_deletion_observation_requires_positive_exact_scoped_evidence` | session.runtime_projection_logical_deletion_requires_positive_exact_scoped_evidence | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 264 | awaken-protocol-managed | `state::events::delivery::committed_delivery_all_equalities_preserve_only_monotonic_completion` | session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion | solver-kissat-v1 | 11 | passed 0s | S | a: passed 0s |
| 265 | awaken-protocol-managed | `state::events::recovery::selection_tests::managed_reply_anchor_selects_exact_fenced_commit` | session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed<br>tool_result.pending_commit_coordinate_is_exact | solver-cadical-v1 | 10 | passed 0s | S | a: passed 0s |
| 266 | awaken-protocol-managed | `state::events::recovery::selection_tests::managed_ticket_admission_preserves_terminal_and_identity_priority` | tool_result.terminal_run_rejects_late_result | solver-cadical-v1 | 10 | passed 2s | S | a: passed 5s |
| 267 | awaken-protocol-managed | `state::session_record::projection_publish_decision_is_total_exact_and_fail_closed` | session.runtime_projection_equal_source_and_content_is_a_read_without_cache_write_or_broadcast<br>session.runtime_projection_stale_writer_never_rolls_back_visible_result<br>session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix | solver-kissat-v1 | 12 | passed 0s | S | a: passed 0s |
| 268 | awaken-protocol-managed | `types::deployment::deployment_run_failure_projection_is_total_exact_and_non_strengthening` | public_dto.deployment_failure_projection_is_total_exact_and_non_strengthening | solver-kissat-v1 | 12 | passed 0s | S | a: passed 0s |
| 269 | awaken-provider-genai | `verification::credential_probe_tri_state_is_total_and_fail_safe` | credential_probe.tri_state_is_total_and_fail_safe | - | 82 | passed 0s | S | a: passed 0s |
| 270 | awaken-provider-genai | `stop_reason::verification::unclassified_provider_stop_reason_fails_closed` | model_stop.unclassified_provider_reason_fails_closed | - | 82 | passed 0s | S | a: passed 0s |
| 271 | awaken-provider-genai | `transcript_projection::proofs::reasoning_replay_projection_preserves_reasoning_and_complete_responses` | protocol_adapter.genai_signed_reasoning_transcript_projection_is_dialect_exact_nonduplicating_and_transport_independent | - | 82 | passed 0s | S | a: passed 1s |
| 272 | awaken-provider-genai | `transcript_projection::proofs::reasoning_fold_prepends_exactly_once_and_is_transport_independent` | protocol_adapter.genai_signed_reasoning_transcript_projection_is_dialect_exact_nonduplicating_and_transport_independent | - | 82 | passed 0s | S | a: passed 0s |
| 273 | awaken-provisioning-contract | `claimed_effect::verification::claim_retention_runs_once_only_for_an_unsettled_drop` | claimed_effect.retention_runs_once_for_an_unsettled_drop | - | 83 | passed 0s | S | a: passed 0s |
| 274 | awaken-provisioning-contract | `lease::verification::credential_expiry_never_exceeds_lease_or_own_ttl` | lease.credential_expiry_is_bounded | - | 83 | passed 0s | S | a: passed 0s |
| 275 | awaken-provisioning-contract | `lease::verification::revoked_or_expired_lease_always_denies_egress` | lease.egress_fails_closed | - | 83 | passed 0s | S | a: passed 0s |
| 276 | awaken-provisioning-contract | `lease::verification::lease_loss_obeys_fixed_fail_closed_fence_priority` | lease.loss_only_fences | - | 83 | passed 0s | S | a: passed 0s |
| 277 | awaken-provisioning-contract | `lease::verification::valid_lease_timing_always_has_three_renewal_opportunities` | lease.timing_has_three_renewal_windows | - | 83 | passed 1s | S | a: passed 1s |
| 278 | awaken-provisioning-contract | `lease::verification::valid_recovery_grace_covers_lease_and_reconciliation` | lease.recovery_grace_covers_reconciliation | - | 83 | passed 0s | S | a: passed 0s |
| 279 | awaken-provisioning-contract | `sandbox::runtime::verification::sandbox_admission_never_weakens_the_isolation_floor` | sandbox.admission_never_weakens_isolation | - | 83 | passed 0s | S | a: passed 0s |
| 280 | awaken-provisioning-contract | `sandbox::runtime::verification::sandbox_admission_requires_every_requested_capability` | sandbox.admission_requires_requested_capabilities | - | 84 | passed 0s | S | a: passed 0s |
| 281 | awaken-provisioning-contract | `sandbox::runtime::verification::fail_closed_sandbox_policy_never_authorizes_a_downgrade` | sandbox.fail_closed_policy_never_downgrades | - | 83 | passed 0s | S | a: passed 0s |
| 282 | awaken-provisioning-contract | `sandbox::runtime::verification::workspace_path_projection_requires_exact_fidelity_admission` | sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths | - | 84 | passed 0s | S | a: passed 1s |
| 283 | awaken-resource-contract | `execution::artifact_publication_receipt_requires_every_identity_axis` | resource_catalog.artifact_publication_receipt_requires_exact_publication_identity | - | 85 | passed 0s | S | a: passed 0s |
| 284 | awaken-resource-contract | `lifecycle::stale_resource_purge_claim_cannot_commit_receipt` | resource_reclamation.stale_claim_cannot_commit_receipt | - | 85 | passed 0s | S | a: passed 0s |
| 285 | awaken-resource-contract | `registry::registry_proofs::resource_config_publication_is_exact_and_never_wraps` | resource_catalog.config_publication_is_exact_and_never_wraps | - | 85 | passed 0s | S | a: passed 0s G8 |
| 286 | awaken-resource-contract | `registry::registry_proofs::exhausted_resource_config_versions_fail_closed` | resource_catalog.config_publication_is_exact_and_never_wraps | - | 85 | passed 0s | S | a: passed 0s G8 |
| 287 | awaken-resource-contract | `registry::registry_proofs::portable_aggregate_revision_is_strictly_monotonic_and_never_wraps` | resource_catalog.aggregate_revision_is_strictly_monotonic_and_never_wraps | - | 85 | passed 0s | S | a: passed 0s |
| 288 | awaken-resource-contract | `registry::registry_proofs::resource_lifecycle_timestamps_project_the_exact_transition` | resource_catalog.lifecycle_timestamp_projection_is_total_and_exact | - | 85 | passed 0s | S | a: passed 0s |
| 289 | awaken-resource-contract | `registry::registry_proofs::live_resource_binding_requires_exact_scope_active_state_and_existing_pin` | session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin | - | 85 | passed 0s | S | a: passed 0s |
| 290 | awaken-resource-reclaimer | `physical_purge_requires_zero_references_leases_and_fence` | resource_reclamation.physical_purge_requires_zero_references_leases_and_fence<br>resource_reclamation.reference_and_lease_races_fail_closed | - | 86 | passed 0s | S | a: passed 0s |
| 291 | awaken-resource-store | `reclamation_fence_admission_requires_absent_owner_and_zero_references` | resource_reclamation.physical_purge_requires_zero_references_leases_and_fence<br>resource_reclamation.reference_and_lease_races_fail_closed | - | 87 | passed 0s | S | a: passed 0s |
| 292 | awaken-run-executor-a2a | `message_identity::verification::a2a_message_identity_rejects_only_resume_without_committed_correlation` | remote_attempt.replay_uses_stable_message_identity | - | 88 | passed 0s | S | a: passed 0s |
| 293 | awaken-run-executor-a2a | `task_state::kani_proofs::remote_operation_sequence_admits_only_prepared_before_send_and_accept_commit` | remote_attempt.prepared_operation_precedes_send_and_blocks_unknown_replay | - | 88 | passed 0s | S | a: passed 0s |
| 294 | awaken-run-executor-a2a | `task_state::kani_proofs::remote_operation_route_is_total_and_fail_closed` | remote_attempt.committed_task_is_endpoint_pinned<br>remote_attempt.endpoint_mismatch_fails_closed<br>remote_attempt.prepared_operation_precedes_send_and_blocks_unknown_replay<br>remote_attempt.recovery_reattaches_without_resending | - | 88 | passed 0s | S | a: passed 0s |
| 295 | awaken-run-executor-a2a | `task_state::kani_proofs::terminal_remote_operation_command_is_exact_run_scoped_removal` | remote_attempt.terminal_commit_retires_task_reference | - | 88 | passed 0s | S | a: passed 1s |
| 296 | awaken-run-executor-a2a | `task_state::kani_proofs::task_reference_admission_requires_complete_fields_and_required_identity_matches` | remote_attempt.cancel_addresses_pinned_task<br>remote_attempt.resume_preserves_context_and_task_reference | - | 88 | passed 0s | S | a: passed 0s |
| 297 | awaken-run-executor-acp | `acp_cli::mcp_client_credential_admission_has_no_gateway_or_adapter_fallback` | credential.mcp_client_admission_requires_exact_delivery_adapter_and_http_transport<br>credential.mcp_delivery_mode_is_frozen_and_never_falls_back | - | 89 | passed 0s | S | a: passed 0s |
| 298 | awaken-run-executor-acp | `host::host_observation_projection_is_exactly_backend_scoped` | worker_credential.host_observation_projection_is_backend_scoped | - | 89 | passed 0s | S | a: passed 0s |
| 299 | awaken-run-executor-acp | `host::local_binding_batch_publication_is_complete_or_stutters` | worker_credential.local_binding_batch_is_atomic | - | 89 | passed 0s | S | a: passed 0s |
| 300 | awaken-run-executor-acp | `host::projected_bindings::projected_binding_use_rechecks_the_exact_projection_after_probe` | worker_credential.projected_binding_use_rechecks_after_probe | - | 89 | passed 0s | S | a: passed 0s |
| 301 | awaken-runtime | `circuit_breaker::kernel::proofs::circuit_admission_is_capacity_bounded_nonwrapping_and_rejection_stutters` | circuit.half_open_probe_capacity<br>circuit.probe_generation_fencing | - | 90 | passed 0s | S | a: passed 0s |
| 302 | awaken-runtime | `circuit_breaker::kernel::proofs::circuit_completion_requires_exact_phase_and_generation_for_every_outcome` | circuit.abandoned_probe_reopens<br>circuit.probe_generation_fencing | - | 90 | passed 0s | S | a: passed 0s |
| 303 | awaken-runtime | `engine::finalize::child_cancellation_effect_requires_committed_intent` | delegation.cancel_intent | - | 90 | passed 0s | S | a: passed 0s |
| 304 | awaken-runtime | `engine::inference::verification::checkpoint_clear_requires_unvetoed_terminal_completion` | checkpoint.terminal_completion_clears_resume | - | 90 | passed 0s | S | a: passed 0s |
| 305 | awaken-runtime | `engine::progress::verification::durable_watermark_successor_is_exact_or_rejected_without_rollback` | checkpoint.durable_watermark_never_rolls_back | - | 90 | passed 0s | S | a: passed 0s |
| 306 | awaken-runtime | `engine::progress::verification::a_resume_receipt_requires_consumption_of_the_exact_ticket` | tool_result.resume_receipt_is_atomic_with_ticket_consumption | - | 90 | passed 0s | S | a: passed 0s |
| 307 | awaken-runtime-contract | `boundary::verification::boundary_priority_is_pause_then_input_then_idle` | live_inbox.boundary_priority_is_pause_then_input_then_idle | - | 91 | passed 0s | S | a: passed 0s |
| 308 | awaken-runtime-contract | `capture::kani_proofs::capture_meet_is_exact_commutative_and_non_widening` | capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed | - | 91 | passed 0s | S | a: passed 0s |
| 309 | awaken-runtime-contract | `capture::kani_proofs::capture_projection_admits_content_only_at_full` | capture.policy_meet_and_projection_are_exact_non_widening_and_fail_closed | - | 91 | passed 0s | S | a: passed 0s |
| 310 | awaken-runtime-contract | `delegation::delivery_verification::child_result_is_recorded_once_and_duplicate_record_stutters` | delegation.result_record_once | - | 91 | passed 0s | S | a: passed 0s |
| 311 | awaken-runtime-contract | `delegation::delivery_verification::child_result_is_consumed_only_from_ready` | delegation.result_consume_once | - | 91 | passed 0s | S | a: passed 0s |
| 312 | awaken-runtime-contract | `delegation::delivery_verification::terminal_delivery_phases_never_reopen` | delegation.late_result_closed | - | 91 | passed 0s | S | a: passed 0s |
| 313 | awaken-runtime-contract | `execution::claim_bound_external_effect_admits_only_current_ownership` | credential_effect.boundary_preserves_exact_materialization_authority<br>worker.secret_materialization_is_claim_fenced | - | 91 | passed 0s | S | a: passed 0s |
| 314 | awaken-runtime-contract | `live_inbox::verification::live_inbox_closed_lifecycle_is_absorbing_for_every_mutation` | live_inbox.close_is_absorbing | - | 92 | passed 0s | S | a: passed 0s |
| 315 | awaken-runtime-contract | `live_inbox::verification::rejected_live_inbox_reorder_is_an_atomic_stutter` | live_inbox.reorder_is_exact_and_rejection_stutters | nightly | 92 | passed 73s | M | a: passed 164s |
| 316 | awaken-runtime-contract | `live_inbox::verification::live_inbox_identity_advances_strictly_or_exhausts` | live_inbox.identity_is_monotonic | - | 92 | passed 0s | S | a: passed 0s |
| 317 | awaken-runtime-contract | `live_inbox::verification::retired_live_inbox_identity_is_never_reissued` | live_inbox.retired_message_never_reappears | - | 92 | passed 0s | S | a: passed 0s |
| 318 | awaken-runtime-contract | `live_inbox::verification::live_inbox_replace_preserves_identity_and_order` | live_inbox.replace_preserves_identity_and_order | - | 92 | passed 3s | S | a: passed 4s |
| 319 | awaken-runtime-contract | `live_inbox::verification::live_inbox_boundary_drain_retires_exactly_the_queue` | live_inbox.boundary_drain_retires_exactly_the_returned_best_effort_fold | - | 91 | passed 1s | S | a: passed 3s |
| 320 | awaken-runtime-contract | `llm::stop_reason_verification::max_token_continuation_requires_every_exact_guard` | runtime.max_token_continuation_requires_every_guard_and_exhaustion_never_succeeds | - | 92 | passed 0s | S | a: passed 0s |
| 321 | awaken-runtime-contract | `llm::inference_budget::model_budget_requires_positive_response_before_watchdog` | llm.inference_budget_requires_positive_response_before_actual_watchdog | - | 92 | passed 0s | S | a: passed 0s |
| 322 | awaken-runtime-contract | `model_operation::no_dispatch_mints_only_reviewed_pre_call_stages` | model_operation.reentry_requires_positive_execution_evidence | - | 93 | passed 0s | S | a: passed 0s |
| 323 | awaken-runtime-contract | `model_operation::model_execution_reentry_requires_positive_evidence` | model_operation.reentry_requires_positive_execution_evidence | - | 92 | passed 0s | S | d: harness edited after last run |
| 324 | awaken-runtime-contract | `model_operation::model_operation_ordinal_never_wraps_or_reuses_a_coordinate` | model_operation.ordinal_never_wraps_or_reuses_a_coordinate | - | 93 | passed 1s | S | a: passed 1s |
| 325 | awaken-runtime-contract | `model_operation::model_operation_history_preserves_distinct_subjects` | model_operation.ordinal_never_wraps_or_reuses_a_coordinate | solver-kissat-v1 | 15 | passed 397s | L | d: harness edited after last run |
| 326 | awaken-runtime-contract | `model_routing::kani_proofs::fallback_selection_preserves_the_complete_publication_pin` | config.inference_access_resolved_once_at_publication<br>model_route.fallback_preserves_the_complete_publication_pin | - | 93 | passed 0s | S | a: passed 0s |
| 327 | awaken-runtime-contract | `permission::kani_proofs::tool_capability_intersection_never_widens_configured_authority` | permission.runtime_capability_and_verdict_projection_never_widens_authority | - | 93 | passed 0s | S | a: passed 0s |
| 328 | awaken-runtime-contract | `permission::kani_proofs::permission_verdict_projects_to_exact_non_widening_gate_outcome` | permission.runtime_capability_and_verdict_projection_never_widens_authority | - | 93 | passed 0s | S | a: passed 0s |
| 329 | awaken-runtime-contract | `permission::kani_proofs::every_gate_outcome_has_one_exact_audit_label` | permission.runtime_capability_and_verdict_projection_never_widens_authority | - | 93 | passed 0s | S | a: passed 0s |
| 330 | awaken-runtime-contract | `plugin::env::kani_proofs::plugin_activation_is_exactly_the_requested_identity` | plugin_activation.identity_dependencies_capabilities_and_uniqueness_are_exact | - | 93 | passed 0s | S | a: passed 0s G9 |
| 331 | awaken-runtime-contract | `plugin::env::kani_proofs::plugin_activation_requires_every_declared_dependency` | plugin_activation.identity_dependencies_capabilities_and_uniqueness_are_exact | - | 94 | passed 0s | S | a: passed 0s G9 |
| 332 | awaken-runtime-contract | `plugin::env::kani_proofs::plugin_activation_never_widens_the_capability_bound` | plugin_activation.identity_dependencies_capabilities_and_uniqueness_are_exact | - | 94 | passed 0s | S | a: passed 0s G9 |
| 333 | awaken-runtime-contract | `plugin::env::kani_proofs::plugin_activation_never_admits_a_duplicate_identity` | plugin_activation.identity_dependencies_capabilities_and_uniqueness_are_exact | - | 93 | passed 0s | S | a: passed 0s G9 |
| 334 | awaken-runtime-contract | `plugin::phase::context_window_verification::anchored_context_window_never_moves_past_the_covered_prefix` | context.window_anchor_preserves_complete_prefix_coverage | - | 94 | passed 0s | S | a: passed 0s |
| 335 | awaken-runtime-contract | `plugin::phase::context_window_verification::unanchored_legacy_context_never_hides_transcript_messages` | context.legacy_unanchored_window_never_drops_transcript | - | 94 | passed 0s | S | a: passed 0s |
| 336 | awaken-runtime-contract | `resilience::kani_proofs::retry_is_authorized_only_inside_the_exact_bounded_budget` | runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed | - | 94 | passed 0s | S | a: passed 0s |
| 337 | awaken-runtime-contract | `resilience::kani_proofs::non_retryable_failure_is_immediately_terminal` | runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed | - | 94 | passed 0s | S | a: passed 0s G10 |
| 338 | awaken-runtime-contract | `resilience::kani_proofs::terminal_retry_state_never_reopens` | runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed | - | 94 | passed 0s | S | a: passed 0s G10 |
| 339 | awaken-runtime-contract | `resilience::kani_proofs::unknown_retry_state_fails_closed` | runtime_resilience.retry_is_exact_bounded_terminal_and_fail_closed | - | 94 | passed 0s | S | a: passed 0s G10 |
| 340 | awaken-runtime-contract | `resolved::advisor_never_substitutes_for_primary_model_admission` | model_advisor.never_substitutes_for_primary_admission_or_duplicates_a_binding | - | 95 | passed 4s | S | a: passed 5s |
| 341 | awaken-runtime-contract | `resolved::model_binding::model_binding_admission_is_exactly_the_documented_rule_table` | model_binding.constructor_and_wire_share_one_admission | - | 95 | passed 0s | S | a: passed 0s |
| 342 | awaken-runtime-contract | `resume::tests::resume_validator_preserves_complete_binding_decision_table` | tool_result.only_matching_call_is_accepted | resume-validator-empty-object-v1 | 14 | passed 297s | M | d: loop profile repinned after last run |
| 343 | awaken-runtime-contract | `runtime_context::legacy_derived_context_remains_durable_but_never_reaches_a_new_model_request` | session.legacy_derived_context_remains_durable_but_is_hidden_from_new_model_requests | - | 95 | passed 0s | S | a: passed 0s |
| 344 | awaken-runtime-contract | `runtime_context::transcript_prefix_is_exact_request_only_context_not_durable_truth` | session.transcript_prefix_is_exact_request_only_context_not_durable_truth | - | 95 | passed 0s | S | a: passed 0s |
| 345 | awaken-runtime-contract | `terminal::verification::committed_terminal_projection_admits_only_exact_ended_identity` | remote_worker.committed_terminal_recovery_skips_execution_and_settles_dispatch<br>remote_worker.pending_signal_is_consumed_by_commit<br>tool_result.terminal_run_rejects_late_result | - | 95 | passed 0s | S | a: passed 0s |
| 346 | awaken-sandbox-container | `docker::docker_absence_requires_an_explicit_not_found_response` | sandbox.docker_absence_requires_explicit_backend_not_found | - | 97 | passed 1s | S | a: passed 3s |
| 347 | awaken-sandbox-container | `runtime::allowlist_claim_never_exceeds_its_evidence` | sandbox.allowlist_claim_requires_runtime_and_issuer_evidence | - | 96 | passed 0s | S | a: passed 0s |
| 348 | awaken-sandbox-container | `runtime::retained_original_never_authorizes_exact_physical_removal` | resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence | solver-kissat-v1 | 16 | never | ~S | d: never recorded |
| 349 | awaken-sandbox-container | `runtime::realization::rebuild_activation_requires_exact_continuation_and_effect` | resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence | solver-kissat-v1 | 16 | passed 0s | S | a: passed 0s |
| 350 | awaken-sandbox-container | `writable::continuation_writable_roots_share_one_claim_without_aliasing` | sandbox_continuation.distinct_mutable_roots_share_one_retained_claim_without_aliasing | - | 96 | passed 0s | S | a: passed 0s |
| 351 | awaken-sandbox-control-transport | `publication::verification::publication_slot_directly_preserves_single_owner_stale_fence_and_absorbing_close` | sandbox_control.single_publication_is_stale_fenced_non_wrapping_and_close_absorbing | - | 98 | passed 2s | S | a: passed 3s |
| 352 | awaken-sandbox-kubernetes | `continuation::continuation_claim_selection_is_total_exact_and_non_widening` | sandbox_continuation.ephemeral_never_allocates_or_references_a_claim | solver-kissat-v1 | 17 | passed 0s | S | a: passed 0s |
| 353 | awaken-sandbox-kubernetes | `continuation::absent_pod_claim_admission_is_total_and_never_starts_deletion` | resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence | solver-kissat-v1 | 17 | passed 0s | S | a: passed 0s |
| 354 | awaken-sandbox-kubernetes | `continuation::continuation_claim_deletion_requires_exact_uid_and_resource_version` | resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence | solver-kissat-v1 | 17 | passed 0s | S | a: passed 0s |
| 355 | awaken-sandbox-kubernetes | `continuation::original_guard_gate_admission_preserves_exact_uid_and_retention` | resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence | solver-kissat-v1 | 17 | passed 0s | S | d: harness edited after last run |
| 356 | awaken-sandbox-kubernetes | `continuation_observation::absent_pod_observation_never_confuses_recovery_with_completed_cleanup` | resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence | solver-kissat-v1 | 17 | passed 1s | S | d: harness edited after last run |
| 357 | awaken-sandbox-kubernetes | `continuation_observation::disabled_continuation_has_no_claim_api_obligation_without_durable_evidence` | sandbox_continuation.ephemeral_never_allocates_or_references_a_claim | solver-kissat-v1 | 17 | passed 0s | S | d: harness edited after last run |
| 358 | awaken-sandbox-local | `memory_mount_owner::live_provider_prunes_only_observed_empty_unreferenced_memory_owners` | memory_participant.live_provider_retains_pending_and_failed_operations | - | 99 | passed 0s | S | a: passed 0s |
| 359 | awaken-sandbox-memoryd | `fuse::listener_cleanup_requests_stop_then_joins_the_owned_handle_exactly_once` | memory_invalidation.listener_cleanup_is_owned_and_joined | - | 100 | passed 0s | S | a: passed 0s |
| 360 | awaken-service-auth-contract | `verification::service_token_retry_is_enabled_only_for_an_exact_changed_token` | service_auth.token_rotation_fails_closed_and_retries_only_a_changed_token | - | 101 | passed 0s | S | a: passed 0s |
| 361 | awaken-service-lifecycle | `verification::shutdown_registry_fences_registration_and_transfers_exactly_once` | service_lifecycle.shutdown_fences_task_registration | - | 102 | passed 5s | S | a: passed 5s |
| 362 | awaken-service-lifecycle | `verification::concurrent_shutdown_admission_waits_for_the_active_drain` | service_lifecycle.concurrent_shutdown_waits_for_active_drain | - | 102 | passed 0s | S | a: passed 0s |
| 363 | awaken-service-lifecycle | `verification::startup_wiring_is_exact_for_every_service_role` | service_lifecycle.startup_wiring_is_exact_for_every_service_role | - | 102 | passed 0s | S | a: passed 0s |
| 364 | awaken-service-lifecycle | `verification::startup_wiring_requires_every_role_owned_component` | service_lifecycle.startup_wiring_requires_every_role_owned_component | - | 102 | passed 0s | S | a: passed 0s |
| 365 | awaken-service-lifecycle | `verification::startup_wiring_fails_closed_for_unknown_missing_or_extra_authority` | service_lifecycle.startup_wiring_fails_closed_for_unknown_missing_or_extra_authority | - | 102 | passed 0s | S | a: passed 0s |
| 366 | awaken-service-lifecycle | `verification::process_role_projects_exact_startup_authority` | assembly_role.process_role_projects_exact_startup_authority | - | 102 | passed 0s | S | a: passed 0s |
| 367 | awaken-service-lifecycle | `verification::worker_never_acquires_service_startup_or_local_mount_authority` | assembly_role.worker_never_acquires_service_startup_or_migration_authority | - | 103 | passed 0s | S | a: passed 0s |
| 368 | awaken-service-lifecycle | `verification::local_managed_mount_is_exactly_the_local_worker_component` | assembly_role.local_managed_mount_requires_exact_local_worker_component | - | 102 | passed 0s | S | a: passed 0s |
| 369 | awaken-service-lifecycle | `release_admission::verified_release_and_tcb_admission_are_total_exact_and_fail_closed` | assembly_role.release_consumption_requires_exact_artifact_profile_and_completed_proofs<br>assembly_role.verified_admission_requires_exact_fresh_tcb_fingerprint | - | 102 | passed 0s | S | a: passed 0s |
| 370 | awaken-session-application | `activity::session_activity_open_and_settlement_plans_are_total_exact_and_fail_closed` | session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement<br>session_event_batch.run_reservation_precedes_distinct_activity_receipt<br>session_execution.activity_receipts_are_unique_durable_and_exactly_settled<br>session_execution.initial_event_path_reaches_idle_only_after_full_settlement | - | 104 | passed 0s | S | a: passed 0s |
| 371 | awaken-session-application | `agent_configuration::missing_agent_publication_is_available_only_to_noninteractive_projection` | session.missing_publication_is_available_only_to_noninteractive_frozen_projection | solver-kissat-v1 | 18 | passed 0s | S | d: harness edited after last run |
| 372 | awaken-session-application | `environment_owner_kani_proof::environment_owner_topology_is_exact_and_frozen` | runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker<br>session_environment.owner_topology_is_exact_and_frozen<br>session_run.execution_requires_dispatch_work_and_realization_owner | - | 104 | passed 0s | S | d: harness edited after last run |
| 373 | awaken-session-application | `creation::create_effect_gate_verification::only_an_applied_awaited_create_may_enter_post_create_effects` | session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store<br>session_creation.atomic_complete_root_precedes_every_projection<br>session_creation.exact_replay_conflict_and_tombstone_are_closed<br>session_creation.repository_receipt_is_the_only_post_create_effect_gate | - | 104 | passed 0s | S | a: passed 0s |
| 374 | awaken-session-application | `creation::create_effect_gate_verification::nonterminal_control_failures_only_replay_the_durable_create_root` | session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store<br>session_creation.repository_receipt_is_the_only_post_create_effect_gate | - | 104 | passed 0s | S | a: passed 0s |
| 375 | awaken-session-application | `creation::create_effect_gate_verification::create_replay_conflict_and_terminal_occupation_are_exact_and_closed` | session_creation.exact_replay_conflict_and_tombstone_are_closed | - | 104 | passed 0s | S | d: harness edited after last run |
| 376 | awaken-session-application | `lifecycle_outbox_relay::pending_lifecycle_fact_completes_iff_delivery_succeeds_with_the_exact_identity` | webhook.delivery_requires_an_enqueued_fact<br>webhook.failed_delivery_remains_pending<br>webhook.outbox_fact_identity_is_stable<br>webhook.periodic_retry_never_retires_a_failed_fact | - | 104 | passed 0s | S | a: passed 0s |
| 377 | awaken-session-application | `mutation::observation_proofs::session_root_observation_is_total_monotonic_and_idempotent` | session_time.root_observations_are_revision_bound_and_replay_stable | - | 104 | passed 0s | S | a: passed 0s |
| 378 | awaken-session-application | `mutation::observation_proofs::session_root_receipt_selects_candidate_only_for_applied_revision` | session_time.root_observations_are_revision_bound_and_replay_stable | - | 104 | passed 0s | S | d: harness edited after last run |
| 379 | awaken-session-application | `realization::publication::realization_publication_selects_only_unacknowledged_active_generation` | session_realization.publication_excludes_staged_retired_generation | - | 105 | passed 0s | S | a: passed 0s |
| 380 | awaken-session-application | `recovery::backoff::proofs::session_recovery_backoff_is_bounded_monotonic_and_saturating` | session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline | - | 105 | passed 0s | S | a: passed 0s |
| 381 | awaken-session-application | `recovery::backoff::proofs::dependency_wake_never_postpones_the_single_existing_deadline` | session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline | - | 105 | passed 0s | S | a: passed 0s |
| 382 | awaken-session-application | `recovery::backoff::proofs::external_wake_coalescing_is_bounded_and_never_postpones_recovery` | session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline | - | 105 | passed 0s | S | a: passed 0s |
| 383 | awaken-session-application | `recovery::backoff::proofs::only_an_empty_hint_driven_scan_arms_the_coalescing_window` | session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline | - | 105 | passed 0s | S | a: passed 0s |
| 384 | awaken-session-application | `resource_reconciliation::repository_rotation_requires_original_live_resource_generation` | session_resources.repository_rotation_requires_original_live_generation | - | 105 | passed 0s | S | d: harness edited after last run |
| 385 | awaken-session-application | `resource_reconciliation::ordinary_reconciliation_gate::terminal_session_never_enters_ordinary_resource_reconciliation` | session_resources.terminal_session_never_reactivates | - | 105 | passed 0s | S | a: passed 0s |
| 386 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_resolver_preserves_empty` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-64-v1 | 23 | passed 2s | S | a: passed 3s |
| 387 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_resolver_preserves_legacy_error_and_frame` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-64-v1/nightly | 23 | passed 69s | M | a: passed 138s |
| 388 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_closed_none_07` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 22 | passed 29s | S | a: passed 58s G11 |
| 389 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_none_00` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 17s | S | a: passed 51s G11 |
| 390 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_none_01` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1 | 24 | passed 43s | M | a: passed 124s G11 |
| 391 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_none_02` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 25s | M | a: passed 67s G11 |
| 392 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_accepted_none_03` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1/nightly | 22 | passed 89s | M | a: passed 180s G11 |
| 393 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_none_04` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 32s | S | a: passed 53s G11 |
| 394 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_none_05` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1 | 24 | passed 41s | M | a: passed 122s G11 |
| 395 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_none_06` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 23s | S | a: passed 42s G11 |
| 396 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_empty_08` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 22s | S | a: passed 52s G11 |
| 397 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_empty_09` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1 | 24 | passed 40s | M | a: passed 138s G11 |
| 398 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_empty_10` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 24s | M | a: passed 75s G11 |
| 399 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_accepted_empty_11` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 22 | passed 37s | M | a: passed 98s G11 |
| 400 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_empty_12` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 24s | S | a: passed 59s G11 |
| 401 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_empty_13` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1/nightly | 24 | passed 51s | M | a: passed 145s G11 |
| 402 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_empty_14` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 20 | passed 21s | S | a: passed 51s G11 |
| 403 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_closed_empty_15` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 22 | passed 21s | S | a: passed 56s G11 |
| 404 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_one_16` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 21 | passed 19s | M | a: passed 67s G11 |
| 405 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_one_17` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1/nightly | 24 | passed 44s | M | a: passed 110s G11 |
| 406 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_one_18` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 21 | passed 28s | M | a: passed 61s G11 |
| 407 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_accepted_one_19` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 22 | passed 33s | M | a: passed 91s G11 |
| 408 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_one_20` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 21 | passed 21s | S | a: passed 44s G11 |
| 409 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_one_21` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1/nightly | 24 | passed 45s | M | a: passed 124s G11 |
| 410 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_one_22` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 21 | passed 20s | S | a: passed 35s G11 |
| 411 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_closed_one_23` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1 | 22 | passed 21s | S | a: passed 32s G11 |
| 412 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_two_24` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1/nightly | 21 | passed 93s | M | a: passed 254s G11 |
| 413 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_accepted_two_27` | session_event_batch.final_non_admission_excludes_late_acceptance | retained-accepted-two-replay-v1/nightly | 19 | passed 80s | M | a: passed 156s G11 |
| 414 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_accepted_two_initial_27` | session_event_batch.final_non_admission_excludes_late_acceptance | - | 106 | passed 31s | M | a: passed 76s |
| 415 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_two_28` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1/nightly | 21 | passed 131s | M | a: passed 212s G11 |
| 416 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_two_30` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1/nightly | 21 | passed 86s | M | a: passed 214s G11 |
| 417 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_closed_two_31` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1/nightly | 22 | passed 86s | M | a: passed 154s G11 |
| 418 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_absent_two_26` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-2-v1/nightly | 21 | passed 119s | M | a: passed 233s G11 |
| 419 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_two_25` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1/nightly | 24 | passed 117s | L | a: passed 326s G11 |
| 420 | awaken-session-contract | `event::admission_outcome::verification::deleted_admission_one_retained_conflict_two_29` | session_event_batch.final_non_admission_excludes_late_acceptance | unwind-memcmp-65-v1/nightly | 24 | passed 120s | L | a: passed 380s G11 |
| 421 | awaken-session-application-contract | `executable_projection::executable_projection_admission_requires_both_ordered_refreshes` | executable_projection.failed_refresh_never_enters_handler<br>executable_projection.runtime_admission_requires_both_refreshes | - | 106 | passed 0s | S | a: passed 0s |
| 422 | awaken-time | `utc::public_millisecond_projection_is_exact_bounded_and_nonwrapping` | session_time.utc_projection_domain_is_total_exact_and_idempotent | - | 106 | passed 0s | S | a: passed 0s |
| 423 | awaken-session-application-contract | `worker_lease::session_work_receipt_kind_and_minting_are_exact` | session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit | - | 106 | passed 2s | S | a: passed 2s |
| 424 | awaken-session-application-contract | `worker_lease_types::stopped_session_work_is_revived_only_by_a_claimed_nonterminal_run` | session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work | - | 106 | passed 0s | S | d: harness edited after last run |
| 425 | awaken-session-application-contract | `worker_lease_types::verification::session_work_effect_requires_not_required_scope_or_exact_owner` | session_run.execution_requires_dispatch_work_and_realization_owner | - | 106 | passed 0s | S | a: passed 0s |
| 426 | awaken-session-configuration-contract | `agent_config::bounded_runtime_capability_iterator_uses_exact_member_projection` | runtime_capability.bounded_iterator_uses_the_exact_eight_member_projection | - | 107 | passed 0s | S | d: harness edited after last run |
| 427 | awaken-session-configuration-contract | `agent_config::runtime_capability_member_projection_is_total_exact_and_bounded` | runtime_capability.bounded_iterator_uses_the_exact_eight_member_projection | - | 107 | passed 0s | S | d: harness edited after last run |
| 428 | awaken-session-configuration-contract | `agent_config::managed_toolset_default_resolution_is_source_exact_and_configurable` | tool_policy.managed_agent_and_mcp_defaults_are_source_exact_and_configurable | - | 107 | passed 0s | S | d: harness edited after last run |
| 429 | awaken-session-configuration-contract | `baseline::kani_proofs::resolved_environment_snapshot_accepts_only_exact_positive_identity_and_revision` | session_creation.selected_environment_is_frozen_in_baseline | - | 107 | passed 0s | S | a: passed 0s |
| 430 | awaken-session-configuration-contract | `model_resolution::verification::session_model_override_reuses_only_the_same_identity_and_resolves_every_mismatch` | model_route.session_override_reuses_only_exact_identity_and_resolves_every_mismatch | - | 107 | passed 0s | S | a: passed 0s |
| 431 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_normalization_none_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 110 | passed 1s | S | a: passed 2s G12 |
| 432 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_normalization_unrestricted_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 111 | passed 1s | S | a: passed 1s G12 |
| 433 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_normalization_empty_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 110 | passed 1s | S | a: passed 2s G12 |
| 434 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_normalization_one_ascii_host_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 111 | passed 8s | S | a: passed 13s G12 |
| 435 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_normalization_two_ascii_hosts_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 111 | passed 30s | S | a: passed 52s G12 |
| 436 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_two_by_two_ascii_hosts_partition` | session_creation.network_intersection_is_canonical_and_non_widening | nightly | 110 | passed 125s | M | a: passed 258s G13 |
| 437 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_none_by_none_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 1s | S | a: passed 3s G13 |
| 438 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_none_by_unrestricted_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 1s | S | a: passed 2s G13 |
| 439 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_none_by_empty_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 1s | S | a: passed 2s G13 |
| 440 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_none_by_one_ascii_host_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 7s | S | a: passed 17s G13 |
| 441 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_none_by_two_ascii_hosts_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 24s | S | a: passed 42s G13 |
| 442 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_unrestricted_by_none_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 110 | passed 1s | S | a: passed 2s G13 |
| 443 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_unrestricted_by_unrestricted_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 110 | passed 1s | S | a: passed 2s G13 |
| 444 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_unrestricted_by_empty_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 110 | passed 1s | S | a: passed 2s G13 |
| 445 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_unrestricted_by_one_ascii_host_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 110 | passed 9s | S | a: passed 31s G13 |
| 446 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_unrestricted_by_two_ascii_hosts_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 110 | passed 28s | M | a: passed 93s G13 |
| 447 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_empty_by_none_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 107 | passed 1s | S | a: passed 3s G13 |
| 448 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_empty_by_unrestricted_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 1s | S | a: passed 3s G13 |
| 449 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_empty_by_empty_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 107 | passed 1s | S | a: passed 6s G13 |
| 450 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_empty_by_one_ascii_host_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 107 | passed 7s | S | a: passed 20s G13 |
| 451 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_empty_by_two_ascii_hosts_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 24s | S | a: passed 50s G13 |
| 452 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_one_ascii_host_by_none_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 109 | passed 7s | S | a: passed 15s G13 |
| 453 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_one_ascii_host_by_unrestricted_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 109 | passed 9s | S | a: passed 15s G13 |
| 454 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_one_ascii_host_by_empty_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 108 | passed 8s | S | a: passed 10s G13 |
| 455 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_one_ascii_host_by_one_ascii_host_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 109 | passed 36s | M | a: passed 72s G13 |
| 456 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_one_ascii_host_by_two_ascii_hosts_partition` | session_creation.network_intersection_is_canonical_and_non_widening | nightly | 109 | passed 79s | M | a: passed 155s G13 |
| 457 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_two_ascii_hosts_by_none_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 109 | passed 20s | S | a: passed 60s G13 |
| 458 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_two_ascii_hosts_by_unrestricted_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 109 | passed 32s | M | a: passed 95s G13 |
| 459 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_two_ascii_hosts_by_empty_partition` | session_creation.network_intersection_is_canonical_and_non_widening | - | 109 | passed 22s | M | a: passed 71s G13 |
| 460 | awaken-session-configuration-contract | `network_policy::collection_tests::session_network_intersection_two_ascii_hosts_by_one_ascii_host_partition` | session_creation.network_intersection_is_canonical_and_non_widening | nightly | 109 | passed 79s | M | a: passed 234s G13 |
| 461 | awaken-session-configuration-contract | `resource::realization_lease::realization_renewal_requires_every_identity_axis` | session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence | - | 111 | passed 0s | S | a: passed 0s |
| 462 | awaken-session-configuration-contract | `resource::realization_lease::realization_renewal_requires_nonregressed_expiry` | session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence | - | 111 | passed 0s | S | d: harness edited after last run G14 |
| 463 | awaken-session-configuration-contract | `resource::realization_lease::realization_renewal_expires_at_exact_boundary` | session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence | - | 111 | passed 26s | S | d: harness edited after last run G14 |
| 464 | awaken-session-configuration-contract | `skill_execution::skill_execution_pin_requires_exact_workspace_revision_and_hash` | session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin<br>skill.execution_pin_binds_exact_workspace_revision_and_hash<br>skill.invalid_workspace_or_hash_fails_closed<br>skill.loaded_version_equals_session_pin | - | 111 | passed 0s | S | a: passed 0s |
| 465 | awaken-session-configuration-contract | `skill_execution::every_skill_execution_pin_axis_is_binding` | skill.execution_pin_binds_exact_workspace_revision_and_hash | - | 111 | passed 0s | S | a: passed 0s |
| 466 | awaken-session-contract | `budget::accepted_managed_budget_cost_never_wraps` | session_budget.accepted_cost_arithmetic_never_wraps | - | 112 | passed 1s | S | a: passed 1s |
| 467 | awaken-session-contract | `event::admission::session_event_batch_admission_resolution_is_total_closed_and_append_once` | session_event_batch.exact_retry_replays_and_changed_retry_conflicts<br>session_event_batch.final_non_admission_excludes_late_acceptance<br>session_event_batch.idempotency_key_appends_at_most_once | - | 112 | passed 0s | S | a: passed 0s |
| 468 | awaken-session-contract | `event::dispatch_settlement::user_event_processing_requires_exact_applied_dispatch_and_activity_settlement` | session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement<br>session_execution.initial_event_path_reaches_idle_only_after_full_settlement | - | 112 | passed 0s | S | a: passed 0s |
| 469 | awaken-session-contract | `event::dispatch_settlement::retained_input_idling_is_exactly_settled_or_only_blocked_users` | session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement<br>session_execution.initial_event_path_reaches_idle_only_after_full_settlement | - | 112 | passed 1s | S | d: harness edited after last run |
| 470 | awaken-session-contract | `event::observation_proofs::processed_event_time_is_first_observation_or_atomic_rejection` | session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement<br>session_time.processed_event_retains_first_time_and_rejects_atomically | - | 112 | passed 0s | S | a: passed 1s |
| 471 | awaken-session-contract | `event::terminal_resolution_proofs::terminal_batch_resolution_empty` | session_time.processed_event_retains_first_time_and_rejects_atomically | - | 112 | passed 3s | S | d: harness edited after last run |
| 472 | awaken-session-contract | `state::environment::transition::proofs::terminal_session_never_reenters_a_live_environment` | session_environment.terminal_session_never_reenters_a_live_environment | - | 113 | passed 1s | S | a: passed 2s |
| 473 | awaken-session-contract | `state::environment::transition::proofs::environment_steps_follow_only_declared_edges` | session_environment.resident_is_entered_only_by_materialization_or_restore | - | 112 | passed 1s | S | a: passed 2s |
| 474 | awaken-session-contract | `state::environment::transition::proofs::environment_effects_require_a_current_realization_lease` | session_environment.effect_instructions_require_a_current_realization_lease | - | 112 | passed 1s | S | a: passed 2s |
| 475 | awaken-session-contract | `state::environment::verification::source_preparation_and_disposal_require_distinct_durable_phases` | session_environment.source_disposal_requires_a_verified_checkpoint | - | 113 | passed 0s | S | a: passed 0s |
| 476 | awaken-session-contract | `state::environment::verification::quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects` | session_environment.quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects | - | 113 | passed 0s | S | a: passed 0s |
| 477 | awaken-session-contract | `state::environment::verification::checkpoint_receipt_requires_every_immutable_generation_axis` | session_environment.checkpoint_receipt_requires_every_immutable_generation_axis | - | 113 | passed 0s | S | a: passed 0s |
| 478 | awaken-session-contract | `state::environment::verification::source_disposal_receipt_requires_exact_binding_and_termination` | session_environment.source_disposal_requires_a_verified_checkpoint | - | 113 | passed 0s | S | a: passed 0s |
| 479 | awaken-session-contract | `state::environment::verification::restore_receipt_requires_exact_checkpoint_and_nonempty_binding` | session_environment.restore_receipt_requires_exact_checkpoint_and_nonempty_binding | - | 113 | passed 0s | S | a: passed 0s |
| 480 | awaken-session-contract | `state::resource_activation::resource_materialization_selects_only_active_or_pending_root_generation` | session_resources.external_io_requires_durable_prepared_state | - | 113 | passed 0s | S | a: passed 0s |
| 481 | awaken-session-contract | `state::resource_activation::terminal_resource_projection_retains_the_exact_desired_generation` | session_resources.terminal_assignment_retains_desired_generation | - | 114 | passed 0s | S | a: passed 0s |
| 482 | awaken-session-contract | `state::resource_activation::failed_resource_replacement_never_promotes_the_pending_manifest` | session_resources.failed_replacement_preserves_active_manifest | - | 113 | passed 0s | S | a: passed 0s |
| 483 | awaken-session-contract | `state::resource_activation::resource_retry_reuses_exact_pending_revision` | session_resources.restart_retries_same_pending_revision | - | 114 | passed 0s | S | a: passed 0s |
| 484 | awaken-session-contract | `state::session_repo::verification::terminal_execution_never_reopens` | session_execution.terminal_state_never_reopens | - | 115 | passed 0s | S | a: passed 0s |
| 485 | awaken-session-contract | `state::session_repo::verification::session_tombstone_requires_hidden_disposition_terminal_execution_and_verified_cleanup` | session.delete_tombstone_requires_hidden_terminal_verified_cleanup | - | 115 | passed 0s | S | a: passed 0s |
| 486 | awaken-session-contract | `state::session_repo::verification::session_delete_request_plan_is_exact_hidden_terminal_and_idempotent` | session.delete_intent_is_hidden_terminal_and_idempotent<br>session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity | - | 115 | passed 0s | S | d: harness edited after last run |
| 487 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_next_epoch_inserts_exact_checked_successor` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | activity-admission-height-v1 | 25 | passed 26s | S | d: loop profile repinned after last run |
| 488 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_exact_epoch_inserts_only_fresh_positive_member` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | activity-admission-height-v1 | 25 | passed 30s | S | d: loop profile repinned after last run |
| 489 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_terminal_transition_clears_one_real_member` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | activity-terminal-height-v1 | 26 | passed 62s | M | d: loop profile repinned after last run |
| 490 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_two_slot_terminal_transition_clears_members` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | - | 114 | passed 1s | S | d: harness edited after last run |
| 491 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_two_slot_next_next_admissions_preserve_exact_members` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | - | 114 | passed 10s | S | a: passed 21s G15 |
| 492 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_two_slot_next_exact_admissions_preserve_exact_members` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | - | 114 | passed 10s | S | a: passed 18s G15 |
| 493 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_two_slot_exact_next_admissions_preserve_exact_members` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | - | 114 | passed 10s | S | a: passed 31s G15 |
| 494 | awaken-session-contract | `state::session_repo::activity_state::proofs::activity_two_slot_exact_exact_admissions_preserve_exact_members` | session_execution.activity_receipts_are_unique_durable_and_exactly_settled | - | 114 | passed 11s | S | a: passed 21s G15 |
| 495 | awaken-session-contract | `state::session_repo::execution_state::initial_realization_recovery_is_exact_for_every_execution_state` | session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store | - | 114 | passed 0s | S | a: passed 0s |
| 496 | awaken-session-contract | `state::session_repo::mutation::root_observation_proofs::session_root_replacement_preserves_creation_and_monotonic_update_observations` | session_time.root_observations_are_revision_bound_and_replay_stable | - | 115 | passed 0s | S | a: passed 0s |
| 497 | awaken-session-contract | `state::session_repo::persisted_session::terminal_environment_receipt_is_admitted_only_during_the_fenced_join` | session_environment.terminal_fence_admits_only_inflight_binding_receipts | - | 115 | passed 0s | S | a: passed 0s |
| 498 | awaken-session-contract | `state::session_repo::persisted_session::initial_event_plan_is_installed_only_on_the_complete_fresh_root` | session_event_batch.initial_plan_and_wake_activity_are_atomic_with_root<br>session_execution.initial_event_path_reaches_idle_only_after_full_settlement | - | 115 | passed 0s | S | a: passed 0s |
| 499 | awaken-session-contract | `state::session_repo::placement::verification::work_projection_is_derived_only_from_durable_session_truth` | session_creation.work_projection_is_disposable_repairable_and_non_authoritative<br>session_run.execution_requires_dispatch_work_and_realization_owner | - | 115 | passed 0s | S | a: passed 0s |
| 500 | awaken-session-contract | `state::session_repo::runtime_intervals::runtime_intervals_open_once_and_never_close_before_start` | session_runtime_interval.one_nonoverlapping_interval_closes_with_exact_fact | - | 115 | passed 0s | S | a: passed 0s |
| 501 | awaken-session-contract | `state::terminal_cleanup::verification::session_cleanup_completion_requires_every_identity_axis` | session.cleanup_completion_requires_exact_identity_and_canonical_artifact_evidence | - | 116 | passed 0s | S | a: passed 0s |
| 502 | awaken-session-contract | `state::terminal_cleanup::verification::session_cleanup_phase_advances_only_not_requested_fenced_requested_completed` | session.cleanup_operation_advances_exactly_and_completed_is_absorbing<br>session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity | - | 116 | passed 0s | S | a: passed 0s |
| 503 | awaken-session-dream-contract | `dream_status_proofs::terminal_dream_statuses_are_absorbing` | dream.lifecycle_is_closed_exact_and_terminal_absorbing | - | 117 | passed 0s | S | a: passed 0s |
| 504 | awaken-session-dream-contract | `dream_status_proofs::dream_success_and_failure_require_a_running_process` | dream.lifecycle_is_closed_exact_and_terminal_absorbing | - | 117 | passed 0s | S | a: passed 0s |
| 505 | awaken-session-dream-contract | `dream_status_proofs::dream_recovery_and_cancel_never_widen_terminal_authority` | dream.lifecycle_is_closed_exact_and_terminal_absorbing | - | 117 | passed 0s | S | a: passed 0s |
| 506 | awaken-session-dream-contract | `dream_status_proofs::dream_transition_table_is_total_exact_and_closed` | dream.lifecycle_is_closed_exact_and_terminal_absorbing | - | 117 | passed 0s | S | a: passed 0s |
| 507 | awaken-session-dream-contract | `dream_status_proofs::dream_public_terminal_requires_committed_cleanup` | dream.public_terminal_requires_exact_session_and_artifact_cleanup_receipts | - | 117 | passed 0s | S | a: passed 0s |
| 508 | awaken-session-dream-contract | `dream_status_proofs::dream_cleanup_admission_is_total_and_retries_every_pending_terminal` | dream.public_terminal_requires_exact_session_and_artifact_cleanup_receipts | - | 117 | passed 0s | S | a: passed 0s |
| 509 | awaken-session-environment | `container_files::read_only_tree_publication_requires_a_complete_restricted_safe_stage` | session_files.read_only_tree_publication_requires_complete_restricted_safe_staging | - | 118 | passed 0s | S | a: passed 0s |
| 510 | awaken-session-environment | `session_files::file_read_authority_never_changes_path_class_or_admits_unsafe_input` | sandbox.file_read_authority_is_explicit | - | 118 | passed 0s | S | a: passed 0s |
| 511 | awaken-session-environment | `session_hand::verification::hand_availability_has_only_explicit_recovery_and_terminal_transitions` | remote_tool.hand_availability_recovery_and_terminal_transitions_are_closed | - | 118 | passed 0s | S | a: passed 0s |
| 512 | awaken-session-environment | `session_hand::verification::hand_binding_can_only_become_ready_through_tracked_starting` | remote_tool.hand_launch_must_cross_a_tracked_starting_phase_before_ready | - | 118 | passed 0s | S | a: passed 0s |
| 513 | awaken-session-environment | `session_hand::verification::hand_generation_advance_never_wraps_and_exhaustion_is_terminal` | remote_tool.hand_generation_exhaustion_is_terminal_and_never_wraps | - | 118 | passed 0s | S | a: passed 0s |
| 514 | awaken-session-execution-contract | `coordination::session_agent_boundary_requires_a_visible_committed_target` | session.agent_message_completed_target_allows_late_source_receipt_without_reexecution<br>session.agent_message_missing_lifecycle_projection_relinquishes_without_settlement<br>session.agent_message_target_boundary_not_source_receipt_settles_activity | - | 119 | passed 0s | S | a: passed 0s |
| 515 | awaken-session-execution-contract | `run_admission::session_run_reservation_binds_exactly_one_nonzero_activity_receipt` | session_event_batch.run_reservation_precedes_distinct_activity_receipt<br>session_execution.activity_receipts_are_unique_durable_and_exactly_settled<br>session_run.recovery_claim_is_admission_only<br>session_run.reservation_precedes_activity | - | 119 | passed 0s | S | a: passed 0s |
| 516 | awaken-session-execution-contract | `session::kani_proofs::awaiting_constructor_cannot_create_a_terminal_or_failed_outcome` | session_outcome.awaiting_constructor_is_nonterminal_and_nonfailed | - | 119 | passed 0s | S | a: passed 0s |
| 517 | awaken-session-execution-contract | `session::kani_proofs::ended_constructor_carries_the_only_failure_authority_and_no_pending_tool` | session_outcome.ended_constructor_owns_failure_and_clears_pending_tool | - | 119 | passed 0s | S | a: passed 0s |
| 518 | awaken-session-execution-contract | `session::kani_proofs::settled_step_has_no_parallel_observation_authority` | session_execution.settled_step_has_no_parallel_observation_authority | - | 119 | passed 0s | S | a: passed 0s |
| 519 | awaken-session-executor | `host::session_ctx::acp_tool_export_requires_claim_execution_environment` | acp.tool_export_requires_claim_execution_environment | - | 120 | passed 0s | S | a: passed 0s |
| 520 | awaken-session-executor | `host::session_ctx::runtime_environment_admission_is_total_and_coordination_never_borrows` | session_run.execution_requires_dispatch_work_and_realization_owner<br>session_run.reservation_never_executes | - | 120 | passed 0s | S | a: passed 0s |
| 521 | awaken-session-executor | `host::session_ctx::coordination_envelope_never_satisfies_execution_cache_admission` | session_run.execution_requires_dispatch_work_and_realization_owner<br>session_run.reservation_never_executes | - | 120 | passed 0s | S | a: passed 0s |
| 522 | awaken-session-executor | `installed_session::resource_transition::session_resource_replacement_requires_newer_or_authority_amended_generation` | session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment | - | 120 | passed 0s | S | a: passed 0s |
| 523 | awaken-session-executor | `redact::kani_proofs::trace_capture_clamp_is_exact_and_never_widens_persisted_content` | capture.persistence_redaction_selection_is_total_exact_and_non_widening | - | 120 | passed 0s | S | a: passed 0s |
| 524 | awaken-session-executor | `redact::kani_proofs::configured_capture_redactor_selection_is_total_and_exact` | capture.persistence_redaction_selection_is_total_exact_and_non_widening | - | 120 | passed 0s | S | d: harness edited after last run |
| 525 | awaken-session-mcp | `mcp::credential_target::mcp_credential_realization_preserves_the_request_target_exactly` | credential_effect.mcp_realization_preserves_exact_authoring_target | - | 121 | passed 0s | S | a: passed 0s |
| 526 | awaken-session-realization | `environment_holding::lifecycle::closed_recovery::closed_environment_recovery_consumes_only_rebuildable_owner_classes` | session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner | - | 122 | passed 0s | S | a: passed 0s |
| 527 | awaken-session-realization | `environment_holding::lifecycle::owner_decisions::durable_projection_identity_is_exact_or_a_same_physical_owner_stutter` | session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner | - | 122 | passed 0s | S | a: passed 0s |
| 528 | awaken-session-realization | `environment_holding::lifecycle::owner_decisions::committed_candidate_retains_effect_kind_and_rejects_identity_relabeling` | session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner | - | 122 | passed 0s | S | a: passed 0s |
| 529 | awaken-session-realization | `resource_staging::projection::transition::explicit_empty_resource_manifest_revokes_without_activation` | session_resources.explicit_empty_manifest_revokes_runtime_projection | - | 122 | passed 0s | S | a: passed 0s |
| 530 | awaken-session-execution-contract | `session_realization::agent_publication_projection::effective_publication_field_replacement_is_exact_and_noop_preserving` | agent_profile.effective_publication_field_replacement_is_exact_and_noop_preserving | - | 123 | passed 0s | S | a: passed 0s |
| 531 | awaken-session-execution-contract | `session_realization::agent_publication_projection::frozen_worker_agent_publication_is_exact_or_fails_closed` | agent_profile.worker_projection_requires_exact_frozen_publication | - | 123 | passed 0s | S | a: passed 0s |
| 532 | awaken-session-execution-contract | `session_realization::control_failure::session_realization_control_failure_disposition_is_total_exact_and_fail_closed` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 123 | passed 0s | S | d: harness edited after last run |
| 533 | awaken-session-execution-contract | `session_realization::control_failure::realization_ownership_loss_proof_is_total_and_exact` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 123 | passed 0s | S | d: harness edited after last run |
| 534 | awaken-session-execution-contract | `session_realization::effect_failure::driver_wait_never_extends_or_discards_the_original_retry_hint` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 123 | never | ~S | d: never recorded |
| 535 | awaken-session-execution-contract | `session_realization::effect_failure::uncounted_failures_bound_recovery_and_run_free_preparation` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 123 | passed 0s | S | d: harness edited after last run |
| 536 | awaken-session-execution-contract | `session_realization::effect_failure::capacity_failures_never_consume_or_exhaust_the_realization_budget` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 123 | passed 0s | S | a: passed 0s |
| 537 | awaken-session-execution-contract | `session_realization::effect_failure::provisioning_defers_only_before_the_providers_deadline` | session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing | - | 123 | passed 26s | S | d: harness edited after last run |
| 538 | awaken-session-execution-contract | `session_realization::exclusion::session_realization_permit_admits_only_a_current_acquired_slot` | session_realization.concurrent_phase_drivers_are_serialized_per_session<br>session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex | - | 124 | passed 0s | S | a: passed 0s |
| 539 | awaken-session-execution-contract | `session_realization::generation_authority::realization_generation_authority_requires_every_axis` | session_realization.generation_authority_requires_every_immutable_axis<br>session_realization.publication_excludes_staged_retired_generation | - | 124 | passed 1401s | XL | b: slowest pass 1401s |
| 540 | awaken-session-execution-contract | `session_realization::model::session_realization_assignment_never_shares_a_live_foreign_owner_or_reuses_an_epoch` | session_realization.concurrent_drivers_preserve_one_owner_fence | - | 124 | passed 0s | S | a: passed 0s |
| 541 | awaken-session-execution-contract | `session_realization::projection_receipt::projection_revision_admission_is_monotonic_and_exact` | session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner | - | 124 | passed 0s | S | d: harness edited after last run |
| 542 | awaken-session-realization-driver | `control_deadline::session_realization_control_permit_ownership_release_is_total_and_exact` | session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex | - | 125 | passed 0s | S | a: passed 0s |
| 543 | awaken-session-run-delivery | `commit_boundary::recovery_projection_cannot_mint_committed_authority` | background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection | - | 126 | passed 0s | S | a: passed 0s |
| 544 | awaken-session-run-delivery | `settled_step::verification::unsettled_running_is_always_rejected_and_settled_kinds_never_interchange` | runtime.settled_step_projection_is_closed_exact_and_non_widening | - | 126 | passed 0s | S | d: harness edited after last run G16 |
| 545 | awaken-session-run-delivery | `settled_step::verification::settled_step_projection_cannot_invent_pending_or_observations` | runtime.settled_step_projection_is_closed_exact_and_non_widening | - | 126 | passed 0s | S | d: harness edited after last run G16 |
| 546 | awaken-session-run-delivery | `thread_coordination::agent_message_source::session_agent_message_admission_requires_every_committed_field` | session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters<br>session.agent_message_requires_exact_committed_tool_source_and_payload | - | 126 | passed 0s | S | a: passed 0s |
| 547 | awaken-session-run-delivery | `thread_coordination::session_reply_receipt::resume_receipt_classification_is_exact_and_conflict_absorbing` | tool_result.conflicting_delivery_operation_never_reuses_a_receipt<br>tool_result.exact_delivery_retry_replays_one_receipt | - | 126 | passed 0s | S | a: passed 0s |
| 548 | awaken-session-run-delivery | `thread_coordination::terminal_quiescence::terminal_quiescence_rejects_only_unstarted_reservations` | session_terminal.unstarted_reservation_is_rejected_after_cancel | - | 126 | passed 0s | S | d: harness edited after last run |
| 549 | awaken-session-store | `session_identity_and_create_shapes_require_one_owned_live_row` | session.owner_saved_atomically<br>session.visible_row_always_owned | - | 127 | passed 0s | S | a: passed 0s |
| 550 | awaken-skill-store | `skill_version_retirement_is_visibility_only_and_total` | skill.pinned_version_survives_retirement | - | 128 | passed 0s | S | a: passed 0s |
| 551 | awaken-store-fs | `recovery_replay_preserves_complete_entry_and_time` | store_history.recovery_preserves_committed_time_and_receipt_identity | - | 129 | passed 0s | S | a: passed 0s |
| 552 | awaken-store-postgres | `dense_commit_sequence_is_exact_nonnegative_and_nonwrapping` | store_history.commits_form_one_dense_stable_global_order | - | 130 | passed 38s | M | a: passed 86s |
| 553 | awaken-store-schema | `verification::dense_migration_versions_are_strictly_increasing` | migration.versions_are_dense_and_increasing | - | 131 | passed 0s | S | a: passed 1s |
| 554 | awaken-store-schema | `verification::migration_step_never_rolls_back_or_skips_a_version` | migration.step_never_rolls_back_or_skips | - | 131 | passed 0s | S | a: passed 0s |
| 555 | awaken-store-schema | `verification::replaying_a_fully_applied_migration_plan_is_a_noop` | migration.replay_of_applied_plan_is_noop | - | 131 | passed 0s | S | a: passed 0s |
| 556 | awaken-tenancy | `verification::successful_scope_resolution_never_widens_authority` | tenancy.authority_never_widens | - | 132 | passed 0s | S | a: passed 1s |
| 557 | awaken-tenancy | `verification::any_uncovered_selector_fails_closed` | tenancy.all_selectors_checked | - | 132 | passed 0s | S | a: passed 1s |
| 558 | awaken-tenancy | `verification::selector_order_cannot_change_an_authorized_result` | tenancy.selector_agreement | - | 132 | passed 1s | S | a: passed 1s |
| 559 | awaken-tool-contract | `tool::created_tool_handle_validation_never_invents_termination` | session_execution.one_physical_attempt_per_thread | solver-kissat-v1 | 27 | passed 135s | M | a: passed 240s |
| 560 | awaken-tool-contract | `tool::same_resource_conflict_is_symmetric_and_exactly_one_write_or_more` | tool.concurrency_resource_conflict<br>tool_schedule.every_wave_is_pairwise_compatible<br>tool_schedule.serial_is_singleton_and_compatible_prefixes_are_maximal | - | 133 | passed 0s | S | a: passed 0s |
| 561 | awaken-tool-contract | `tool::tool_wave_extension_requires_every_exact_guard` | tool_schedule.every_wave_is_pairwise_compatible<br>tool_schedule.order_is_stable_and_conflicting_predecessors_are_never_bypassed<br>tool_schedule.parallel_bound_is_positive_exact_and_zero_normalizes_to_one<br>tool_schedule.partition_is_total_contiguous_and_exactly_once<br>tool_schedule.serial_is_singleton_and_compatible_prefixes_are_maximal | - | 133 | passed 0s | S | a: passed 0s |
| 562 | awaken-tool-contract | `tool::tool_wave_extension_never_crosses_the_positive_bound_or_admits_serial` | tool_schedule.parallel_bound_is_positive_exact_and_zero_normalizes_to_one<br>tool_schedule.serial_is_singleton_and_compatible_prefixes_are_maximal | - | 133 | passed 0s | S | a: passed 0s |
| 563 | awaken-tool-contract | `tool_batch::verification::terminal_calls_are_never_reentered` | tool.transition_preconditions | - | 134 | passed 0s | S | a: passed 0s G17 |
| 564 | awaken-tool-contract | `tool_batch::verification::only_the_matching_approval_ticket_enters_execution` | tool.transition_preconditions | - | 134 | passed 0s | S | a: passed 0s G17 |
| 565 | awaken-tool-contract | `tool_batch::verification::known_returns_only_admit_materialization_and_never_physical_reentry` | tool.transition_preconditions | - | 133 | never | ~S | d: never recorded |
| 566 | awaken-tool-contract | `tool_batch::verification::executor_entry_admission_requires_exactly_executing_phase` | tool.executor_entry_order | - | 133 | passed 0s | S | d: harness edited after last run |
| 567 | awaken-tool-contract | `tool_batch::verification::recovery_reentry_requires_policy_capability_and_remaining_budget` | tool.recovery_policy | - | 134 | passed 2s | S | d: harness edited after last run |
| 568 | awaken-tool-contract | `tool_batch::verification::every_tool_call_transition_has_the_unique_documented_precondition` | tool.transition_preconditions | - | 133 | passed 0s | S | d: harness edited after last run |
| 569 | awaken-tool-contract | `tool_batch::verification::a_tool_call_completion_is_applied_at_most_once` | tool_result.call_is_consumed_at_most_once | - | 133 | passed 0s | S | d: harness edited after last run |
| 570 | awaken-tool-contract | `tool_batch::verification::terminal_tool_calls_only_accept_result_staging` | tool.transition_preconditions | - | 134 | passed 0s | S | d: harness edited after last run |
| 571 | awaken-tool-contract | `tool_batch::verification::run_end_sealing_targets_exactly_nonterminal_calls` | tool.transition_preconditions | - | 134 | passed 0s | S | d: harness edited after last run |
| 572 | awaken-tool-contract | `tool_batch::verification::a_second_wait_is_never_admitted` | tool.transition_preconditions | - | 133 | passed 0s | S | a: passed 0s |
| 573 | awaken-tool-contract | `tool_batch::verification::only_finalized_batches_expose_staged_publication` | tool.publication_barrier | - | 134 | passed 0s | S | a: passed 0s |
| 574 | awaken-tool-contract | `tool_discovery::every_u8_is_admitted_exactly_when_it_is_within_the_search_limit` | tool.discovery_limit | - | 134 | passed 0s | S | a: passed 0s |
| 575 | awaken-tool-relay | `executor::verification::hand_channel_only_exact_decoded_reply_restores_readiness` | remote_tool.cancelled_channel_never_correlates_a_late_reply_to_another_call | - | 135 | passed 0s | S | a: passed 0s |
| 576 | awaken-tool-relay | `executor::verification::hand_result_disposition_allows_only_explicit_success` | remote_tool.indeterminate_never_false_success | - | 135 | passed 0s | S | a: passed 2s |
| 577 | awaken-tool-relay | `executor::verification::durable_hand_identity_requires_the_runtime_operation_token` | remote_tool.stable_operation_identity | - | 135 | passed 0s | S | a: passed 0s |
| 578 | awaken-tool-relay | `ledger::verification::ledger_completion_requires_the_exact_executing_operation_entry` | remote_tool.result_matches_operation | - | 135 | passed 0s | S | a: passed 0s |
| 579 | awaken-tool-relay | `ledger::verification::ledger_begin_allows_claim_only_from_absent_and_never_from_replay` | remote_tool.invoke_at_most_once | - | 135 | passed 0s | S | a: passed 0s |
| 580 | awaken-tool-relay | `wire::verification::recovery_control_plan_never_selects_invocation_admission` | remote_tool.controls_never_admit_invocation | - | 135 | never | ~L | d: never recorded |
| 581 | awaken-tool-relay | `wire::verification::hand_protocol_requires_supported_version_and_v2_operation_identity` | remote_tool.protocol_version_requires_stable_v2_operation_identity | - | 135 | passed 0s | S | d: harness edited after last run |
| 582 | awaken-work-store | `mutation::work_lease_epoch_is_exact_nonnegative_and_nonwrapping` | work.persisted_epoch_is_exact_nonnegative_and_nonwrapping | - | 136 | passed 0s | S | a: passed 1s |
| 583 | awaken-work-store | `mutation::work_lease_authority_transition_is_complete_and_fenced` | work.lease_authority_snapshot_and_owner_mutations_are_atomic | - | 136 | passed 0s | S | a: passed 0s |
| 584 | awaken-work-store | `schema::clock_proofs::work_lease_ttl_selection_is_exact` | work.exact_boundary_reclaim | - | 136 | never | ~S | d: never recorded |
| 585 | awaken-work-store | `schema::clock_proofs::work_lease_deadlines_clamp_without_wrapping` | work.exact_boundary_reclaim | nightly | 136 | passed 592s | L | d: harness edited after last run |
| 586 | awaken-work-store | `schema::clock_proofs::work_reclaim_cutoff_is_exact_and_underflow_free` | work.exact_boundary_reclaim | - | 136 | passed 0s | S | a: passed 0s |
| 587 | awaken-worker | `credential_liveness::observation_batch_is_single_generation_and_never_wraps` | worker_credential.heartbeat_observations_are_one_atomic_batch | - | 137 | passed 0s | S | a: passed 0s |
| 588 | awaken-worker | `worker_realization_scope_source_is_total_and_exact` | worker.realization_scope_uses_only_the_kubernetes_namespace_or_persisted_local_workspace | - | 137 | passed 0s | S | d: harness edited after last run |
| 589 | awaken-worker | `lifecycle::environment_evidence::environment_evidence_decision_never_refreshes_expired_proof` | worker_authority.independent_evidence_never_extends_expired_proof | - | 137 | passed 0s | S | a: passed 0s |
| 590 | awaken-worker-contract | `capability_agreement::incompatible_capability_evidence_never_recovers_to_available` | worker.acp_capability_is_detected_only_after_a_verified_handshake | - | 138 | passed 0s | S | a: passed 0s |
| 591 | awaken-worker-contract | `verification::accepted_version_is_inside_worker_range` | worker.accepted_protocol_version_is_inside_advertised_range | - | 139 | passed 1s | S | a: passed 2s |
| 592 | awaken-worker-contract | `verification::non_ready_worker_never_accepts_work` | worker.non_ready_state_never_accepts_work | - | 139 | passed 0s | S | a: passed 0s |
| 593 | awaken-worker-contract | `verification::sandbox_tool_recovery_claim_axis_is_exact_and_non_widening` | worker.sandbox_tool_recovery_is_a_hard_claim_axis | - | 140 | passed 0s | S | a: passed 0s |
| 594 | awaken-worker-contract | `verification::manifest_recovery_mapping_accepts_only_the_exact_installed_capability` | worker.sandbox_tool_recovery_is_a_hard_claim_axis | - | 139 | passed 0s | S | a: passed 0s |
| 595 | awaken-worker-contract | `verification::dynamic_evidence_can_only_restrict_ready_worker_admission` | worker.readiness_is_probe_independent_but_dynamic_evidence_gates_claim | - | 139 | passed 0s | S | a: passed 0s |
| 596 | awaken-worker-contract | `verification::process_readiness_after_startup_is_probe_independent` | worker.readiness_is_probe_independent_but_dynamic_evidence_gates_claim<br>worker_credential.probe_failure_does_not_drain_worker | - | 139 | passed 0s | S | a: passed 0s |
| 597 | awaken-worker-contract | `verification::worker_dynamic_observation_requires_exact_fact_and_half_open_lease` | worker_credential.dynamic_observation_requires_exact_fact_and_half_open_lease<br>worker_credential.expired_observation_never_executes | - | 140 | passed 0s | S | a: passed 0s |
| 598 | awaken-worker-contract | `verification::never_replace_rejects_every_replacement` | worker.never_replace_rejects_every_replacement | - | 139 | passed 0s | S | a: passed 0s |
| 599 | awaken-worker-contract | `verification::sandbox_continuity_authorizes_replacement_exactly_when_bound` | worker.sandbox_continuity_requires_a_binding | - | 140 | passed 0s | S | a: passed 0s |
| 600 | awaken-worker-contract | `verification::same_incarnation_never_spends_replacement_authority` | worker.same_incarnation_does_not_spend_replacement_authority | - | 139 | passed 0s | S | a: passed 0s |
| 601 | awaken-worker-contract | `verification::required_worker_capability_cannot_be_omitted` | session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin | - | 139 | passed 0s | S | a: passed 0s |
| 602 | awaken-worker-contract | `observation_budget::observation_publication_budget_is_strict_and_overflow_safe` | worker_credential.publication_budget_covers_probe_and_delivery | - | 138 | passed 1s | S | d: harness edited after last run |
| 603 | awaken-worker-contract | `placement::placement_extension_admits_only_allowed_unique_identity` | worker.placement_extension_cannot_widen_eligibility | - | 138 | passed 0s | S | a: passed 0s |
| 604 | awaken-worker-contract | `registry::worker_replacement_and_heartbeat_share_one_non_resurrection_boundary` | worker.drain_and_replacement_races_preserve_registry_authority<br>worker.incarnation_generation_fences_stale_processes<br>worker_credential.stale_heartbeat_never_rolls_back_revision | - | 138 | passed 0s | S | a: passed 0s |
| 605 | awaken-worker-contract | `registry::worker_process_exit_receipt_requires_closed_exact_immutable_history` | sandbox.k8s_process_observation_requires_complete_exit | - | 138 | passed 0s | S | a: passed 0s |
| 606 | awaken-worker-contract | `registry::changed_worker_observation_always_mints_a_strictly_fresh_fence` | observation_reconcile.evidence_aba_has_fresh_fence<br>observation_reconcile.skipped_epoch_was_published_or_superseded | - | 138 | passed 0s | S | a: passed 0s |
| 607 | awaken-worker-contract | `registry::observation_publication_fence_is_success_owned_exact_and_failure_stable` | observation_reconcile.evidence_aba_has_fresh_fence<br>observation_reconcile.failed_refresh_does_not_advance_fence<br>observation_reconcile.skipped_epoch_was_published_or_superseded<br>observation_reconcile.unchanged_evidence_coalesces | - | 138 | passed 0s | S | a: passed 0s |
| 608 | awaken-worker-contract | `registry::worker_registration_never_grants_authority_to_seen_or_unknown_history` | worker.incarnation_generation_fences_stale_processes | - | 138 | passed 0s | S | a: passed 0s |
| 609 | awaken-worker-transport-security | `worker_transport_selector_admits_only_supported_exact_postures` | worker.remote_transport_is_secure_exact_and_identity_bound | - | 141 | passed 0s | S | a: passed 0s |
| 610 | awaken-worker-transport-security | `remote_transport_never_downgrades_or_widens_identity` | worker.remote_transport_is_secure_exact_and_identity_bound | - | 141 | passed 0s | S | a: passed 0s |
