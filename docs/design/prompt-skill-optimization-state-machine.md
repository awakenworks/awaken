# Prompt And Skill Optimization State Machine

This document owns the dynamic lifecycle for
[ADR-0068](../adr/0068-unified-prompt-and-skill-optimization.md). The global
flow freezes a target and dataset, establishes a baseline, proposes candidates,
validates them with increasingly strong evidence, runs one sealed test, and
promotes only through the target owner. This document focuses on causality,
state transitions, consistency, failures, retries, cancellation, and erasure.
Static fields and ports are defined in the companion
[data contracts](prompt-skill-optimization-data-contracts.md).

## State Structure

Do not encode every concern in one combinatorial enum. Three orthogonal state
machines share ids and revision fences:

```text
OptimizationJob lifecycle       Candidate lifecycle       Data disposition
-------------------------       -------------------       ----------------
Created                         Generated                 Active
Preparing                       Screening                 Restricted
BaselineRunning                 ScreenedOut               ErasurePending
Generating                      ValidationRunning         Erased
Screening                       Validated
Validating                      TestRunning
TestRunning                     Eligible
AwaitingPromotion               Rejected
Promoting
Promoted | Rejected | Cancelled | Errored
```

`JobState` is the aggregate's workflow authority. Candidate state cannot make a
job terminal. `DataDisposition` is a privacy overlay: anything except `Active`
denies new content reads, inference, tool calls, tests, and promotion regardless
of job/candidate state.

Leases, retry counts, budgets, cancel intent, sandbox status, and target
publication state are facts attached to transitions; they are not extra job
states.

## Job Lifecycle

```text
Created
  -> Preparing
       -> BaselineRunning
            -> Generating
                 -> Screening
                      -> Validating
                           +-> Generating       (next bounded round)
                           `-> TestRunning      (one selected candidate)
                                -> AwaitingPromotion
                                     -> Promoting
                                          -> Promoted

Any non-terminal state -> Cancelled
Any running state      -> Errored  (unrecoverable infrastructure/policy fault)
Preparing..TestRunning -> Rejected (no eligible candidate / quality gate)
Awaiting/Promoting     -> Rejected (stale base / promotion denial)
```

Terminal states are `Promoted`, `Rejected`, `Cancelled`, and `Errored`. They are
immutable except for completion of separately tracked artifact erasure and
accountability receipts.

### Transition table

| From -> To | Trigger | Guard | Atomic state write / action |
|---|---|---|---|
| absent -> `Created` | `CreateOptimization` | authorized Workspace; idempotency key new or matching; target pin and dataset exist; finite retention/privacy policy valid | insert job revision 1 and creation event |
| `Created` -> `Preparing` | worker claim | no cancel/restriction; lease epoch current | freeze all model/tool/env/scorer/policy hashes and dataset capabilities |
| `Preparing` -> `BaselineRunning` | preparation complete | base pin unchanged; dataset Active; sandbox/egress capabilities admit all planned stages; budgets sufficient | persist frozen-inputs hash and baseline work items |
| `BaselineRunning` -> `Generating` | baseline report complete | every required observation terminal; report committed; baseline hard-gate semantics valid | attach baseline report ref and round 1 budget |
| `Generating` -> `Screening` | proposal batch complete | candidate count and proposal budget within bounds; every materialized artifact checksum/lineage committed | freeze batch ids; revoke proposal writes for the round |
| `Screening` -> `Validating` | static/LLM screen complete | at least one candidate survived every static hard gate | rank survivors deterministically and create validation work |
| `Screening` -> `Rejected` | screen complete | no survivor | terminal reason `no_candidate_passed_screen` |
| `Validating` -> `Generating` | round decision | no candidate eligible; another round and budget remain; failure attribution committed | expose training failures only; increment round |
| `Validating` -> `TestRunning` | select candidate | one candidate dominates policy or deterministic tie-break; all validation work terminal; test never opened | persist selected id and one-time sealed-test grant atomically |
| `Validating` -> `Rejected` | round decision | no eligible candidate and stop condition/budget reached | terminal reason `quality_gate` or `budget_exhausted` |
| `TestRunning` -> `AwaitingPromotion` | sealed report complete | selected candidate passes all test hard gates and privacy scan; target base still current | mark candidate Eligible; commit comparison report; revoke test grant |
| `TestRunning` -> `Rejected` | sealed report complete | any hard gate fails or selected evidence is invalid | terminal reason `sealed_test_failed`; test is not reopened |
| `AwaitingPromotion` -> `Promoting` | authorized promote command or admitted auto-policy | expected job revision; candidate Eligible; data Active; target base current; approval/effect policy satisfied | write promotion intent keyed by idempotency key and candidate hash |
| `Promoting` -> `Promoted` | target commit observed | target-owned commit exactly matches candidate hash and old base pin | store receipt and terminal event atomically |
| `AwaitingPromotion`/`Promoting` -> `Rejected` | CAS/policy failure | target changed, approval denied/expired, or target owner rejects candidate | terminal reason; never rebase in place |
| any non-terminal -> `Cancelled` | cancel convergence | durable cancel intent or privacy cancellation; owned work quiesced/fenced | terminal cancellation event; cleanup continues idempotently |
| running -> `Errored` | unrecoverable fault | retry classifier says permanent or retry budget exhausted | terminal infrastructure/policy reason distinct from quality |

Every transition compares `expected_revision`; worker transitions also compare
the current lease epoch. State, revision, event, budget counters, and newly
reachable artifact/report references commit in one metadata transaction.

## End-To-End Dynamic Sequence

```text
Client       Coordinator      JobStore       Eval/Runtime       Sandbox/Tools      Target owner
  | create       |               |                 |                  |                 |
  |------------->| validate/CAS  |                 |                  |                 |
  |              |------------->| Created         |                  |                 |
  |              | claim/freeze |                 |                  |                 |
  |              |------------->| Preparing       |                  |                 |
  |              | baseline work|---------------->| create/admit ---->|                 |
  |              |              |<-- observations/reports -----------|                 |
  |              | propose(train only)            |                  |                 |
  |              | screen -> validate------------>| real runs ------>|                 |
  |              | select + open test once ------>| sealed test ---->|                 |
  |              |<------------- eligible report |                  |                 |
  | promote      |               |                 |                  |                 |
  |------------->| intent/CAS -->| Promoting       |                  |-- target CAS -->|
  |              | reconcile receipt <-------------------------------- target commit --|
  |<-------------| Promoted      |                 |                  |                 |
