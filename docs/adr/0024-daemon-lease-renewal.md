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
No new table, receipt store, public API or wire format is introduced.

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
recover a complete committed `TaskReference` whose endpoint matches the frozen
publication. Its recovered `execute` path performs only `tasks/get`; it cannot
repeat the initiating `message:send`. The leaf executor may therefore request
`handoff_attempt`, which atomically replaces the predecessor slot with the exact
current claim in the same Dispatch row. Missing, malformed, or endpoint-mismatched
state, a fresh send, and A2A resume all retain ordinary blocking. This is neither
lease-expiry cleanup nor a generic process-crash bypass.

```text
claim A -> begin A -> message:send -> commit exact TaskReference
process A crashes while polling -> lease expires -> claim B
executor B proves exact committed continuation
handoff B replaces slot A -> tasks/get (never message:send) -> finish B
```

The A2A executor also uses the Runtime's same attempt-local return observer.
Send/resume failures, a failed reference commit before a remote boundary,
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

The existing CheckpointRecovery model covers five entry classes and four prior
evidence classes within MaxChunks=3, including control return without evidence,
resume/handoff before observation, retained unknown checkpoints and read
settlement under fair local timer/scheduler progress even for a pending store.
Fourteen semantic mutations and thirteen reachability witnesses guard
the model. Runtime tests separately cover actual permission/tool/child calls and
control replay, deadline expiry without cancellation, and successful slow reads.
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
This model treats each physical attempt as one external invocation; the
per-model-operation ordinal, sequential retries inside an attempt and opaque
SDK retries are outside that abstraction. The Native watchdog regression
separately enables retry, failover and outer tolerance at the actual Runtime
boundary and checks that abandoned inference cannot enter any of them.

The Managed adapter projects committed Indeterminate through its existing
`session.error` / `unknown_error` / terminal retry status, before terminal idle.
Root and child histories use one Run-state classifier; repeated and cold reads
keep the same deterministic error identity. This reports a logical outcome,
not physical quiescence, and adds no Awaken recovery field to the standard wire.

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

If renewal reports lost ownership, no replacement guard is created. The same
Clock makes the next ownership check fail closed, and claim/commit epoch fencing
prevents stale settlement.

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
