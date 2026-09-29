# ADR-0024: Exact-Claim Lease-Renewal Ownership

- Status: Accepted
- Amended: 2026-08-31 — exact-claim renewal is the only local and remote
  Run-lease authority; the redundant owner-wide transport is removed.
- Amends: ADR-0019 — schedules the `renew_lease` operation it introduced; kept
  as its own number for history, but it is a refinement, not an independent
  decision.
- Date: 2026-06-30
- Depends on: ADR-0011, ADR-0019, ADR-0065

## Context

A Run can execute or resolve for longer than its dispatch lease. Without
renewal, another Worker can reclaim it and duplicate an external effect.

The original decision assigned an owner-wide heartbeat to `DispatchService`.
That description no longer matches the local or remote execution path:
the service synchronously awaits a drive, process pools resolve a Session Worker
after claiming, and foreground child execution bypasses both daemons. An
owner-wide loop also retains unrelated claims after their activity has ended.

`DispatchWorker` remains clock-free in the structural sense: it stores no Clock
and reads no private SystemClock. The edge that starts a drive owns one
`Arc<dyn Clock>` and passes that source through the whole claim lifecycle.

## Decision

### D1: One exact-claim guard owns local renewal

`renew_claim_while_active(store, claim, lease_ms, clock)` creates one renewal
guard for one fenced claim. Every interval (`lease_ms / 3`) it invokes
`renew_lease(claim, lease_ms, clock.now_ms())`. The complete `RunClaim`
(`run_id`, owner, monotonic `lease_epoch`) is the same fencing identity used by
commit and settlement; an owner name is never sufficient because a process
identity may eventually be reused. Losing ownership stops the task; dropping
the guard cancels it.

Each renewal request is bounded to half the regular interval. A transient
transport/store error retries on the same guard with a short bounded delay,
without moving the next regular renewal away from its absolute cadence. If no
successful renewal proves ownership by two thirds of one lease, the guard
cancels the attempt before a peer may legitimately recover the durable lease.
Recovery may advance the mutation claim, but D5 still forbids the peer from
entering an external executor until predecessor quiescence is acknowledged.
This local proof deadline is conservative signalling only; the dispatch store
remains the sole lease-expiry authority.

The guard, not a daemon-wide in-flight registry, is the renewal authority. A
direct service or foreground drive creates it at the canonical Worker drive. A
process pool creates it immediately after claim so slow Session/Environment
resolution is covered, then transfers that same guard into the Worker drive.
The Worker must not create a second Tokio task at that handoff. Resolution
failure, retry terminalization, and other Pool-owned non-execution branches keep
the Pool guard until that exact operation returns and then drop it.

### D2: One edge Clock spans claim, renewal, verification, and settlement

The service, pool, or foreground entry supplies a single `Arc<dyn Clock>` for an
operation. The same source determines:

1. claim eligibility and lease deadline;
2. renewal timestamps;
3. claim-bound pre-effect ownership verification;
4. retry deadlines and coordinated settlement fencing.

`DispatchWorker` has no `ownership_clock` field or clock-setting builder. A
scalar `now_ms` snapshot is insufficient because renewal and verification occur
later. Tests supply a `ManualClock`; production edges supply `SystemClock`.

### D3: Remote Workers renew each exact claim

A database-independent remote Worker cannot transfer a process-local guard into
the Coordinator, but it has the epoch-bearing claim returned by the signed claim
endpoint. Its remote guard therefore invokes the same `renew_lease` contract for
that exact claim. The Coordinator derives owner, lease duration, and time from
authenticated Worker authority. No owner-wide renewal verb, compatibility route,
or parallel daemon is retained.

### D4: Epoch-bearing renewal is a coordinated wire cutover

The authenticated remote request carries `run_id + lease_epoch`; the Control
side derives owner, lease duration, and time from trusted Worker authority.
Missing epoch fails closed rather than falling back to owner-only renewal. A
release containing this amendment therefore drains old Workers and deploys
Control plus Worker binaries as one coordinated maintenance cutover; no legacy
renewal endpoint or epoch-default compatibility path is retained.

### D5: The Dispatch aggregate also owns physical-attempt quiescence

A live claim does not by itself prove that a superseded model, tool, or Sandbox
future has returned. Each Dispatch row therefore carries at most one exact
`(active_attempt_owner, active_attempt_epoch)` slot. `begin_attempt` admits only
the current claim into an empty slot; an exact retry is idempotent; a successor
claim receives `Blocked` while the predecessor slot remains. `finish_attempt`
may clear only the matching exact slot, even after its mutation lease advanced.
It grants no commit, checkpoint, or settlement authority.

The physical-slot transition kernel in `awaken-dispatch-contract` is the
single rule owner. Continuation handoff reuses ordinary admission and changes
only `Blocked` to `Replace`; it cannot bypass stale-claim or corrupt-slot
rejection. The backend-neutral adapter in `awaken-dispatch::dispatch` owns
the mapping to admission/settlement outcomes and corruption errors. Memory,
SQLite and PostgreSQL load one locked snapshot, call that adapter, and apply the
returned mutation within their existing lock or transaction. Their clocks and
storage operations remain backend responsibilities, not duplicate domain rules.
HTTP authentication precedes this same store path and adds no second reducer.
No new table, receipt store, endpoint or recovery authority is introduced. The
existing admission result preserves the reducer's three successful origins:
`Applied` for `Install`, `AlreadyApplied` for `Stutter`, and `Replaced` for
continuation handoff. Because an older Worker cannot decode `Replaced`, this
internal Control/Worker contract follows the coordinated maintenance cutover in
D4 rather than retaining a flattened legacy result.

The shared recovery conformance trace checks duplicate admission, lost-ACK
retry, and an old ACK arriving after successor admission against the same
backend contract. The production-kernel Kani harness checks the complete slot
and claim partitions, including equivalence of ordinary and continuation guards.
These checks validate admission and fencing; they do not establish that an
external effect stopped or that a whole-Session stop is attempt-local.

```text
claim A -> begin A -> external future A
lease A expires -> claim B
begin B -> Blocked
future A returns with no abandoned nested effect -> finish A
begin B -> Applied -> external future B
```

Explicit executor return can acknowledge quiescence only when no nested effect
was abandoned and process cleanup succeeded. The attempt-local
`RuntimeRunContext.physical_quiescence` latch propagates uncertainty from Native
tool/model execution and ACP cleanup failure; it can veto an ACK but
cannot clear the Dispatch slot. Inline children share this latch; a separately
admitted attempt receives a fresh one. No second durable authority is introduced.