```

The proposer never receives validation/test verdicts or another candidate's
output. Evaluation runs receive the same immutable candidate and frozen inputs;
only their split-scoped data capability differs.

## Candidate Lifecycle

```text
Generated -> Screening -> ScreenedOut
                    `-> ValidationRunning -> Rejected
                                           `-> Validated
Validated (selected once) -> TestRunning -> Rejected
                                         `-> Eligible
```

| Transition | Required evidence |
|---|---|
| `Generated -> Screening` | proposer Run committed; target-specific candidate materializes; checksum, diff, lineage, and privacy scan stored |
| `Screening -> ScreenedOut` | at least one static hard gate failed; exact gate and artifact evidence recorded |
| `Screening -> ValidationRunning` | all static hard gates passed; optional LLM screen meets only its soft/declared semantic threshold |
| `ValidationRunning -> Rejected` | deterministic/real-run hard gate failed or observation set is invalid |
| `ValidationRunning -> Validated` | required sample complete; all hard gates pass; comparison to frozen baseline complete |
| `Validated -> TestRunning` | job atomically selects this candidate and opens the one-time sealed test grant |
| `TestRunning -> Rejected` | sealed hard gate fails, provider/config drift invalidates evidence, or privacy quarantine occurs |
| `TestRunning -> Eligible` | sealed evidence complete and all promotion prerequisites except authority approval pass |

A candidate rejection is immutable. A revised artifact is a new `CandidateId`
with a parent link. This preserves negative examples and prevents overwriting a
failed trajectory.

Selection order is deterministic: hard-gate pass, configured primary metric,
confidence requirement, secondary metrics in declared order, cost/latency, then
stable candidate id. The selector may not ask an LLM to break an undeclared tie.

## Evaluation Work And Retry Semantics

Each work item is uniquely keyed by:

```text
(job_id, baseline_or_candidate_id, split, dataset_item_id, repetition,
 frozen_inputs_sha256)
```

Work status is `Pending -> Claimed -> Completed | Failed | Cancelled`. A claim
has an epoch and expiry. An expired claim can be reclaimed; stale workers cannot
commit because observation insertion and work settlement compare the epoch.

Failures are classified before retry:

| Failure class | Example | Retry | Quality denominator |
|---|---|---|---|
| transient infrastructure | connection reset, worker crash before effect, temporary provider 5xx | same frozen work, bounded exponential backoff with jitter | excluded from model score; reported |
| quota/rate limit | 429, billing quota | retry only within declared deadline/budget; never switch provider/model | excluded; job may end Errored |
| indeterminate effect | timeout after external write | reconcile by idempotency/effect receipt before any retry | excluded until resolved; fail closed |
| deterministic candidate failure | parser, compiler, admission, secret leak | never retry unchanged candidate | included as candidate hard failure |
| model output failure | invalid schema, wrong answer/tool sequence | repeat only when plan declared repetitions; no ad-hoc repair hidden from report | included |
| sandbox/policy unsupported | no Container, allowlist, or secret-substitution guarantee | never downgrade; non-retryable until deployment changes | job Errored before content leaves boundary |
| privacy restriction | consent/lawful basis withdrawn, erasure/objection | no retry; restrict and cancel | not a quality failure |

Bounded repair attempts, if part of a production contract, are frozen as their
own observation steps. First-pass schema reliability remains separately visible;
repair never overwrites it.

## Sandbox And External Effect Lifecycle

For each real case:

```text
plan SandboxSpec
  -> prepare_environment(capabilities)       [fail closed]
  -> SandboxProvider::create
  -> persist SandboxHandle before workload
  -> materialize candidate + split-scoped inputs
  -> spawn ordinary Agent/CLI or use Runtime + ToolExecutor
  -> collect committed output, tool/effect receipts, artifacts
  -> settle observation under claim epoch
  -> teardown/reap and persist cleanup receipt
```

One case gets one sandbox by default. Reuse is allowed only for an explicit
reuse group with identical target/candidate/environment/privacy subject set and
a clean immutable base volume plus fresh writable session volume. No writable
cache or home directory crosses candidates, dataset splits, or subject sets.

External effects are fenced before execution:

- Recorded tools have no network and replay an exact fixture hash.
- Ephemeral integrations provision a unique namespace/account, persist its
  cleanup handle before calls, and block observation completion until cleanup is
  confirmed or durable recovery work exists.
- Live canaries require approval and persist an effect intent plus idempotency
  key before the call. Success, known failure, and indeterminate response are
  separate. An indeterminate outcome is reconciled; it is never blindly retried.

Secret material is resolved immediately before the admitted egress boundary and
never enters the job, observation, sandbox handle, command log, or artifact.

## Promotion Transaction And Recovery

Promotion cannot be one cross-store ACID transaction, so it is a recoverable
intent protocol:

```text
1. JobStore CAS: AwaitingPromotion -> Promoting + PromotionIntent
2. Target adapter reads exact current base pin
3. Target-owned idempotent CAS/appended-version commit
4. JobStore CAS: record target commit + PromotionReceipt -> Promoted
```

Recovery of `Promoting` reads the target by exact identity:

- if a revision/version with the intent idempotency key and candidate hash
  exists, record the missing receipt and finish `Promoted`;
- if the old base is still current and no commit exists, retry the target commit
  under the same intent;
- if a different target commit exists, finish `Rejected(stale_base)` and alert;
- if target state is indeterminate/unreadable, remain `Promoting` with bounded
  recovery retries; never report promotion success.

For Skill, ordinal assignment and latest-pointer movement stay atomic inside
`SkillStore::append_version`. For Agent/Judge, config CAS and publication use the
existing config workflow. An unpublished draft is not a successful promotion.

## Cancellation

`CancelOptimization` first CAS-writes `cancel_requested_at`; it does not assume a
worker stopped. New claims, provider calls, sandbox processes, tool calls, test
opening, and promotion are denied immediately.

The current owner then:

1. cancels ordinary Runs through their existing cancellation seam;
2. signals sandbox processes and waits for the bounded grace period;
3. reconciles indeterminate external effects;
4. tears down or leaves durable reap work for every sandbox/integration;
5. fences outstanding work claims;
6. commits `Cancelled`.

Cancellation preserves governed reports and candidates until retention/erasure;
it does not promote the best-so-far candidate. Repeated cancel requests are
idempotent.

## GDPR Restriction And Erasure Lifecycle

```text
Active -> Restricted -> ErasurePending -> Erased
             |                |
             `---- legal hold / exception decision (remains Restricted)
