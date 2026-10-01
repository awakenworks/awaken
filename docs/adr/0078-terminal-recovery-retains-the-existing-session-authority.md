# ADR-0078: Terminal Recovery Retains the Existing Session Authority

- Status: Accepted
- Date: 2026-09-07
- Amends: [ADR-0075](0075-unified-managed-session-worker-execution.md)
- Implementation: complete Resource projection and terminal-only release implemented; bounded same-process Kubernetes handle-free convergence implemented

## Context

Review against upstream main `982cc1b34` found that terminal assignment still
selected only active Resources. A Worker can already have staged desired B
before its activation receipt reaches the root. Sending active A -> A then
fails the Host's correct non-regression check against A -> B. Retrying the
same malformed projection cannot repair it.

The current root already retains active, pending, and Repository retirement
inputs. The current Host already serializes complete projection installation,
and recovery already pages past invalid terminal candidates and bounds control
calls. Adding another cleanup manifest, queue, recovery owner, or generation
counter would duplicate these mechanisms.

Container terminal preparation also rejects an absent durable handle. This is
an availability gap, but the fail-closed response is necessary until a provider
can prove more than a point-in-time absence. A Create request can be admitted,
lose its response, and arrive at Kubernetes after an absence query. Coordinator
lease checks alone cannot fence an already-sent physical request.

## Decision

### 1. Reuse the root's complete transition

The sole Session terminal assignment adapter uses the existing frozen desired
projection, which includes the previous active manifest. Warm polling and cold
claims share that adapter. The Host's existing Terminal mode stages this
projection without ordinary activation or Resource effects. Its revision and
Workspace checks are not relaxed.

Pending intent is not discarded at termination. Only the existing verified
cleanup receipt permits `complete_terminal_release`. Existing Repository
retirement plans retain their separate canonical inputs; active and pending
bindings are not flattened into an invalid single Resource manifest.

This first slice changes no aggregate, schema, public wire type, or API. Its
owners are `awaken-session-application` and `awaken-session-executor`; the Resource
state and cleanup contracts retain their current ownership. G55 and the
production adapter/Host tests enforce it.

### 2. Require physical closure before claiming handle-free absence

The physical slice remains inside the existing
`ContainerEnvironmentProvider::prepare_terminal_environment_for_effect` and
Session cleanup protocol. No generic effect service or product recovery API is
introduced. The generic runtime default and every unsupported topology stay
fail closed.

The provider must distinguish:

| Evidence | Allowed result |
|---|---|
| Exact reconstructible realization, matching namespace, scope, frozen provider configuration, original effect, Pod UID and retained PVC UID | Reconstruct terminal participants, then use existing prepare/dispose commands |
| Exact original attempt closed, every possible participant absent, and no admitted delayed Create can materialize | Verified absence |
| Missing handle, a point-in-time 404, timeout, ambiguous transport, foreign identity, missing participant evidence, or unsupported backend | Retryable uncertainty; no completion receipt |

Stable names and labels discover candidates; they never authorize deletion.
Memory/Secret writeback and pending Repository effects must be accounted for,
not silently skipped. No cleanup may delete the remote GitHub repository or
application/database PVCs.

Implementation requires an exact attempt-closure witness at the physical effect
owner: either a completed join of every admitted attempt or a durable primitive
that rejects late creates at their own linearization point. Merely checking a
ConfigMap/lease before a separate Pod POST leaves the same race. UID-fenced
deletion solves replacement safety, not late-create absence. Kubernetes,
Docker, and Podman adapters must name and test the primitive independently.
Until then unsupported cases remain visibly pending, not falsely converged.

The 2026-09-11 bounded implementation supplies the first of those witnesses for
same-process Kubernetes creation only. Before its first backend effect, the
Kubernetes adapter records the exact `(scope, operation, owner, runtime
incarnation, epoch)` attempt and enters a shared read-side gate around both Pod
reservation and activation. Terminal preparation may close only an already
observed exact gate, waits for every reader to leave, and leaves a process-life
tombstone that rejects late re-entry. While holding the close side it admits
only an ephemeral plan with no package, bind, Memory, or continuation-PVC
participant, then linearly observes both canonical Pod and PVC absence. A new
Worker has no gate witness and therefore remains pending. This is a join plus
absence proof, not the rejected point-in-time 404 rule.