The canonical ordinary-tool effect boundary also observes invoke, detached
start, poll and cancel after concurrency admission and the ownership recheck.
An entered Future dropped or unwound sets the same latch. A returned typed
`ToolError::Indeterminate` sets it before any awaited result processing, bypasses
model-visible completion and reaches the existing logical Indeterminate outcome.
Normal and recovered batches share one terminal-error projection. A definitive
ordinary error remains model-visible; replay capability never proves effect
locality or termination. The Hand wire retains its existing Indeterminate arm,
and both server and client preserve that classification instead of converting it
to an ordinary error string. The Session Hand cannot mask it with a subsequent
catalog-refresh failure. Independently claimed background attempts receive fresh
latches; their inline children share them. Unknown start/poll/cancel observations
terminate through the existing background Indeterminate candidate, not another
retry loop. These rules do not prove that a custom adapter correctly classifies
its external protocol, or supply a provider receipt for post-crash admission.

The MCP client maps effectful call/start/cancel failures through one adapter
rule into that same typed uncertainty. Only transport-owned `NotDispatched`
evidence (closed before send or rejected by the writer/admission fence) permits
an unavailable-before-dispatch result. A server/protocol error, HTTP failure,
generation revocation during a call, or malformed remote task coordinates is
not such evidence. The generation fence still owns only local call drain; it
does not acquire attempt or remote-stop authority. Read-only status/result
failures for an already-persisted task handle remain observation failures for
the existing bounded Host retry path, never permission to repeat creation.
The existing MCP wire result schema is unchanged; the Rust transport error enum
adds the explicit pre-dispatch case. Mapping decision tables, real HTTP replies
and generation-drain traces exercise this adapter boundary, not remote provider
termination, arbitrary custom transports, or whole-system crash refinement.

Returned durable-task coordinates are validated after creation through
`ToolTaskHandle::validate_created`, reusing the same shape validator as input
validation but mapping failure to `Indeterminate`. Runtime applies this before
its monotonic uncertainty veto; ordinary adapter-return errors cannot disguise
a possibly live task whose handle was malformed. MCP uses the same returned-
handle boundary. Prepared tool and Host background-task tests require retained
uncertainty, no wait/completion success and no repeated start; valid handles and
pre-dispatch input validation keep their existing behavior. This classification
does not manufacture a provider termination receipt or clear a Dispatch slot.

Direct and durable delegated child Runs share the existing owned task boundary,
so a durable Worker is not recursively polled on its parent's stack. Dropping
the owner requests abort of that exact child task, never detachment; abort still
does not prove physical termination. Dispatch fencing and the inline-child
uncertainty propagation remain responsible for the affected execution scope.

Cancellation that exceeds the lease request budget keeps observing the same
owned executor Future with its Thread lock held. The budget emits a diagnostic;
it does not preempt the executor's TERM-to-KILL cleanup or discard a late ACK.
After observed return, unavailable or timed-out `finish_attempt` requests retry
the same claim without rerunning the executor. This also covers a committed ACK
whose response was lost: the existing empty-slot transition is idempotent, while
a different successor slot remains fenced. Permanent rejection still fails.
Process death before a successful ACK remains an explicit recovery boundary.
An acknowledged execution epoch is now retired in the same Dispatch row and
the same transaction that clears its physical slot. Admission and durable
handoff reject an epoch at or below that watermark, even if its mutation lease
is still current. This closes the `begin(A) -> finish(A) -> delayed begin(A)`
ABA window: a repeated old ACK cannot clear a resurrected execution using the
same identity. A new claim may execute only with a higher epoch; no new lease,
receipt store or recovery authority is introduced. Migration conservatively
retires historical empty-slot epochs because they cannot distinguish an unused
claim from a lost ACK response. Historical occupied slots remain unchanged.
ScheduledAction stages are deliberately driven inside the original physical
attempt, sharing its Thread lock, cancellation carrier and uncertainty latch.
Only the whole drive returns an ACK; there is no release/reacquire between
stages under the same claim. An uncertain stage cannot launch the next stage.
Retirement is not evidence of process or remote termination. This prerequisite
does not wire the provider observation below into Native recovery.
The existing registered Worker runtime protocol floor advances to v4 for this
whole-claim ACK boundary and typed Dispatch renewal outcomes. A v3 incarnation
must not be admitted merely because an old queued request pins v3: it would ACK
a scheduled stage and then try to reuse the now-retired claim. Registration,
placement and physical begin/handoff reuse the same current-version check.
The Worker builder also uses that predicate rather than a separate `contains(1)`
check; an explicit current-only manifest is valid, while omitted/ANY or v3-only
manifests fail before registration. No compatibility fallback is introduced.
The exact authenticated predecessor ACK remains available independently of that
execution floor, so upgrade cannot itself prevent observed-stop retirement.
This retains ADR-0066's coordinated Control cutover requirement; it does not make
mixed-version Control replicas safe, or replace independent stop evidence.
The Kubernetes adapter now offers a narrower independent local observation:
the read-only `PodProcessObserver::bind_pod_process` verifies the authenticated
Pod UID and freezes the complete ordinary-container set before effects,
and `observe_pod_process_termination` requires the same Pod UID, node, container
IDs and start times plus every exact runtime exit. It rejects restartable,
privileged, init and ephemeral-container profiles. A live projector vetoes the
whole-domain observation even after the main container is killed. Pod absence,
controller-synthesized Failed, synthetic ContainerStatusUnknown, missing legacy
bindings and name reuse never establish termination. The subject-bound result
has private fields and cannot be deserialized as an incoming receipt.

Worker registration now retains every incarnation in the same Registry table;
`current` and placement observations select only the highest generation. The
shared transition rejects historical physical incarnations, even after another
successor expires: an A/B/A bootstrap retry cannot mint generation three. Exact
current-incarnation retries return the original record without extending its
lease. Memory holds the same generation history; SQLite uses its existing
immediate transaction and PostgreSQL its existing per-slot transaction lock.
History lookup, admission and insertion share that transaction. There is no
current-row cache or second blacklist to reconcile.