```

An erasure, objection, purpose withdrawal, expired retention, or invalid lawful
basis first moves all related subject-artifact links to `Restricted` in a durable
transaction. Reads and processing fail immediately. Affected non-terminal jobs
receive a privacy cancel intent; dataset snapshots become Invalidated and can no
longer start or promote jobs.

Erasure fan-out then covers:

1. dataset item content and split-capability material;
2. model inputs/outputs and proposer/Judge transcripts;
3. candidate patches/materialized bundles and derived reports;
4. tool arguments/results, effect previews, sandbox files/homes, caches/indexes;
5. external processor deletion calls and receipts;
6. artifact ciphertext and erasure-capable keys.

Each eraser is idempotent and checkpointed. `ErasurePending` remains until every
registered eraser succeeds. Partial failure returns `erasure_incomplete`, keeps
all data restricted, retries within policy, and alerts the controller. Only the
aggregate resolver can issue the final erasure receipt. A lawful exception or
legal hold records the decision, retained categories, purpose, and review date;
it never fabricates an `Erased` state.

If an artifact concerns multiple subjects, erasing one subject deletes or
rebuilds the shared artifact so their content is no longer present. Merely
removing one association row is insufficient. New snapshots receive new ids and
exclude erased items.

## Crash Recovery And Reconciliation

On startup or lease expiry the coordinator scans non-terminal jobs:

1. reclaim only expired leases with a higher epoch;
2. verify data disposition, cancel intent, retention, dataset state, and target
   base before any content access;
3. reconcile pending artifacts and readable report references;
4. adopt or reap persisted sandboxes through the existing provider;
5. reconcile external effects before redispatch;
6. rebuild missing deterministic reports from append-only observations;
7. resume the current job state without regenerating already frozen candidates;
8. reconcile `Promoting` as specified above.

There is no `resume from current Agent/Skill`. Recovery always uses the frozen
base and candidate hashes. If those bytes are unavailable or invalidated, the
job ends `Errored` or privacy-cancelled; it does not fetch latest content.

## Stop Conditions

Candidate generation stops when the first condition holds:

1. a candidate meets the declared validation eligibility rule;
2. maximum rounds, candidates, provider calls, tokens, cost, effects, or wall
   time is reached;
3. two consecutive completed rounds produce no declared minimum improvement, if
   configured;
4. all failures are classified as non-generalizable/project-specific;
5. cancel or privacy restriction occurs.

Budget exhaustion with completed valid evidence is `Rejected(budget_exhausted)`;
infrastructure exhaustion without a valid comparison is `Errored`. This keeps
quality conclusions distinct from inability to evaluate.

## Terminal Outcome Contract

| Terminal state | Meaning | Promotion receipt | Retry by same job |
|---|---|---|---|
| `Promoted` | target owner committed the exact candidate and receipt | required | no |
| `Rejected` | valid evidence/policy says no winner or target became stale | absent | no; create a new rebased job |
| `Cancelled` | durable cancel/privacy intent converged and active work fenced | absent | no |
| `Errored` | required evidence could not be obtained or invariant/infrastructure recovery exhausted | absent | no; a new job may reuse an admissible snapshot |

An eligible candidate without a target receipt is never reported as Promoted. A
successful target commit without a job receipt remains `Promoting` until
reconciled, never `Errored` or falsely repeated.

## Required Verification

### Deterministic state tests

- every allowed transition and every illegal edge;
- optimistic revision and lease-epoch fencing;
- cancel racing proposal, observation, sealed-test opening, and promotion;
- retry classifier decision table and budget accounting;
- deterministic candidate tie-break and one-time sealed-test grant;
- all four terminal states immutable.

### Store conformance

Run identical in-memory, SQLite, and Postgres suites for claim expiry, stale
settle, idempotent observation, atomic selection/test seal, promotion intent,
restriction, and checkpointed erasure.

### Real E2E

- Agent instruction baseline -> candidate -> sandboxed validation -> CAS publish;
- Skill bundle with a real parser/compiler/tool fixture -> append one version;
- provider failure remains distinct from candidate score;
- network-disabled offline tool run succeeds, while authenticated external MCP
  fails admission on current capabilities;
- crash after target commit but before receipt reconciles to one promotion;
- erasure during validation immediately blocks reads, cancels the job, reaps the
  sandbox, invalidates the snapshot, and completes all registered deletion
  receipts;
- stale target change between test and promotion ends `Rejected(stale_base)`.
