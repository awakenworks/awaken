# Remote Task Control and the Queue-Only Session Application

Status: source implementation candidate; integrated compilation, real regressions and API generation are pending. Implemented: the Task observation codec and delivered
candidates (`awaken-ext-background-task`), the private Run input and the codec
demand (`awaken-session-application-contract`, `awaken-dispatch-contract`),
Task attention admission with its acknowledgement
(`SessionRunApplication::submit_task_attention`), attention that carries the
observation and claimed input offered before execution
(`awaken-session-executor`), and the Coordinator's Task control routes
(`awaken-dispatch-http`). Worker side implemented: claim-bound committed reads
(`ClaimedTerminalReads`, R1–R6), holder-bound Task tenure reads
(`TaskCommittedReads::committed_task`, H1–H5), holder and attention route
selection (`TaskAttentionRoute`, W1–W5) and the authenticated Task control
client (`WorkerTaskControlClient`, K1–K6); the plan admits `background_task`
on a registered Worker. Remaining: the queue-only application
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
- Worker capability `background-task-control/v1`, advertised by the standard
  Worker manifest.
- `PrivateTaskInput { version: 1, observations: [TaskObservationRecord] }`.
- Route `/v1/worker/task/tenure`: request `{identity, read: TaskTenureRead
  {thread, partition, run, task, holder, epoch}}` with `partition` either
  `{"kind": "thread"}` or `{"kind": "session", "session_thread"}` (required), response `{tenure: TaskTenureRecord
  {version: 1, slot} | null}`. Both shapes refuse unknown fields; a record of
  any other version is refused.
- `awaken_agent_contract::thread::background_task`: the slot's state key and
  the holder of its current attempt (SDK addition inside the pending 2.0.0);
  `awaken-ext-background-task` no longer exports `BACKGROUND_TASK_STATE_PREFIX`.

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

The Worker sends `TaskTenureRead {thread, partition, run, task, holder, epoch}` with
its own holder, the Task's origin Run and the Session partition the Task's
Thread is committed in, over the authenticated route `/v1/worker/task/tenure`.
No claim exists at that point and the holder names only the incarnation, so
the partition travels with the read; the Worker derives it from the Session
its observer belongs to (`TaskReadPartition::of`). The Coordinator checks the
holder like attention (H4), reads the Thread's committed State through the
same partitioned read the Run's own commits use, and returns the slot only
when its current attempt names that holder.

| Row | Partition | Committed read |
|---|---|---|
| P1 | `thread`: the Task's Thread is its own partition (root, or no Session) | `RunRecoverySource::recovery_snapshot` |
| P2 | `session` naming the Session Thread of a child Thread | `recovery_snapshot_in_session(session, thread, origin Run)` |
| P3 | `session` naming the Task's own Thread | refused: one encoding per partition |
| P4 | the origin Run is not committed in the answered partition | refused (`BadRequest`), never read as an absent slot |
 It decodes
the holder through `awaken_agent_contract::thread::background_task`
(`committed_task_slot`, `committed_task_attempt`), so the Coordinator closure
never depends on the extension: the extension keeps the aggregate, its
decisions and its codec, and the Worker decodes the returned slot with that
codec before the existing admission. An ownership projection that does not decode
fails closed as H5; it is not a second complete aggregate validator. A carrying
Run's ACK proves durable admission only: its Task owner validates the complete
namespace at StepStart, and a decoding error stops that attempt before model or
tool work.

### Holder and attention route selection

| Row | Placement | Holder | Attention route | Committed reads |
|---|---|---|---|---|
| W1 | co-located (`hosts_session_application`), application bound | process fingerprint | `SessionApplicationBinding::background_runs` | R1, H1 |
| W2 | co-located, no background application bound | process fingerprint | none (attention is a no-op); a Managed Session refuses at plan | R1, H1 |
| W3 | registered Worker with `RegisteredTaskControl` installed | `control.holder()` = `registered_task_holder(worker, incarnation)` | the authenticated Task attention route | R2–R5, H2–H5 |
| W4 | registered Worker without Task control | n/a | none | `background_task` refused at plan |
| W5 | a co-located host given a `RegisteredTaskControl` | n/a | n/a | construction refused: one holder per process |