The converged migration appends one portable table migration after either
published history, preserving all fields and immutable migration receipts.
Old overwrite-only binaries cannot use `ON CONFLICT(worker_id)` afterward, so
Control must perform a coordinated migration/cutover. A legacy row above
generation one has already lost predecessor identities: keep current retries
and heartbeat/drain semantics, but reject any new incarnation on that slot with
`IncompleteIncarnationHistory`. Move that Worker to a new configured Worker id;
never fill the unknown past with invented identities. Retained histories are
not garbage-collected while bootstrap replay remains possible. They are
registration authority, not process-exit or remote-stop receipts.

H1-H10 Registry test comments define the historical replay, state/expiry,
projection, concurrent replacement and reopen matrix; M1-M6 execute both
published migration streams in SQLite and PostgreSQL; T1-T5 follow the actual
signed client/HTTP handler/Registry chain. The production Kani predicate covers
seen identity and history completeness. `RemoteWorkerProtocol` separates those
physical identities from allocated generations, with an additional bounded
three-generation registration configuration, three semantic mutants and two
positive witnesses. These checks do not prove arbitrary SQL histories, trusted
bootstrap identity rotation, physical termination or the recovery join below.

`PodProcessStopper` adds the independent provider command, not a second observer
or cleanup owner. Trusted deployment must install
`awaken.dev/worker-process-evidence` before binding; the opaque binding freezes
that retention fact. Missing legacy retention stays false and serializes exactly
as before. Adding a finalizer later cannot upgrade an already admitted binding.
The command reads the exact private non-restarting process set, requires the held
retention marker, and submits UID/resourceVersion-conditional Pod deletion.
Neither acceptance nor an already-deleting Pod proves exit; retries observe the
same bound domain, and only the original exit validator may return its receipt.
The stop command requires an observed deletion intent even when all containers
exited before recovery began. It first closes that exact retained Pod with the
same conditional DELETE and returns Unknown; a subsequent read must observe the
deletion intent together with complete matching exits. The read-only observer
still reports only local process facts, not lifecycle closure. In particular,
`restartPolicy: Never` is not a substitute for the stop command: Kubernetes
documents that kubelet/runtime state loss can cause unexpected reruns under
[restart policies](https://v1-35.docs.kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/#individual-container-restart-policy-and-rules).
Control-plane deletion intent and runtime exit reports remain trusted
infrastructure facts, not a proof of node isolation or remote-operation stop.
CAS conflicts and response loss retry without retargeting a replacement. The
adapter deletes no Session/PVC participant. The existing Worker Registry history
now stores one immutable `WorkerProcessExit` for the exact incarnation, bound to
the frozen provider and execution references. The application persists and reads
back that receipt before the adapter may remove only
`awaken.dev/worker-process-evidence` through the shared Pod finalizer CAS loop.
After response loss or Worker replacement, retries read the historical receipt
and never issue a second stop command. Supported non-restarting Worker Pod
deployments must install the finalizer before admission; the bundled k3d crash
fixture does so and grants Coordinator only Pod get/update/delete. The ordinary
restartable StatefulSet remains outside this binding profile.
Its whole-Worker scope requires trusted recovery composition, not per-Run
cancellation. Remote-participant proof remains unavailable. ADR-0065 owns the Coordinator's
local-stop and complete-assertion consumption: only the separately verified full
assertion can reach exact retirement. The Kubernetes adapter retains Unknown for
that assertion; local exit alone never retires an attempt.

This is a provider observation, not a Native recovery command. Dispatch now
freezes an optional `AttemptExecutionBinding` with its physical slot, in the
same admission transaction. It contains the exact registered Worker identity,
provider identity and opaque physical reference. The shared admission validator
checks the structured Worker assignment, not just the encoded lease-owner
string. Same-claim retries must preserve even an absent binding; late backfill
cannot manufacture pre-effect provenance. Continuation handoff replaces the
slot and binding together; exact finish clears both and retires the epoch.
Memory, SQLite and PostgreSQL use this same validator. The existing Dispatch
port supplies a narrow `load_physical_attempt` read, not another current-state
store. Converged schema V5 leaves historical bindings absent and prevents an
orphan binding without an occupied owner/epoch pair.

The value is a coordinate, not a receipt or proof of provider provenance. Trusted
in-process composition may supply it to `DispatchWorker` before admission;
the HTTP client rejects supplied coordinates instead of silently discarding them
or accepting Worker JSON as provider authority. The two physical-admission HTTP
verbs now share one authenticated handler sequence. An optional Coordinator-owned
`WorkerExecutionBindingSource` supplies the coordinate for the verified registered
incarnation; the request carries no binding or stop evidence and rejects unknown
fields. Configured lookup failure/timeout cannot fall back to None. Lookup holds
no Dispatch lock, is bounded to one server lease budget, and is followed by a
fresh Registry check and a fresh dispatch clock sample before the ordinary
transaction. Same-claim response-loss retries still use the existing immutable
binding rule. The exact predecessor ACK does not depend on this lookup service.
The source is threaded through the existing Coordinator composition, without a
new cache, state store or recovery queue. With `container-k8s` and an explicit
`worker_kubernetes` policy, CLI startup co-selects TokenReview authentication and
this source using one authenticated Kubernetes client. Private assembly fields
prevent an injected or signed authenticator from being paired with Pod facts.
Read-only observation no longer constructs a Sandbox runtime or provisions its
NetworkPolicy. Default non-Kubernetes transports remain unbound. The current
restartable Worker Pod profile is not admitted by the Kubernetes adapter. The signed
HTTP R1-R13 decision table covers source failures, identity/epoch changes during
lookup, no-lock renewal progress and strict request parsing. Exact-claim preflight
also rejects invented/stale Runs before provider lookup; it does not replace the
final atomic admission check. Enabling or disabling the source cannot rewrite an
existing slot's binding; configuration rollout cannot backfill historical proof.
These are executable
transport tests, not provider-provenance or full crash-recovery proofs.
Therefore the new persisted coordinate
alone cannot authorize crash recovery or clear a slot. B1-B22 test comments own
the binding lifecycle, transaction-race, reopen, corruption and transport
decision table, including the Native tool-entry boundary and historical upgrade.
The binding retry Kani harness proves only its finite Boolean
immutability predicate, not database linearizability or physical termination.
No remote model/MCP stop capability or historical evidence repair is implied.
The authenticated API, kubelet/runtime exit-report honesty, private process
confinement and non-restart semantics remain the trusted infrastructure boundary.
The provider-only three-generation/two-container model deliberately does not
duplicate Dispatch. A real k3d test uses exact-container CRI hard stop, observes
the live projector veto, drains the full Pod through the independent stopper
behind a fixture-installed production retention finalizer,
checks delayed exec rejection and then verifies that removed history is unknown.
It also recreates the same Pod name through the canonical provider: the old UID
cannot bind or delete the live successor, and the successor's later exit proves
only its fresh binding. The stale DELETE rule checks Kubernetes UID preconditions;
the provider stop command separately rejects that old binding without a write.
Sixteen model mutants and six positive observation/stop witnesses cover the finite
provider boundary; neither those
checks nor the live test establish whole-attempt recovery or evidence retention
after API history is removed without the durable Registry receipt.
The exit-before-stop regression crosses Completed/Error/OOMKilled and every
existing stop-response-loss, CAS, replacement and retention-loss fault. Its KPT10
extension in the existing live test hard-stops every container before invoking
the stopper and requires the first accepted stop to remain Unknown. On 2026-09-09
the updated 226-test container library suite and strict all-target Clippy passed;
the updated live case was blocked before cluster creation by the canonical disk
capacity check (about 4.4 GiB available, 10 GiB required). Earlier live results do
not qualify this new fault cut.
Cooperative cancellation may cause a verified return; dropping or aborting a
nested Future does not prove an opaque remote request stopped. Task abort, process crash, Lease
expiry, and heartbeat expiry therefore leave the slot occupied. Recovery waits
for an explicit old-worker ACK, or for a future authority command backed by an
authoritative provider terminal receipt. Process/Pod termination can prove only
process-bound tool/Sandbox work. This chooses safety over availability when
external work cannot be proven stopped.

Consequently, interruption of a Native opaque tool or model can admit
cancellation intent yet remain unable to resume the same Thread after abandoning
that effect. The ordinary-tool and model ports do not supply a terminal-receipt
recovery command. Native inference returns data rather than invoking proposed
Awaken tools, but discarding that data does not prove provider computation or
billing stopped. The model Future therefore arms the same attempt-local veto on
first poll. A Rust return only disarms that observer; Runtime separately latches
returned errors with unknown remote execution before any awaited journal write.
Cancellation, task abort and
watchdog expiry retain the physical slot. A watchdog ends the logical Run as
Indeterminate without inner retry, continuation, failover or outer failure
tolerance. Cooperative cancellation under the current execution claim retains
its logical Cancelled outcome. Durable cancellation that replaces the claim while
the predecessor remains unconfirmed instead commits Indeterminate through the
existing Worker pre-execution terminal path; it cannot enter a second physical
cancel attempt. Neither logical outcome is physical-termination evidence, and
neither clears the predecessor's occupied slot. Managed's shared root/child
lifecycle classifier reads the same Run's model evidence at the cancellation
commit and exposes Unknown as the
existing execution-unknown error. It never clears a Dispatch slot or rewrites
Run state. The browser retains that current error across same-activity idle;
a new committed working boundary moves it to history. ACP's fixed business
outcome likewise does not imply successful process cleanup.

One continuation case is stronger than terminal receipt: an A2A executor may
recover a versioned `Accepted` remote operation whose complete `TaskReference`
matches the frozen endpoint. Its recovered `execute` path performs only
`tasks/get`; it cannot repeat the initiating `message:send`. The leaf executor
may therefore request `handoff_attempt`, which atomically replaces the
predecessor slot with the exact current claim in the same Dispatch row.

The same Run-scoped StateCell first commits `Prepared` with an operation id,
endpoint, context, optional predecessor task, stable message id and input digest;
only the validated send response replaces it with `Accepted` and the exact task.
A crash before send and a crash after remote acceptance but before its local
commit are deliberately indistinguishable: `Prepared` has no addressable task,
cannot hand off, cannot resend, and cannot use its predecessor task as stop
evidence. The coordinated binary and durable-state cutover admits only the
versioned operation schema; flat, unknown-version, missing, malformed,
endpoint-mismatched or `Prepared` state fails closed and retains the physical
obligation. This is neither lease-expiry cleanup nor a generic process-crash
bypass.

```text
claim A -> begin A -> commit Prepared(A) -> message:send
        -> commit Accepted(A, exact TaskReference)
process A crashes while polling -> lease expires -> claim B
executor B proves exact committed Accepted continuation
handoff B replaces slot A -> tasks/get (never message:send) -> finish B
```

If cancellation is already visible after admission but before B constructs its
executor Future, only `Applied` proves that B installed an empty slot and may ACK
its own known no-effect attempt. `AlreadyApplied` may be a lost-response retry,
and `Replaced` inherits A's remote responsibility; both retain the slot and take
the existing Indeterminate path. Local "not started" is not evidence that an
earlier caller or remote operation never started.

The A2A executor also uses the Runtime's same attempt-local return observer.
Send/resume failures, a failed Accepted commit before a remote boundary,
reattachment failures, exhausted polling, identity mismatch and an abandoned
Future veto physical ACK. Even failed transport resolution must retain that veto
when a remote task already exists; fresh pre-dispatch resolution does not invent
an external participant. Diagnostics remain execution/commit errors and cannot
be used as stop evidence. An unknown send no longer commits an ordinary
`a2a_error` terminal result. A validated boundary precedes its final commit, so
failure of that final commit does not undo observed remote completion.

Every poll reuses the existing task-reference identity admission, not only the
initial reattachment. Explicit and cooperative cancellation share one owned
cancel-and-observe path. A successful HTTP cancel response proves delivery only;
an exact task/context terminal GET is required, including after a lost cancel
response. Waiting states are execution boundaries, not terminal cancellation
receipts. A failed/nonterminal observation retains the slot and the existing
reference for recovery; it does not retry creation. The former Future-drop
background cancellation is removed: drop propagates uncertainty, while the
existing claimed cancellation/recovery owner remains responsible for progress.
For an authenticated publication, dispatch compiles only the selected A2A
candidate's secret-free `RemoteAgentAuthorization` binding at the current claim
epoch. The same claim-bound materializer rechecks current ownership, holder,
revision, usage, target, and source status before `tasks/get` or `tasks/cancel`.
Missing or stale authority prevents the wire call; there is no anonymous fallback,
and the failed authorization is not physical-stop evidence. Anonymous and non-A2A
cancellation claims remain credential-free.
No Session-wide stop, new receipt store, task schema or wire route is introduced.
Task-state/identity/delivery decision tables, observer-drop and commit-failure
tests cover this adapter; the existing Kani identity/route proofs remain narrow.
Remote terminal-state honesty, transport completion and atomic external effects
remain assumptions, not a generic all-participant crash-stop proof.

Native model invocation evidence now uses the existing Run-scoped Thread state
log (`ActiveModelOperation`), not a new receipt database. Before every
`LlmExecutor::infer_streaming` call, including Runtime retries and model-pool
failover, the StepLedger commits a non-wrapping invocation ordinal and the
selected model binding. An acknowledged entry is followed by another live
ownership check. Local return records `Returned`; the outer watchdog records
`TimedOut`; cancellation or process death during execution leaves `Entered`.
Entry is conservative intent, not proof that the network send occurred.
The complete ordinal history remains in the same append-only state log.

After an acknowledged entry, Runtime scopes the existing Thread/Run/ordinal/
model-binding tuple to the actual provider invocation. Provider adapters derive
a versioned bounded operation id from that tuple; direct unjournaled calls have
no such identity and explicitly mask an outer scope. The brokered grant adapter
requires exact Run and model-binding context and uses it for its existing grant
idempotency key. Its former per-executor counter restarted on every lazy pinned
materialization, incorrectly aliasing different model calls to one grant; that
second allocator is removed. Streaming and nonstreaming grant lifecycles share
one implementation. Reconstructing one recorded invocation preserves its key;
a subsequent journal ordinal gets a different key. This supplies addressable
correlation, not provider deduplication, permission to resend, or termination
evidence. No new receipt database, request payload copy or public model protocol
field is introduced. Grant-response loss and opaque remote completion still
require their respective existing recovery policies and independent evidence.

Evidence write rejection or lost ACK terminates the drive as a commit error,
outside provider retry, model failover and consecutive-inference-failure
tolerance. Read/provider waits remain cancellable, but cancellation retains
ownership of an in-flight evidence commit until its result before terminalizing.
Cancellation and evidence-write failure retain the interrupted-stream checkpoint;
neither counts as successful recovery or consumes a retry that never entered.
The existing model completion audit remains the only logical request/usage
observation; these small state records contain no request, response or credential
material and do not create another billing path.

This is an evidence-boundary repair, not a physical-slot recovery bypass.
`Returned` means the local executor future returned, not that an arbitrary
provider stopped computing. The same record stores independent execution
evidence: `Unknown`, `NotDispatched`, or `Terminated`. Missing fields, including
legacy `Returned`, decode as `Unknown`. Before checkpoint salvage and every new
invocation, Runtime checks the prior Run-scoped evidence; Unknown retains the
physical veto and ends logically as Indeterminate without retry or failover.
Positive model evidence is still subordinate to this attempt's shared physical
veto: an inline child can remain uncertain even when its outer tool returned
success or a model-visible business error. Runtime checks that same veto before
model preparation/admission, after their successful awaits, during journal
reentry, and after the post-intent ownership await immediately before provider
entry. A quota Pause cannot hide the veto as an ordinary resumable budget wait.
Native decision-table regressions reproduce the formerly admitted second model
call, preserve ordinary success/error continuation, and exercise veto arrival
before Run entry, before/after durable model intent, during preparation and
during Admit/Pause observation. These checks reuse the existing monotonic latch
and model journal; they do not persist a new all-participant stop receipt or
authorize independent post-crash Dispatch retirement.
Error taxonomy still owns provider health and routing, never physical safety.
An existing stream checkpoint also requires predecessor evidence: no journal
is not equivalent to a fresh call when an interrupted partial exists. Such
historical checkpoints remain retained and cannot authorize model continuation
or tool salvage. Fresh inference without a checkpoint still starts normally.

One recovery observation now precedes initial execution and validated resume,
including permission approval, child delegation and model-free ToolBatch handoff.
The entry passes the validated checkpoint into Native drive; drive does not read
it again. Only a successful absent read or a matching Run/Thread checkpoint,
together with permitting model evidence, may reach those effect boundaries.
Storage/fence errors and foreign identity end Indeterminate; cancellation during
the read returns Cancelled through the existing cancellable read boundary. These
failures retain the checkpoint and physical veto rather than fabricate absence.
The same read has one 30-second observation budget, even when no cancellation
token is supplied. Expiry maps to the existing unavailable-read decision;
it neither cancels the Run token nor times out a provider or durable evidence
write. Successful slow reads before the deadline still admit normal execution.
Cancel/stop preserve their requested logical outcome after read expiry while
retaining the physical veto. No new retry loop or recovery authority is added.
Malformed owned model evidence retains its Commit error classification and veto.
Cancel, stop and coordinated tool interruption restore that same veto before
returning their logical outcome, including terminal interruption replay.
Recovery rejection seals unfinished tool/child state through the existing final
ThreadCommit; resume supplies its explicit committed reader to that same sealing
implementation instead of depending on the optional context reader.

Durable cancellation keeps the backend cancel callback AND that terminal writer
inside one `run_physical_attempt` operation. Its original attempt-local latch
therefore receives the evidence veto before `finish_observed_attempt` can clear
the exact slot. Previously only the backend callback was inside this boundary;
Native's no-op could be ACKed before its later terminal writer discovered
Unknown evidence. No Native-only duplicate preflight is needed. The existing
Memory/SQLite Worker regressions require Unknown to retain the slot and withhold
settlement, while Terminated still permits ordinary cancellation and settlement.

The existing CheckpointRecovery model covers five entry classes and four prior
evidence classes within MaxChunks=3, including control return without evidence,
resume/handoff before observation, retained unknown checkpoints and read
settlement under fair local timer/scheduler progress even for a pending store.
Fifteen semantic mutations and fourteen reachability witnesses guard
the model. Runtime tests separately cover actual permission/tool/child calls and
control replay, deadline expiry without cancellation, and successful slow reads.
The model's explicit control ACK transition rejects acknowledgement before the
control observation; a semantic mutant reproduces the early-callback ACK and a
positive witness ensures confirmed control can still be acknowledged.
This is bounded abstract-model evidence, not Rust refinement or
provider termination proof. It does not make blocking synchronous storage
preemptible, prove an unjournaled historical Run stopped, or add independent
execution-domain receipts. Invalid resume commands are rejected before recovery
observation and are not claimed as predecessor-stop proof.

Trusted adapters annotate pre-I/O validation/materialization errors as
NotDispatched, and errors after validated protocol completion as Terminated.
Transport EOF, local deadlines, unparsed bodies and HTTP failure statuses remain
Unknown. Both production GenAI and Responses constructors use one HTTP-client
policy that disables retries and redirects. Injected custom clients/executors
must uphold the same port contract; this does not prove arbitrary SDK behavior.
The SDK's captured response is the sole full streaming accumulator in the GenAI
adapter; the adapter retains only tool emission offsets, not a second copy of
text, reasoning and tool arguments.

Provider terminal/task receipt recovery and authority-backed Native crash
handoff remain unresolved; no existing physical slot is cleared based only on
these records. A historical Run with no model journal at all is not retrospectively
proved stopped. This repair rejects known uncertain invocation history; it is
not a blanket historical migration or an independent execution-domain provider.

Broad queue selection first considers eligible rows without physical occupancy,
then occupied recovery candidates. Within either cohort the existing recovery,
wake and fresh-work priorities remain. Relinquishing a recovery mutation lease
preserves the predecessor slot, but must not let its expired-running priority
starve fresh work in another Thread. Exact claims and A2A recovery remain
available, and same-Thread peers remain excluded until the original Run settles.
Terminal-only maintenance also requires an empty physical slot for expired
running rows, just as for pending/awaiting rows. Otherwise it acquires a lease
that cannot settle and delays the genuine ACK's effect until another expiry.
This restriction does not affect ordinary/A2A recovery claims.
The shared Dispatch conformance matrix crosses occupancy, cancellation and all
three selection ports on Memory, SQLite and PostgreSQL. This is bounded progress
for independent queued work; it does not prove fairness for an occupied recovery
candidate under an infinite stream of other work.

The slot is not another lease or state machine. Claim, attempt admission,
settlement, retry exhaustion, and dead-lettering mutate the same Dispatch row in
one backend transaction. `ThreadCommit` remains the sole execution-result truth.

The canonical background handle is `DispatchReconciler`: it reconciles already
committed terminals, relays staged input and expires aged manual quarantines.
The pool and coordinator-only Host reuse its one reconciliation loop. It is not
a new aggregate, recovery center or source of physical-stop evidence; the former
`DispatchMaintenance` name has no parallel compatibility implementation.

The Dispatch adapter's immutable `owns_local_maintenance` boundary determines
whether this process may run that loop. Memory/SQLite/PostgreSQL own queue
maintenance; `HttpDispatchQueue` does not, and `AnyDispatchStore` forwards the
same fact. Database-less Workers keep claim, renewal and retry-exhaustion drive
but never periodically invoke Coordinator-only relay, purge or global scans.
Explicit unsupported calls still reject; they are not successful no-ops.
Service gates its terminal deadline as well as its maintenance body, preventing
a skipped remote scan from leaving a permanently ready timer. Pool and standalone
Reconciler use the same early ownership check, without another deployment flag.
The adjacent 12-row scheduler test crosses ownership, entry and timer selection;
the HTTP test checks that type erasure preserves refusal. The bounded
`DispatchMaintenanceScheduling` model covers lost/duplicate hints, periodic
progress and cancellation with three semantic mutants and reachability witnesses.
Its atomic scheduling steps assume no storage operation is in flight: it does
not prove driver completion, claim semantics or physical process termination. The
same model additionally checks raw observations from the actual three public
entrypoints: a pending maintenance port keeps explicit shutdown pending until
that port returns, including an error. The original ownership/timer matrix and
these cancellation cuts share one fixture and the existing Rust-to-TLA pipeline.
This finite correspondence does not turn returning fixture I/O into a guarantee
that a production database, callback or provider will eventually return.

Standalone reconciliation directly consumes the configured `WakeSignal`.
Within a Pool, only the existing wake coordinator consumes that external port
and relays to a `LocalWakeSignal` for reconciliation; no extra listener or
notification registry is introduced. A hint schedules a read of durable truth,
not a claim, completion receipt or authorization. Duplicate hints coalesce and
lost hints remain covered by the queue poll. The terminal-reconciliation timer
is independently selectable, so a shorter configured interval is not silently
delayed by the ordinary poll. `None` still disables only terminal observation.

Service, Pool and standalone Reconciler shutdown cancel their existing local
token and join their existing tasks. They do not publish a network hint merely
to unblock a cancellation-aware wait. All command/reconciliation hint publication
uses `publish_dispatch_hint` with one 100ms best-effort budget. Notification
failure cannot change a completed durable admission. In-flight storage and
observation work is still joined; only the non-authoritative notification may
be abandoned. The PG listener owns cancellation and join handles; its final
owner requests cancellation, while explicit shutdown joins the listener.
Service drains at most 64 attempts per scheduling batch without collecting an
unbounded result vector. Full batches yield and revisit cancellation and terminal
coordination, not the ordinary idle poll. A single in-flight operation still has
its original authority and completion semantics, not a fabricated stop timeout.

Host and Worker share `TerminalReconciliationScan`, a process-local cursor over
the existing Dispatch primary key. `scan_dispatch_ids` reads at most the page
budget without decoding execution requests; `dispatch_summary` reads one exact
row. The scheduling budget counts inspected IDs, including nonterminal, missing
and unreadable rows, capped at 256 per pass. The shared production page iterator
also enforces this cap when a custom adapter returns too many IDs; it does not
bound that adapter's allocation or query cost. Per-row failures are reported and
retain the original Dispatch, but do not prevent visiting later IDs. Advance
precedes per-row I/O; short/empty pages wrap, and restart forgets only scheduling
position. A concurrent scan yields instead of adding a waiter. No durable cursor,
mirror index, recovery queue or event-feed projection is introduced. The old
administrative `list_dispatches` shares summary decoding but is not a recovery
scan fallback. Pending, Awaiting and Leased candidates use the same filter;
committed truth and exact claim/slot guards are rechecked before settlement.
Custom Dispatch adapters must implement both bounded/exact read ports; their
defaults explicitly reject unsupported capability, never silently scan all rows.
Host/Worker sample their supplied clock after asynchronous projection reads,
immediately before terminal claim. PostgreSQL exact claims of every mode sample
database time after the Thread lock; retry-exhaustion discovery also uses database
time. Caller clock skew cannot expire a live PostgreSQL claim or mint an already
expired replacement. Memory/SQLite retain their existing caller-clock contract.

Virtual-time and real-PG regressions cover publication failure, listener lifetime,
bounded batches, page boundaries, failed prefixes, concurrent scans and restart.
The minimal `TerminalReconciliationScan` model checks budget and conditional
progress for a finite stable ordered set under fair scheduling, returning reads
and bounded restarts. Its negative cursor mutation and positive wrap witness do
not duplicate Dispatch transitions or certify SQL/OS/provider behavior. Existing
Dispatch kernel and SessionRunProtocol proof scopes remain unchanged.

Physical exclusion is Thread-scoped even though the exact attempt belongs to a
Run's Dispatch row. Cancellation may return a row to Pending while its old
physical owner remains. Therefore Running-state uniqueness cannot enforce
physical exclusion. Begin and continuation handoff require a same-transaction
peer-occupancy snapshot regardless of status, in the canonical physical
transition kernel. Memory holds its aggregate mutex; SQLite holds an Immediate
transaction; PostgreSQL takes the existing Thread advisory lock before the row
lock, matching claim lock order. Converged migration V0003 adds one partial
unique index on Thread for nonempty physical owners. Existing paired-column
checks remain; historical migration streams are unchanged. Conflicting old
rows fail migration without discarding either owner's evidence.

Settlement requires claim state, epoch and physical-slot shape together in
`DispatchTransition::settle`; adapters no longer repeat a separate settlement
policy. Pre-settlement effect guards remain because those effects happen before
the final database transaction. Empty permits settlement; occupied or corrupt
does not. Old exact completion can clear only its original row/owner/epoch.
The conformance cancellation/peer matrix, direct PostgreSQL constraint race,
schema upgrade conflict tests and production-kernel Kani harnesses cover these
boundaries. They do not certify external provider termination.

Direct, non-durable ingress uses Runtime's one process-local Thread gate and
holds it across the complete executor Future. An unconfirmed direct return keeps
only a blocking marker in that same registry: queued and later acquisitions fail
before entering an executor, while unrelated Threads remain available. Normal
completion still removes the dead weak entry. This marker is process-local;
direct ingress does not acquire cross-process recovery guarantees.
Durable `RunService::start` and
`resume` fail closed; their only legal path is durable submission and claim.

SessionRunProtocol now represents external invocations as Run/Worker/Epoch
tuples independently of its Run-keyed slots. Only a separate observed external
return permits slot release; lease handoff does not remove that tuple. This can
represent two physical invocations of the same logical Run, unlike a Run-keyed
function alone. Receipt authenticity remains an assumption, not an implemented
Native provider-recovery API. The mutation gate rejects an unconditional finish
and requires an external-execution reachability witness.
`SessionRunStopEvidence` extends that same composition without copying any
Dispatch, Session or Work transition. Its three participant classes (local
process, model, tool) stop and deliver exact Run/Worker/Epoch receipts separately.
Physical exit precedes receipt delivery; an open local domain can still receive
a delayed launch. Only irreversible closure plus every exact participant receipt
permits delivery of a complete assertion. Delivery and its application to the
original aggregate return and exact `FinishExecution` are separate actions, so
response loss and successor admission between old replies remain observable.
The consumer must match that assertion's exact subject, not merely find any
complete assertion. Seven safety mutants, one fairness mutant and six positive
witnesses qualify these boundaries. A committed
Ended fact, lease change, local-only exit or an older attempt's complete receipts
cannot discharge another outstanding participant. Duplicate deliveries stutter;
delivery loss may persist indefinitely. The safety slice retains two Workers
and three epochs for one Run; existing configurations retain the separate
multi-Run/same-Thread exclusion coverage. Conditional liveness has a separate
one-epoch slice: once all work stopped in a closed domain, fair receipt delivery
and exact retirement suffice. It never assumes an opaque provider will stop.

This is bounded design evidence, not universal Rust refinement. Complete
pre-effect enrollment, adapter scope honesty and authenticated provider truth
remain trusted boundaries. Native governed-domain implementation is specified
in D6 below; opaque child/tool work and missing legacy enrollment remain Unknown.
A terminal business phase can contain Indeterminate and never proves closure.
No receipt database or second Dispatch authority is introduced. The per-model-operation ordinal, sequential
retries inside an attempt and opaque SDK retries remain outside this abstraction.
The Native watchdog regression
separately enables retry, failover and outer tolerance at the actual Runtime
boundary and checks that abandoned inference cannot enter any of them.

The Managed adapter projects committed Indeterminate through its existing
`session.error` / `unknown_error` / terminal retry status, before terminal idle.
Root and child histories use one Run-state classifier; repeated and cold reads
keep the same deterministic error identity. This reports a logical outcome,
not physical quiescence, and adds no Awaken recovery field to the standard wire.

### D6: Controlled execution closure and unknown provider results

The physical slot excludes concurrent governed execution. Provider computation
and billing may remain unobserved after a response-only consumer closes; that
fact never becomes a successful model result or a provider termination receipt.
`ModelOperation.effect_scope` is supplied by the exact concrete adapter and
frozen in the entry commit. Legacy/custom/hosted-action calls remain opaque.
Only the qualified DeepSeek mappings without server tools are response-only.
Unknown invocations still cannot be retried or consumed as complete responses.

Static ownership remains:

| Owner | Authority |
| --- | --- |
| ThreadCommit | Model operation scope/evidence, ToolBatch execution and output-materialization domains, pre-effect Native coverage |
| AttemptQuiescence | One local entry barrier and TaskTracker; result-only uncertainty vetoes continuation independently of effect uncertainty |
| Dispatch row | Exact physical owner/epoch, closing phase and retired watermark |
| Worker Registry / Sandbox provider | Independent process identity, retained exit evidence and process containment |
| Session application | Activity settlement, terminal intent and resource cleanup |

`JournalledV1` covers ordinary model/tool boundaries only. Untracked hooks,
actions, guards, terminal observers, effectful permission gates and unknown
content/admission callbacks exclude that profile. Domain metadata is committed
before executor or output-spill entry. ToolBatch retains the history of every
batch and cannot upgrade an entered scope through a later result. A tool's
replay policy remains independent of containment. Worker-local data transforms
need no independent process stop; a retained restricted Kubernetes Sandbox
requires the exact ordinary-container and completed-init process set to stop.
Opaque effects retain Unknown even when an ordinary output exists.

The same Dispatch row records irreversible local entry closure. A signed current
protocol Worker may report it only after its entry barrier closes and all local
participants drain. Independently observed Worker exit can establish the same
fact after a hard crash. The closing phase prevents both continuation handoff
and late ordinary ACK from freeing/replacing the old scope during provider IO.
Coordinator-only `finish_attempt_closure` reuses the same exact-slot finish
kernel after complete trusted evidence. It grants no Thread write or replay.

```text
unknown result -> committed Indeterminate (or cancellation)
  -> owned consumer closes; local entries drain, or exact Worker exit is proven
  -> close original physical slot atomically
  -> stop/observe frozen controlled domains; never reissue the initiating effect
  -> missing/opaque evidence: retain closing slot and existing retry backoff
  -> complete evidence: exact closure ACK -> retired watermark
  -> ordinary Dispatch/Session settlement -> idle -> standard archive
```

Kubernetes containment reuses existing process observation and continuation
retention. Completed nonrestartable init containers are frozen; live/restarted
init, extra/privileged participants, UID/name reuse, synthetic node-loss status
and missing history cannot produce a receipt. The process observer never removes
a Session continuation/PVC harvest finalizer; its Resource owner retains cleanup.
A closing Worker entry does not require killing that healthy shared Worker.

Internal Worker protocol v5 carries this return/closure distinction. Control and
Worker deploy together; older execution is not admitted through an unversioned
fallback. Standard Managed Sessions, events, AI SDK and Files do not change.
Historical missing enrollment remains unsupported. Tests own the cause/effect
rules and exercise exact close, response loss, cold reopen, handoff/ACK races,
result-only uncertainty and provider identity/scope mismatches. External provider
honesty, CNI enforcement and unrestricted/custom adapter behavior remain TCB;
no claim of universal remote termination or report-semantic correctness follows.

## Dynamic behavior

```text
local edge claims with Clock C
  -> one exact guard starts with C
  -> optional Pool resolution retains that guard
  -> Pool transfers the guard and C to DispatchWorker
  -> exact epoch renewal succeeds on an absolute cadence
     or transient failure retries inside the safety window
     or lost/unprovable ownership cancels the attempt
  -> Worker verifies ownership with C before an external effect
  -> Worker settles with C
  -> guard drops and renewal stops

remote Worker claims with run_id + lease_epoch
  -> one remote exact-claim guard starts
  -> signed renewal carries run_id + lease_epoch
  -> Coordinator derives owner/time and renews only that claim

every local or remote durable executor call
  -> begin exact physical slot
  -> or, only for a proved committed continuation, atomically hand off that slot
  -> model/tool/Sandbox future returns
  -> if nested effects and cleanup are known complete, finish exact physical slot
     otherwise retain it and report physical-quiescence uncertainty
  -> then settle/relinquish may remove or release the row
```

If renewal reports `CancellationRequested`, the guard cancels Runtime's existing
attempt token and stops; only the exact cancelled physical owner/epoch can receive
that result. `Lost` handles every unrelated, stale, settled, or missing claim.
No replacement guard is created for either outcome. The same Clock makes the next
ownership check fail closed, and claim/commit epoch fencing prevents stale
settlement. A worker that currently owns the separately claimed durable
cancellation receives `Current`, so its control attempt is not cancelled by the
intent it is responsible for completing.

## Consequences

- Long local Runs, foreground child Runs, cancellation, and slow resolution all
  retain their exact leases without an in-flight registry.
- A blocked renewal request cannot consume the whole lease, and transient
  failures do not wait one full regular interval before retrying.
- Pool-to-Worker handoff has one renewal task, not two concurrent writers.
- Deterministic claims are never compared with an unrelated wall clock.
- Remote topology uses the same signed exact-claim renewal; no owner-wide
  compatibility route remains.
- `renew_lease`, physical attempt admission, and quiescence are covered across
  Memory, SQLite, PostgreSQL, real HTTP, Worker, Pool, Direct, and TLA layers.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G6 (durable ingress additive over control).
- ADR-0011 — edge Clock injection and autonomous draining.
- ADR-0019 — exact `renew_lease` and distributed claim fencing.
- ADR-0065 — database-independent remote Worker topology.


### 2026-09-29 amendment: exact Run operational replay

Flow observed a completed Verification whose Run had already ended naturally
and whose exact Dispatch had recorded `Settled(Done)`, while its new execution
link still paged through unrelated global Dispatch history. Filtering after the
page limit makes each new link repay the complete partition history; unrelated
recovery operations can dominate observation without changing Run truth.

The existing `DispatchOperationalFeed::events_after` now selects an optional
exact Run before pagination. `None` retains the global operational view. Both
views use the same append-only rows, original global sequence identities, store
times, exclusive cursor, and decoder; empty pages retain the caller cursor.
Memory, SQLite, PostgreSQL and `AnyDispatchStore` share this contract. The sole
converged schema append point adds the `(run_id, sequence)` read index, leaving
both published migration histories immutable. No additional receipt store,
queue, cursor domain, outcome writer or public protocol event is introduced.

Flow supplies the link Run at this existing query boundary. It continues to
combine `Settled(Done)` delivery authority with committed Run outcome truth
before producing success through the existing inbox and WriteBack owner. A
Run ending naturally alone still cannot trigger successful acceptance. Claim,
physical-attempt exclusion, unknown-execution recovery and cleanup admission
are unchanged. Paging all foreign operations inside Flow or reading Awaken SQL
from Flow were rejected because filtering belongs to the existing feed owner.

The shared operational conformance comments own rules Q1-Q5: foreign history
exceeds the page limit; scoped pagination retains exact identities and times;
unknown/empty reads retain cursors; the global view keeps its original ordering.
The migration test upgrades both published histories through converged V6 to V7,
checks retained fact bytes, the actual SQLite query plan and idempotent reopen.
Flow extends its existing two-stream settlement test with a foreign prefix
larger than its page, retaining pre-settlement refusal and exactly-once replay.
These tests certify query and projection behavior, not predecessor termination
or completion of the original product roots.

### Published converged V7 migration identities

The exact Run operational index and governed entry closure were published on
independent branches as converged V7. The migration owner selects the known
index-first receipt by its exact checksum and appends closure at V8; the main
closure-first lineage appends the index at V8. Existing receipts are never
rewritten. Both histories reach the same schema and all future migrations use
one numbering stream. Unknown checksums retain the ordinary migration refusal.
SQLite adapter tests cover both persisted V7 prefixes, idempotent reopen, exact
receipt preservation and rejection of an unknown checksum. PostgreSQL uses the
same selection owner for migration and external-schema verification.
