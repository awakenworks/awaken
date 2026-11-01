# Remote Task Control and the Queue-Only Session Application

Status: designed. Implemented: the Task observation codec and delivered
candidates (`awaken-ext-background-task`), the private Run input and the codec
demand (`awaken-session-application-contract`, `awaken-dispatch-contract`),
Task attention admission with its acknowledgement
(`SessionRunApplication::submit_task_attention`), attention that carries the
observation and claimed input offered before execution
(`awaken-session-executor`), and the Coordinator's Task attention route
(`awaken-dispatch-http`). Worker side implemented: claim-bound committed reads
(`ClaimedTerminalReads`, R1–R6), holder and attention route selection
(`TaskAttentionRoute`, W1–W5) and the authenticated attention client
(`WorkerTaskAttentionClient`, K1–K6). Remaining: holder-bound Task tenure reads
(H2–H5), and with them the plan guard; the queue-only application
([Remaining](#remaining)).
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

## Worker side

A registered Worker runs a BackgroundTask with the same supervisor, observer
and StepStart law as a co-located process. Three inputs differ: who the holder
is, how the observer reads committed truth, and how an observation reaches the
Session application. The co-located (`LocalWorker`) placement keeps its
existing inputs (Q2); both placements stay supported until Q3.

### Committed reads: claim-bound, supplied by the Coordinator

The terminal observer decides which Tasks to launch, and hands the launched
tool its State, from committed truth. On a Worker that truth is the
Coordinator's. The Worker's recovery projection is a process-local cache of
one claim's prefix plus its own acknowledged commits; it may be replaced by the
next claim of the Session and it is never committed authority. The obligation
`background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection`
(Kani `recovery_projection_cannot_mint_committed_authority`) keeps it that way
and is unchanged.

The Worker instead reads from the Coordinator under the claim that is
delivering the Run's committed terminal. The Dispatch Worker delivers terminal
observers after committed truth is visible and **before** it settles the claim,
so that claim is live for the whole delivery. While it delivers, the Worker
records the claim in `ClaimedTerminalReads` (owned by
`awaken-dispatch-execution`); the observer's read is the existing
claim-guarded recovery read (`ClaimedCheckpointTransport::load_recovery_snapshot`,
route `/v1/worker/recovery/snapshot`), which the Coordinator answers from its
canonical stores. No new route and no new wire shape.

| Row | Cause | Effect |
|---|---|---|
| R1 | co-located: the commit boundary is local | `CommittedAuthority` (unchanged; only `SessionCommit::committed_authority` mints it) |
| R2 | Worker; this Worker is delivering the observed Run's terminal under claim C | the Coordinator's committed State of the Thread, read under C |
| R3 | Worker; no claim of the observed Run is being delivered (out-of-band observation, after settlement, another Run) | refused; the observer fails; nothing is launched |
| R4 | the Coordinator fences C (lost, superseded, stale incarnation) or the transport fails | refused; observer failure withholds settlement; the next claim redelivers (existing law) |
| R5 | the read names another Run or Thread than the delivering claim | refused |
| R6 | the Worker's recovery projection | never a source for the observer |

Two deliveries of the same Run overlap only across claims; each registration
removes only itself, so a finished delivery never removes a newer one.

### Task tenure reads: holder-bound, after the claim settles

A launched Task executes after its Run's claim settles. Before every tool
effect the runtime asks the attempt's ownership verifier whether the committed
Task slot still admits this attempt (`BackgroundTaskOwnership`). A co-located
process reads its `CommittedAuthority`. On a Worker no claim exists any more,
so this read is bound to the Task holder instead, the same authority that
admits attention (A2/A3).

| Row | Cause | Effect |
|---|---|---|
| H1 | co-located | `CommittedAuthority` (unchanged) |
| H2 | Worker; the authenticated incarnation's holder names the attempt | the Coordinator returns the committed Task slot; the Worker applies the existing admission (`admits_execution_of`, supervisor active) |
| H3 | the committed slot names another holder or epoch, or is absent | `Lost`; the attempt stops before the effect |
| H4 | the holder is not this incarnation's | refused before any read (A3) |
| H5 | transport failure or Coordinator unavailable | `Unavailable`; the effect does not run |

H2–H5 need a Coordinator route that decodes one Task slot for its holder. It
is not implemented ([Remaining](#remaining)); until it is, the plan guard
keeps refusing `background_task` on a registered Worker.

### Holder and attention route selection

| Row | Placement | Holder | Attention route | Committed reads |
|---|---|---|---|---|
| W1 | co-located (`hosts_session_application`), application bound | process fingerprint | `SessionApplicationBinding::background_runs` | R1, H1 |
| W2 | co-located, no background application bound | process fingerprint | none (attention is a no-op); a Managed Session refuses at plan | R1, H1 |
| W3 | registered Worker with `TaskAttentionDelivery` installed | `delivery.holder()` = `registered_task_holder(worker, incarnation)` | the authenticated Task attention route | R2–R5, H2–H5 |
| W4 | registered Worker without the delivery | n/a | none | `background_task` refused at plan |
| W5 | a co-located host given a `TaskAttentionDelivery` | n/a | n/a | construction refused: one holder per process |

The delivery carries its holder, so the holder that tenures a Task and the
identity that delivers its attention cannot come from different incarnations.
The standard Worker composition installs the delivery. It advertises
`background-task-control/v1` only when the plan admits W3, so until H2–H5
exist the codec demand (C1) keeps BackgroundTask Runs off registered Workers
and W3 is refused at plan.

### Worker ACK and retry

The client maps the Coordinator's single refusal classification
(`attention_failure`) back onto K1–K5. Only a decoded acknowledgement is an ACK.

| Row | Response | Effect |
|---|---|---|
| K1 | 2xx with `{"ack": {"run_id": …}}` | ACK: the carrying Run is durably admitted; the sender stops |
| K2 | any retry | the same `TaskAttention`, so the same operation id and record reach the same Run |
| K4 | 4xx other than 429 | `BadRequest`: definitive; the sender stops; the lease law takes over |
| K5 | transport failure, 429, 5xx | `Unavailable`: retried with the existing backoff |
| K6 | 2xx whose `ack` is missing or does not decode (unknown field, other shape) | `Unavailable`: not an ACK; the retry reaches the same Run (K2) |

## Remaining

1. Task tenure reads (H2–H5). A Coordinator route, holder-authenticated like
   the attention route, that returns the committed slot of one Task held by
   the authenticated incarnation in a versioned record; the Worker's
   ownership verifier (`TaskCommittedReads::tenure_state`) reads it in place
   of refusing. **Needs a decision:** the Coordinator closure
   (`awaken-coordinator`, `awaken-dispatch-http`, `awaken-session-application`,
   `awaken-process-startup`) has no Task codec, and the registered-execution
   Coordinator has no executor to ask. Either the Task slot model and its
   state key move into a contract crate the Coordinator may depend on, or the
   process root supplies a Task tenure port (which the registered-execution
   root cannot build without that codec either).
2. Lift the guard once 1 holds: drop the W3 refusal in
   `TaskAttentionRoute::plan`, and advertise `background-task-control/v1` in
   the standard Worker manifest. R1–R6, W1–W5 and K1–K6 are implemented and
   tested.
3. Queue-only application (Q3). Retire the `LocalWorker` placement: the
   co-located executor runs as an in-process registered Worker over the same
   Dispatch store, with in-process implementations of the claimed transport
   ports (commit, realization control, coordination control, Task attention)
   that apply the same authority checks as the HTTP handlers. Then delete
   `SessionApplicationBinding`, `SessionApplications::Local`, its exemption in
   [injection-and-error-cleanup.md](injection-and-error-cleanup.md#exemptions-for-the-v1-guard)
   and `test_application_bindings`. Executor unit tests that build a local
   application move to the in-process Worker fixture. This follows V8
   acceptance (user decision 2026-11-01).