### 3. Join an already-terminal Session through its existing cleanup owner

Ordinary user archive retains IdleOnly admission. The internal
`SessionApplication::release_terminal_session` uses a distinct TerminalOnly
admission in the existing archive CAS kernel and requires the existing verified
cleanup receipt before returning success. It admits only durable
`ActivationFailed` or `Terminated` execution, preserves the failure state, and
rejects every live execution class, including Idle. It exposes no public route,
new durable operation, epoch, or Run-to-Session lifecycle authority.

This narrows and replaces the earlier draft's proposed exact-Run release for
this failure-recovery slice. A Session's terminal execution is absorbing under
the existing state machine; an old Run therefore cannot use this command to
terminate a newer active Session. A Run that ended while its Session remains
live is deliberately insufficient. A future command that terminalizes live
Sessions would need exact activity admission and is not part of this repair.
The existing public archive and Repository publication paths are unchanged.

Run outcome, Session cleanup, and downstream Issue acceptance remain distinct
facts. A read-only UI can show committed failure plus cleanup pending without
creating a second durable status. Moving business completion before cleanup is
a separate policy change and is not part of this repair.

## Verification and release sequence

1. Reproduce active-only assignment on the latest main before fixing it. Cover
   empty/nonempty active state crossed with absent/unattempted/attempted pending
   state after an actual SQLite read. Assert exact previous/desired payload and
   zero root mutation. Check warm/cold Host installation and rejection of the
   old projection, without live Resource materialization.
2. Extend `SessionResourceActivation.tla` to retain pending state until receipt
   and check complete terminal projection and conditional convergence. Restore
   the old adapter projection and premature receipt as negative controls; each
   must fail its named invariant, not just produce a tool error.
3. `SessionUnmaterializedCleanup.tla` records the physical proof boundary. Its
   current config proves that unsupported cleanup remains pending. Its Target
   config checks convergence only assuming exact physical join and fair
   external progress. Removing the join must expose the late-Create race. The
   Kubernetes adapter's executable gate table covers the implemented
   same-process correspondence; durable cross-process closure remains outside
   that correspondence and fails closed.
4. Before implementing physical convergence, exercise actual adapter faults at
   intent commit, pre-create, PVC-only, Pod-created, response-loss, preparation
   receipt, disposal, and final root CAS. Resume an old Worker at every gap;
   race a newer incarnation and a same-name foreign UID. Run the same command
   conformance locally/remotely and across supported root stores. Cancellation
   of an HTTP future is not an attempt-closure witness.
5. Cover all seven execution states at terminal-only admission, unchanged public
   IdleOnly rejection, preserved failure, verified cleanup, response-loss replay,
   accepted Event fences, and unavailable cleanup followed by retry. Reuse the
   existing `terminal_execution_never_reopens` Kani harness and Session Root
   proof; they prove terminal absorption, not adapter cleanup completion. Keep
   a poisoned head, a timeout, and more than one page
   of bad candidates in recovery tests; reuse existing pagination/deadline
   owners rather than adding another scheduler.
6. Publish reviewed upstream fixes, update the consuming exact Git revision,
   then validate the local deployment. A full upstream-main upgrade is separate
   from this schema-free projection patch: consumers must adapt the new
   optional Environment sandbox-policy input and obey G47's old-writer cutover
   rules for changed Session payloads. Do not point shared manifests at local
   paths or unpublished commits, or patch Cargo caches.
7. Product acceptance still requires the two independent UI-created projects
   to produce real Issue -> Coding -> GitHub PR/commit evidence, accepted root
   output, clean-checkout offline tests, and their browser oracle. Mock tests,
   a terminal Session, or a model-checking pass cannot replace those results.

## Rejected alternatives

- Treating no handle/404 as success loses delayed or partially created effects.
- Rolling the Host revision backward hides missing pending cleanup obligations.
- Duplicating a cleanup aggregate, queue, or Resource collection creates a
  competing authority alongside the existing Session protocol.
- Public force archive permits a stale Run to terminate a newer activity.
- A new Run/activity release protocol is unnecessary for an already-terminal
  Session; it expands authority and compatibility work beyond this repair.
