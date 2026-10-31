# Remote Task Control and the Queue-Only Session Application

Status: designed. Implemented: the Task observation codec and delivered
candidates (`awaken-ext-background-task`), the private Run input and the codec
demand (`awaken-session-application-contract`, `awaken-dispatch-contract`),
Task attention admission with its acknowledgement
(`SessionRunApplication::submit_task_attention`), attention that carries the
observation and claimed input offered before execution
(`awaken-session-executor`), and the Coordinator's Task attention route
(`awaken-dispatch-http`). Remaining: the Worker side and the queue-only
application ([Remaining](#remaining)).
Principles: P2 (one owner per state machine), P5 (construction-time
injection), P6 (errors are values) of the architecture plan.

## Problem

A BackgroundTask runs its tool after the Run that started it has committed. Its
result becomes Thread state only when a later Run's StepStart folds it. Today
the result lives only in the process that ran the tool
(`BackgroundTaskSupervisor`), and the executor asks the co-located Session
application for that later Run through `SessionApplicationBinding`. A claimed
Worker has neither: the later Run can land on another Worker, and the Worker
has no co-located application. `context/plan.rs` therefore refuses
`background_task` on a claimed Worker.

## Decisions

### Authority: who may launch, fold and deliver a Task result

The Task holder is the identity in the Task attempt's tenure
(`TaskTenure::holder`). It never derives from a Run claim.

| Row | Cause | Effect |
|---|---|---|
| A1 | co-located process | holder = the process incarnation (`background-worker-v1` fingerprint, unchanged); attention goes to the bound application |
| A2 | registered Worker | holder = `registered_task_holder(owner, incarnation)`: the authenticated registry owner plus a per-incarnation nonce |
| A3 | attention whose holder names another owner than the authenticated Worker | refused `BadRequest`; no Run is admitted |
| A4 | Standalone (no Session application) | `background_task` refused at plan (unchanged) |
| A5 | ACP backend | `background_task` refused at plan (unchanged) |

Per-Run private Task authority: the executor gives a claimed Run the Task
results its own claim carried (below). They are private to that Run: they
travel only on the dispatch row and the claim, never on a public wire, and a
Run that carried none can fold only what its own process executed.

### Admission: version and codec

| Row | Cause | Effect |
|---|---|---|
| C1 | the frozen snapshot enables `background_task` | the dispatch row demands `background-task-control/v1`; only a Worker that advertises it can claim (same compiler as `tool-result-journal/v1`) |
| C2 | `PrivateTaskInput` header `version` is 1 | decoded |
| C3 | header version missing, 0, any other number, not an integer, or an unknown field | the row does not decode: a historical or future row is refused, never read as "no Task input" |
| C4 | `TaskObservationRecord.version` is 1 and the body matches | decoded into a completion or a wait candidate |
| C5 | record version other than 1, unknown field, empty task id, zero epoch | refused by the Task codec (`TaskObservationCodecError`); the claimed Run fails before execution |

A local claim pool ignores codec demands (unchanged): its process owns the
codecs it compiled.

### StepStart: claim-bound fold

The claim's records are offered to the supervisor before the Run executes
(`BackgroundTaskSupervisor::accept_delivered`). The existing StepStart law then
folds them; it compares each candidate with the committed attempt.

| Row | Slot before | Delivered record | Effect |
|---|---|---|---|
| D1 | vacant | any | slot = the record (Completed or Waiting) |
| D2 | older epoch, any phase | any | replaced by the record |
| D3 | same tenure, Active | any | unchanged: this process still executes the attempt |
| D4 | same tenure, Waiting | Ended | replaced by Completed |
| D5 | same tenure, Waiting | Waiting | unchanged |
| D6 | same tenure, Completed | any | unchanged |
| D7 | newer epoch, or same epoch with another holder | any | unchanged: stale or incomparable |

| Row | Committed attempt vs candidate | Fold (existing law) |
|---|---|---|
| S1 | equal tenure | stage the candidate; keep it for commit-loss retry |
| S2 | committed epoch newer | retire the candidate |
| S3 | no attempt, or incomparable | preserve; no effect |

### Tenure: independent of the Run claim

| Row | Cause | Effect |
|---|---|---|
| T1 | the Run that started the Task settles or loses its claim | Task tenure unchanged; the Task lease (`BACKGROUND_TASK_LEASE`) governs |
| T2 | the Task lease expires | any StepStart reclaims (existing law: new epoch, or terminal for never-replay) |
| T3 | a delivered record names an older epoch than the committed attempt | inert (S2) |

### ACK: durable delivery of a result

The application method is `SessionRunBackgroundApplication::submit_task_attention`.
It admits one background Run whose dispatch row carries the record, through the
same reservation, activity and activation as `submit_session_run_background`.

| Row | Cause | Effect |
|---|---|---|
| K1 | the Run is reserved, its activity committed and it is activated | `TaskAttentionAck { run_id }`; the sender stops retrying |
| K2 | the same operation id again (lost ACK, retry) | the same Run id: the operation id and the record are in the command fingerprint |
| K3 | the same operation id with a different record | refused: reused Run id with different input |
| K4 | `BadRequest` (holder/owner mismatch, Session gone, codec refused) | definitive; the sender stops; the candidate waits for the lease law |
| K5 | unavailable or transport failure | retried with the existing backoff; then the lease law |

The operation id is `background-task-attention-{fingerprint}` over Thread, Task,
holder, epoch and attention kind (unchanged), so K2 holds across processes.

### Queue-only Session application (Family A, remedy (a))

| Row | Placement | Application roles | Executor callbacks |
|---|---|---|---|
| Q1 | `RegisteredWorker` | `WorkerPlacedSessions`: Dispatch queue, committed reads, frozen projections; no executor | a Worker reaches the application over the signed transport (`ClaimedSessionCoordinationControl`, Task attention route) |
| Q2 | `LocalWorker` (today) | the executor's roles (`ChildRunCoordination`, `SessionRuns`, `RunAssembly`, `RunCapabilities`) | through `SessionApplicationBinding` |
| Q3 | target | every deployment is Q1; the co-located executor is an in-process registered Worker | construction-time injection; `SessionApplicationBinding` deleted |

Latency for Q3: the in-process Worker waits on the same `WakeSignal` that
`WorkerPlacement.wake` publishes on every activation, so a child Run starts on
the next local claim without the remote poll interval.

Removal condition for `SessionApplicationBinding`: Q2 no longer exists.

## Wire and persisted changes

- `RunDispatch.private_task_input: Option<PrivateTaskInput>` (dispatch row,
  omitted when absent; older rows decode as `None`).
- `AdmitSessionRun.private_task_input`, included in the command fingerprint.
- Worker capability `background-task-control/v1`.
- `PrivateTaskInput { version: 1, observations: [TaskObservationRecord] }`.

## Remaining

1. Claimed committed reads. The terminal observer launches Tasks from
   `CommittedAuthority`, which a Worker's recovery projection cannot mint
   (`background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection`).
   A Worker needs the state its own acknowledged claimed commit wrote, as a
   per-Run committed snapshot handed to that Run's observer.
2. Worker selection. A registered Worker builds its supervisor with
   `registered_task_holder(worker_id, incarnation_id)` and publishes attention
   through `WORKER_TASK_ATTENTION_PATH` (client on `WorkerControlClient`) in
   place of the binding, which has no application on a claimed Worker.
3. Lift the guard in `context/plan.rs` ("background_task requires a co-located
   Session application ...") once 1 and 2 hold. The codec demand (C1) then
   keeps Workers without `background-task-control/v1` from claiming.
4. Queue-only application (Q3). Retire the `LocalWorker` placement: the
   co-located executor runs as an in-process registered Worker over the same
   Dispatch store, with in-process implementations of the claimed transport
   ports (commit, realization control, coordination control, Task attention)
   that apply the same authority checks as the HTTP handlers. Then delete
   `SessionApplicationBinding`, `SessionApplications::Local`, its exemption in
   [injection-and-error-cleanup.md](injection-and-error-cleanup.md#exemptions-for-the-v1-guard)
   and `test_application_bindings`. Executor unit tests that build a local
   application move to the in-process Worker fixture.