The Task control carries its holder, so the holder that tenures a Task, the
identity that delivers its attention and the identity that reads its tenure
cannot come from different incarnations. The standard Worker composition
installs it and advertises `background-task-control/v1`, so the codec demand
(C1) routes BackgroundTask Runs only to Workers that can execute them.

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

1. Queue-only application (Q3). Retire the `LocalWorker` placement: the
   co-located executor runs as an in-process registered Worker over the same
   Dispatch store, with in-process implementations of the claimed transport
   ports (commit, realization control, coordination control, Task attention)
   that apply the same authority checks as the HTTP handlers. Then delete
   `SessionApplicationBinding`, `SessionApplications::Local`, its exemption in
   [injection-and-error-cleanup.md](injection-and-error-cleanup.md#exemptions-for-the-v1-guard)
   and `test_application_bindings`. Executor unit tests that build a local
   application move to the in-process Worker fixture. This follows V8
   acceptance (user decision 2026-11-01).


## Authenticated attention and exact replay

TaskAttention carries one TaskTenureRead (Task, logical Thread, original Run,
Session partition, holder and nonzero epoch). The original PrivateTaskInput v1
wire remains readable without tenure, but no unbound input grants current Worker
execution. Current attention binds complete tenure into the same private command
input even for an input-required wake with no observation. Ordinary empty input
construction remains refused. Every observation decodes through the sole codec
at `awaken_tool_contract::tool::background_task` and must match all tenure axes;
HTTP and the actual claimed dispatch check the complete batch before admission or
offer, respectively. The Task aggregate/decisions remain in the extension.

The authenticated HTTP boundary first checks Worker incarnation and the complete
immutable command receipt. The existing Session application constructs the same
command fingerprint, and its original coordination read reaches the one
SessionRunAdmissionReads source. An exact accepted Live/Completed fact recovers
its original ACK even after Task holder/phase changes; missing evidence permits
only the subsequent actual committed held-Task check, never a guessed ACK. A
changed subject or payload conflicts. The root reservation port stays root-only;
Task children use the original bounded child admission with their complete
SessionCommand identity. Ordinary children remain FullDispatch. Completion has
no Task-child legacy-fingerprint fallback or second receipt ledger.

The original successful-response classifier treats HTTP2xx body/JSON decode
failures as Unknown/Unavailable. A malformed successful ACK is not a definitive
rejection: the same operation and input retry the original Run. Explicit4xx
refusals keep their original classification. The child assembly installs its own
BackgroundTask observer and shares one ClaimedTerminalReads with its actual
DispatchWorker; it never inherits the parent's BackgroundTask observer or reads
its cached recovery prefix as committed authority.


### Existing child identity and frozen execution source

A callback targets an existing logical Thread in the original physical Session.
The callback read reuses committed Native delegation registries and accepted
coordination ToolBatch results; it writes no relationship registry and does not
claim a cross-Thread atomic snapshot. The original creation origin preserves
Native depth and lineage when another callback Run carries a result on the same
Thread. The public observed-root coordination query keeps its original fence.

For queued execution, the existing Done transaction retains its ORIGINAL
RunDispatch only when the same Task-control predicate used by codec eligibility
requires it. The operational completion feed keeps its original DTO and bytes;
no second source ledger or copied Task model is created. The coherent original
admission observation carries the receipt independently from the optional source
result: an unreadable source is Unknown for fresh execution, while a valid exact
receipt still recovers its accepted ACK. Historical NULL source is not guessed.

Fresh callbacks read their origin snapshot and frozen publication bundle from
that original authority, so ordinary `version: None` selection continues with
the same original bundle after a later publication. The same compilation kernel
validates that bundle with `exact_source`; it never chooses today's catalog or
the first publication sharing an Agent id. The immutable input fingerprint stays
at its original command owner. Source retention grants neither Task tenure nor a
Run claim, and changes no direct, nonqueued SDK execution semantics.