- Fixed startup sleeps assume network timing; downstream workers should wait
  for actual control and advertised MCP transport availability, with shutdown
  cancellation and Worker-independent server liveness.

## Consequences

The first repair is small and independently testable. Same-process,
zero-participant Kubernetes attempts can now converge without a durable handle;
cross-process and rich-topology physical availability gaps remain explicit
rather than being hidden behind a successful receipt. Formal evidence states
the bounds and assumptions; production physical conformance and downstream
end-to-end acceptance are still required.

## Amendment 2026-09-25: profiled release joins terminal cleanup

The existing Workspace-scoped profiled Session release route is used by Flow to
close a failed WorkUnit before it may retry. Its no-publication branch previously
called the public idle-only archive, so an `ActivationFailed` or `Terminated`
Session with unverified cleanup retried forever with “only an idle Session may
be archived.” This was a transport-adapter mismatch, not permission to force
terminate a live Session.

That same profiled route now reads the durable Session root only to select the
existing application command. An already-terminal root joins
`release_terminal_session` and requires its verified cleanup receipt. An idle
root still passes idle-only archive admission and then joins the same terminal
cleanup; every other live state remains rejected. The application rechecks the
root under its canonical CAS, and the route retains its existing exact
Workspace ownership authorization. The ordinary public archive remains
idle-only. No new route, cleanup queue, status authority, credential, or Run
permission is introduced; this is the existing product release contract
reusing the terminal-only Session owner.

## Amendment 2026-09-28: reuse the retained terminal Repository source

A failed or response-lost terminal preparation can leave the exact bound
Environment in the process-local Retiring phase while the aggregate still owns
an uncompleted Repository publication. Reusing the ordinary LiveSource role
then rejects the retained owner forever, even when the provider proves the same
physical source is Ready. A fresh Host does not exercise that hot retry shape.

The existing bound-Environment preparation mode now distinguishes terminal
Repository publication from ordinary live/checkpoint adoption. Both require the
same Ready observation and exact binding/handle validation. Only the aggregate's
terminal Repository command may reuse an exact bound terminal-retiring Arc; it
keeps that owner retired and does not reopen tool execution. Unpublished,
foreign, closed and disposing sources remain rejected; a revoked wrapper is never
reused directly (the fresh-wrapper amendment below owns recovery). Current lease,
Workspace, publication expectation and provider effect fencing are unchanged.
There is no additional owner, receipt, cleanup queue or persistent state.

The existing real Git/Namespace integration covers publication and byte-identical
replay with this retained owner, rejection by ordinary LiveSource, and rejection
of nonterminal retirement. Its original cold recovery, remote-ref rejection and
ordered cleanup cases remain in the same test. Real product acceptance still
requires the exact consuming revision and the UI delivery evidence above.

## Amendment 2026-09-28: a closed publication source is a negative outcome

A Kubernetes eviction can close the exact bound source after Coding returns
but before its Repository publication is acknowledged. Retrying a Ready-only
publication then has no possible local progress. A transport error, missing
identity marker, foreign incarnation, or elapsed timeout does not prove this
condition and must not be promoted to source closure.

The existing `SandboxObservation` and provider validation own physical evidence.
For terminal Repository publication alone, a validated `Terminal` or
`DefinitivelyUnavailable` observation returns a non-executable preparation and
zero Git I/O. `Disposing`, `Provisioning`, incompatible evidence and observation
errors retain their existing refusal/retry semantics. Live/checkpoint callers
still require Ready and cannot consume this negative preparation.

`SessionRepositoryPublicationRejectionReason` extends the existing negative
sidecar with the source-binding fingerprint and closed observation. The Session
aggregate compares that fingerprint to its retained source before the existing
lease-checked root CAS; identical durable replay works after source release.
The original transport CAS JSON and fingerprints are unchanged, and the sole
`RepositoryPublicationRejection` transport type still admits only real ref
conflicts. The existing signed Worker rejection endpoint carries both causes.

The causal sequence is observation, negative outcome CAS, ordinary preparation,
physical disposal, and Flow's existing failure/cleanup convergence. Source
closure explicitly retains an unknown remote publication outcome: it neither
creates a success receipt nor proves that an earlier push did not happen.
Any later Coding stage still needs its exact accepted inputs and publication
CAS. The failed output and immutable publication intent remain recorded.

Rejected alternatives are retrying a permanently closed source indefinitely,
inventing `RemoteRefAbsent`, weakening the Ready requirement for Git, or deleting
the publication sidecar to bypass cleanup. No publication queue, recovery store,
physical-stop authority, business Git-ref writer, or new Run status is added.

The test design lives with the existing cases: contract closure/legacy-wire and
foreign-source tables; a real Namespace source-loss injection preserving its
identity marker and independent bare remote; Session/SQLite cleanup and replay;
and signed Worker HTTP cause/lease transport. The original FPS same-data
deployment and UI root acceptance remain required live qualification.

## Amendment 2026-09-28: terminal Container cleanup does not rebind live control

The real source-loss rollout recorded the negative publication outcome but then
blocked ordinary preparation: Container adoption tried to obtain a Ready-only
Sandbox control binding from the same Terminal Pod. A recorded control
incarnation is still part of its immutable handle, but cannot reopen a live
listener after the owning process terminated.

The existing adoption helper now receives the existing observation purpose.
Only the fenced terminal-preparation caller, after exact Terminal/Disposing
observation, preserves the recorded control identity without live rebinding.
Ready and ordinary recovery retain the real binding requirement. Spec topology,
physical incarnation, current authority, Secret and Memory participant recovery,
and the later authorized disposal remain unchanged; the observation purpose
does not itself grant mutation authority.

Host reuses `RecoverOnlyUnprepared` for a Terminal Container because its file
scanner requires attached exec. It recovers existing artifact receipts and
still requires provider preparation. Writable Memory without readable source or
prior durable preparation remains refused; durable Secret write-back cannot be
softened into stale bytes. Filesystem terminal owners retain their direct file
read path, and Disposing alone continues to prove prior source preparation.

There is no new lifecycle mode, capability store, receipt, queue, RPC endpoint or
Ready fallback. The existing fake-runtime authority-wait table covers no live
control on closed owners and real control on Ready owners; Host mode tests
retain the preparation/Memory boundary. The live k3d control-Pod test terminates
only its own agent, retains original identity, runs fenced preparation/disposal,
and independently observes Pod absence. Original FPS/UI delivery remains a
separate same-data acceptance requirement.


## Amendment 2026-09-29: fresh terminal wrapper after realization revocation

A real Coding completed with an unpublished local commit in a Ready retained Pod,
but realization revocation left its published Environment behind a local retirement
fence. Direct reuse remains forbidden. Terminal Repository preparation previously
kept retrying that same closed wrapper instead of using the existing rebound path.

Only an exact Durable Bound retirement for RealizationRevocation, the aggregate
terminal Repository effect, current effect authority and validated Ready source
admit this repair. The existing provider `rebind_retained` replaces the closed Hand
wrapper over the same physical source. The one owner then uses its existing
retiring-adoption Candidate and committed-Candidate retirement transitions atomically,
retaining the original Store-read identity and binding. The wrapper remains Retiring;
no ordinary Adopt receipt, Resident publication, model/tool admission, new owner,
queue or persistent state is added. Admission is checked before and after provider
IO. Unbound, LegacyDirect, foreign-target and other retirement causes keep their
existing refusal or recovery path. Closed-source and disposal decisions are unchanged.

The real Namespace/Git publication fixture first reproduces the same non-resident
error, then requires a different wrapper with the same Store identity, binding and
physical handle, continued ordinary-reader closure, exact remote commit and receipt
replay. Its original source-closure and divergent-remote cases remain exercised.
This source proof does not certify the pending original UI delivery or missing
historical physical-attempt termination evidence.

## Amendment 2026-10-02: local Worker exit evidence stays below the same ports

A Linux user-service adapter now pairs the existing `WorkerExecutionBindingSource`
and `WorkerExecutionStopSource` through one `WorkerTransport`. It binds an exact
Worker incarnation to a unique systemd transient unit, its `InvocationID`,
cgroup and kernel boot identity before Dispatch admits physical execution.
The unit must retain exit state, have no automatic restart and use
`KillMode=control-group`. Neither the Worker request nor a process-group PID is
accepted as an exit receipt.

After a closed Worker is authorized for stop, the adapter rechecks the original
unit, kills its cgroup and waits for the exact invocation to have no remaining
process group. It durably writes a provider-owned physical exit receipt before
retiring the transient unit; response loss can reopen that receipt and finish
the same unit retirement. A changed kernel boot proves the old local process
cannot survive only when the unique unit name is not reused. Missing or
conflicting evidence stays Unknown. The Registry still records the Worker exit,
and Dispatch alone retires its physical attempt; this filesystem receipt is
neither a Run result nor a Session cleanup manifest.

The local adapter currently admits complete stop classification only for the
Native model-only coverage profile. Journalled tools, opaque ACP execution and
response-only remote requests remain Unknown until their independent effects
have pre-entry enrollment and physical closure evidence. The platform launcher
can now preselect the Worker's 128-bit incarnation through the canonical
`WorkerNodeBuilder`, and shares the exact unit-name projection with the stop
adapter. It has not yet selected this adapter or started the matching unit
automatically, so the local product does not yet claim user-invisible hard-crash
recovery. Flow's existing `supervise_agent_worker` loop, not systemd
`Restart=`, will own placement of each new Worker incarnation; the unit owns
only physical process supervision and never becomes a second claim scheduler.
Kubernetes cross-process Sandbox creation and original UI-root
acceptance remain separate outstanding gates.

## Amendment 2026-10-02: Kubernetes Namespace deletion is not a request join

The K8s adapter currently records an exact create attempt in process memory.
`prepare` can create a retained PVC before its Pod, and `converge` can create
ConfigMaps and Secrets after the Pod. The handle-free proof is deliberately
restricted to a zero-participant plan and an attempt gate observed by the same
process. A replacement Worker has no such gate and returns Unknown. This is the
correct safety outcome, not completed cross-process recovery.

A live isolated k3d v1.31.5 experiment rejected a tempting shortcut. A
Pod Create passed `NamespaceLifecycle` and paused in a validating webhook. The
Namespace then fully deleted. After the webhook released the already-admitted
request, the API returned Pod creation success and an all-namespaces list showed
the Pod under the deleted Namespace name. The isolated Pod and webhook were
subsequently deleted. A namespace deletion timestamp, finalizer completion,
namespace 404, Pod 404, and a finite retry count therefore cannot establish
that every earlier admitted request has finished. A namespace may help isolate
new requests, but is not a request-completion witness by itself.

The physical effect owner must join every admitted Create request through an
independent request lifetime and retain an exact, durable result before a
replacement process may claim closure. That witness must cover PVC, Pod,
ConfigMap, Secret and other participants admitted by the frozen plan; an
unknown broker lifetime or result remains Unknown. The existing provider
`preflight_create_for_effect`, `prepare_for_effect`, `activate_for_effect`,
`prove_unmaterialized_absence`, exact UID deletion and Session cleanup receipt
remain the only execution and cleanup path. The implementation must extend
those ports rather than introduce a Flow cleanup queue or infer closure from
Worker expiry. Until a provider-backed request join is implemented and tested
across Worker and control-plane restarts, Flow cannot advance a successor on
handle-free Kubernetes cleanup.

## Amendment 2026-10-05: unfinished terminal cleanup remains Worker demand

A Hosted Run can settle its Dispatch lease after the Session root has recorded
terminal Repository rejection while the same Pod-bound Worker still owes
preparation or disposal. Dispatch depth then becomes zero, and an autoscaler
reading only that depth can remove the sole physical owner before cleanup
completes. A new Worker cannot claim the old source by guessing its identity.

The Session root's existing verified terminal cleanup operation is the only
authority for this remaining work. Its read-only operational projection reports
demand exactly when execution is terminal, cleanup has been requested or is in
progress, and the verified operation is incomplete. Corrupt sidecars fail the observation; lease
expiry or Pod absence never turns an incomplete operation into completion.
The existing Session repository contract reads its `Reconcilable` scan to
count distinct retained Worker owners plus one first-placement need for any
unbound, never-materialized terminal root. It fails closed on corrupt roots,
quarantine, an invalid physical owner, or an incomplete scan. Hosted composition
consumes that read-only count alongside Dispatch depth to keep Worker capacity until
the root records its actual completion. The read neither claims cleanup nor
adds a queue, journal, receipt, reassignment rule, or recovery writer. A
historical lost Pod still needs its original physical exit evidence and is not
made replayable by a positive demand metric.
