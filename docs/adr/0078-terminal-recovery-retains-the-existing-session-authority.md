# ADR-0078: Terminal Recovery Retains the Existing Session Authority

- Status: Accepted
- Date: 2026-09-07
- Amends: [ADR-0075](0075-unified-managed-session-worker-execution.md)
- Implementation: Resource projection and terminal-only release implemented; bounded direct-process and brokered cross-process Kubernetes Pod/retained-PVC handle-free convergence implemented; six-Kind Create name isolation and guarded PVC-only terminal cleanup implemented; richer participant closure remains pending live qualification

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
`missing_handle_terminal_evidence`, exact UID deletion and Session cleanup receipt
remain the only execution and cleanup path. The implementation must extend
those ports rather than introduce a Flow cleanup queue or infer closure from
Worker expiry. Until a provider-backed request join is implemented and tested
across Worker and control-plane restarts, Flow cannot advance a successor on
handle-free Kubernetes cleanup.

## Amendment 2026-10-02: target provider-owned Kubernetes Create join

The live late-Create counterexample above rules out Namespace deletion as the
atomic close primitive. A point-in-time Pod/PVC read, Worker Pod termination,
expired lease, or completed Namespace finalizer is likewise insufficient. The
remaining provider capability is an independent Create executor whose admission
and completion can be joined after the Worker process disappears. This is a
**target**, not a current recovery guarantee.

The existing `K8sRuntime` remains the Sandbox effect owner: it constructs the
Pod, retained PVC, ConfigMap and Secret from the frozen plan and validates the
current `ContainerEffectFence`. The existing signed Worker control transport
and Kubernetes `WorkerTransport` select an independent Coordinator-side Create
executor. The Worker receives no direct Create privilege for those Sandbox
resources in this topology. The executor uses the existing Kubernetes client
and exact object verification; it holds only physical request state, never a
Session, Dispatch, Flow WorkUnit, cleanup queue or business retry policy.
Production admission must independently verify that the Worker ServiceAccount
cannot bypass the broker with direct Create calls.

Before forwarding each Create, the executor atomically adds a ticket under the
exact `(realization namespace, scope, effect owner, operation, runtime
incarnation, epoch, object kind, object digest)` to a provider-owned durable
ConfigMap in a separate control namespace. Kubernetes `resourceVersion` CAS
orders this admission against an exact close transition. A request admitted
before close remains visible as InFlight until its independent executor has
observed the API outcome and durably recorded the object UID or a definitive
no-effect refusal. A close that wins CAS rejects every later ticket. A ticket
that wins first must be joined, even if its Worker has died or a stale response
arrives. An executor crash between forwarding and durable completion leaves
InFlight/Unknown; no response timeout, retry count or 404 may retire it. An
active-active Coordinator must use the same CAS, not a process-local mutex, for
this admission order. The ledger's own UID cannot serve as its only identity
anchor: a deleted and recreated ConfigMap can report its new UID while hiding
the old ledger and an admitted late Create. Before the first Create is forwarded,
the Coordinator initializes the ledger and the existing Worker Registry first
registration atomically freezes its UID and provider configuration with the
exact Worker incarnation. This is one physical anchor for both Managed Session
effects (whose realization lease names that Worker incarnation) and ordinary
Run effects (whose Dispatch physical binding names it); neither business owner
acquires a duplicate ledger field. Per-effect ticket groups can close while the
Worker remains active. A replacement process reads the retained historical
Registry record and may only reobserve the frozen UID. If the anchor is absent
after an effect could have started, or the ledger has a different UID, closure
remains Unknown. An unanchored legacy incarnation must never be upgraded by
creating a fresh ledger under the same name.

The Create authority includes indirect Pod creators. The pre-broker Worker Role
permitted Job and DaemonSet Create, while the separate package-image provisioner
creates ConfigMaps and Jobs and image prewarm creates DaemonSets. Their
controllers may create Pods after the Worker's own Pod has stopped. Removing
only direct Pod/PVC/ConfigMap/Secret Create from the Worker Role would therefore
leave a bypass. The independent executor must cover these Create paths under
their existing package-build or warmup owners, or the Worker must have no
authority to invoke them in the brokered topology. A Session cleanup proof
must classify their independence from that Session or join their exact
controller-owned participants; an unclassified Job/DaemonSet remains Unknown.

Once all admitted tickets are Complete, the same provider close checks the
exact plan's full participant set, uses UID-fenced preparation/disposal, and
retains the closed witness until the original Session cleanup owner commits
its receipt. Only then may Flow's existing failed-stage progression continue.
A missing ledger, legacy direct-Create path, broker read error, foreign UID,
partial PVC/Pod, opaque remote effect or unmatched Worker execution domain
remains Unknown. The present process-local `creation_attempts` gate is to be
consolidated into this one provider admission path when the broker is selected;
it cannot be treated as an independent cross-process fallback.

Acceptance must include the IC-11 before-send, admitted-before-response,
response-before-receipt, Worker death, Coordinator restart, same-name/foreign-UID,
PVC-only, Pod/ConfigMap/Secret partial, stale response and cleanup-receipt-loss
cuts. The corresponding test comments own the cause/effect table. The safety
result is zero early receipts/successors whenever any ticket or participant is
Unknown; conditional progress requires one exact terminal receipt and one
successor after all independent physical results have been retained.

The 2026-10-04 implementation checkpoint freezes the exact ConfigMap UID in
the existing Worker Registry before Ready, admits Create tickets through one
provider-owned resourceVersion CAS state machine, and forwards Worker control
POSTs through the existing signed transport to an independent Coordinator
task. The product K8s overlay gives Worker only read/disposal and bounded
exec/port-forward authority; Coordinator alone holds Create for Pod, PVC,
ConfigMap, Secret, Job and DaemonSet collections. The real k3d checks cover
UID persistence, same-name replacement refusal, CAS ordering, signed Create
and the six direct-Create RBAC denials. This is **request-lifetime evidence**,
not a complete handle-free cleanup receipt. The replacement process still
returns Unknown until the original Worker execution domain is stopped, every
ticket is joined, and the provider has verified/disposed the exact Pod, PVC,
ConfigMap, Secret and any controller-owned participant before the existing
Session cleanup owner commits its receipt.

The next closure slice retains a Worker process binding before Registry insert in the
same provider evidence authority: the Kubernetes control ConfigMap holds the
exact Pod binding, while the existing systemd evidence directory holds an
immutable unit binding. The post-registration retention read is idempotent and
checks the same coordinates before Ready. A closed idle Worker with no Dispatch slot receives an
exit receipt only after the independent stop source observes that original
process. Ingress persists and reads back the receipt in the existing Worker
Registry before provider evidence can be released. A graceful Quiesced Worker
that is still returning its deregistration request is observed again later;
only after the original Registry lease deadline may an active closed Worker be
stopped by the same exact provider operation. Both HTTP and Host maintenance
pass their existing trusted clock to this decision.
This closes the evidence-release ordering gap for newly registered independent
Workers; missing legacy bindings remain Unknown.

For the deliberately narrow handle-free, zero-participant Kubernetes shape,
the provider requires a complete sealed request join. All definitively refused
Creates plus exact Pod/PVC absence prove no materialization. A single successful
canonical Pod Create can instead be joined to its still-live original UID and
effect fence, provided its complete normalized Pod security projection matches,
no PVC or other successful participant exists, and the frozen spec has no mount
or control-service owner. The ordinary first-Create path uses the same complete
Pod verifier, so copying a digest annotation onto a changed executable or
security spec cannot authorize either creation replay or terminal recovery.
The provider constructs a
temporary typed V2 handle and runs the existing terminal observation,
preparation and UID-fenced disposal; it does not issue a cleanup receipt from
the ticket alone. A Created Pod that has already disappeared, a terminating or
foreign Pod, an orphan claim, another Created kind, or an InFlight ticket remains
Unknown. In particular, a Coordinator hard exit after forwarding the API POST
but before retaining its outcome is still unresolved; no later 404 converts
that ticket into a joined result.

The request ledger is grouped conservatively by the entire Worker incarnation,
so an unrelated successful Create can also keep this narrow proof Unknown; a
future per-effect refinement must preserve that safety ordering and avoid a
second ticket authority. In brokered mode the runtime does not enter the
historical process-local Create gate, and a runtime that has already admitted a
direct effect cannot switch transport modes. The gate remains only for
direct-mode compatibility with its original process-lifetime proof boundary.

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

## Amendment 2026-10-05: isolate an unjoined Pod Create by its exact name

The earlier handle-free path deliberately returned Unknown after a Coordinator
hard exit between Pod POST and ticket-result CAS. A real API experiment showed
that Namespace finalization does not close an already-admitted Create: the Pod
can appear after the Namespace has disappeared. A second experiment held the
old Create at admission while an inert Pod occupied the same Namespace/name.
The old request then returned 409; deleting that inert Pod before releasing the
old request instead allowed the old Pod to appear. Therefore the name must
remain occupied for the entire unknown request lifetime.

The existing provider ConfigMap remains the sole Create authority. Its first
resourceVersion CAS now mints an isolation ID for each Pod ticket, and the
Coordinator stamps that ID and the ticket's original request digest on the
forwarded Pod, together with a Create-result finalizer. An original Pod that
reached storage therefore remains visible even if deletion was requested
before its 201 result could be retained. The existing exact-Pod finalizer CAS
releases only this guard after UID-fenced terminal deletion intent, through
the handle-bound cleanup path; unreceipted scope cleanup cannot release it.
Without that retained original, a reservation winner could not distinguish
"never created" from "created and then disappeared" and would be unsafe.
After the existing Registry has retained the original Worker
process-exit receipt, the existing seal path CASes an `isolation_pending`
marker into that ticket **before** racing one unschedulable, tokenless Pod
reservation against the still-InFlight original request. A later definitive
response from the original POST does not clear the pending marker or let the
group join; a replacement Coordinator replays the same reservation until its
own API effect is observed and CASed. The apiserver's
same-name Create serialization yields either the exact original UID or the
exact reservation UID. A lost reservation response is resolved by reading that
same UID and full immutable reservation projection; an unrelated occupant,
missing UID, lost policy, failed CAS, or disappearing object remains Unknown.
The ticket records `Created` or `Isolated` in the **same** ledger. A late old
POST that loses to the reservation cannot publish while that name is retained.

`Isolated` is not a Session cleanup receipt. The handle-free terminal reader
requires the sealed Worker/group join, the canonical Pod ticket, no other
successful participant, the exact reserved UID and normalized Pod security
projection, no node binding or container execution status, an active cluster
admission policy/binding, an actual UID-fenced
dry-run DELETE denied by that policy, and exact PVC absence. Only then can the
existing Session cleanup owner write its receipt and Flow use its existing
failed-stage continuation. `Created` still uses the full original Pod
projection, temporary typed handle and ordinary UID-fenced disposal. The
policy denies Pod UPDATE/DELETE for the reservation label; the runtime roles
cannot modify the policy or use Pod collection deletion. The finalizer alone
does not prove immutability. The admission policy and reservation are retained
because no bounded timeout or finite number of 404s proves the old request
has finished. An operator with cluster-administrator authority is outside this
runtime trust boundary and must not remove this policy or its reservations
while old requests may exist.

This slice extends only the K8s Sandbox physical adapter and its deployment
policy. Flow WorkUnit, Dispatch attempt, Worker Registry, Session cleanup owner,
and the direct-mode process-local gate remain their existing authorities.
At this Pod-only checkpoint, historical tickets without an isolation ID,
non-Pod InFlight tickets, rich participant plans, remote/opaque effects, and a
missing or unready admission policy remained Unknown. The retained inert Pod
consumes one Pod name and quota entry for each unresolved original request; that
availability cost is explicit
and must be included in capacity planning. The IC-11 fault matrix needs both
race orders, response loss, process restart, policy drift, same-name/foreign
UID and cleanup-receipt loss before general automatic recovery is claimed.

The opt-in k3d provider test `brokered_create_isolation_joins_six_real_api_uids_after_reservation`
now exercises the crash-before-send row for all six Kinds against real
ConfigMap CAS, API defaulting, admission policy activation, dry-run DELETE
denial and exact resource cleanup.
The opt-in `original_create_guard_keeps_a_deleted_pod_visible_until_exact_release`
test observes that a UID-fenced DELETE leaves the guarded Pod visible and a
same-name Create returns 409; exact guard removal then allows Pod and Namespace
cleanup.
It does not establish a full Flow successor or an already-sent original POST
race in the product topology; those remain separate acceptance rows.

## Amendment 2026-10-06: same-name isolation is observable for each brokered Kind

The existing opt-in late-Create negative control now exercises the two
same-name race orders for Pod, PVC, ConfigMap, Secret, Job and DaemonSet. In a
dedicated real k3d API, each original request paused inside admission returned
409 when an inert object of the same Kind retained the exact namespace/name;
releasing that object before resuming the old request instead allowed a new UID
to publish. The Pod Namespace-deletion counterexample remained reproducible.
The fixture uses pending, non-binding PVCs, suspended Jobs, and DaemonSets with
an unmatched node selector; it checks the Job/DaemonSet reservations have no
controller-owned Pods at observation time. Each case removes its unique
Namespace, resource and webhook before returning.

This is only a provider-primitive qualification. A production closure witness
still needs the original request's provider-stamped identity and retention,
durable ticket CAS, protected immutable reservation, exact UID and projection
read-back, and the resource's own side-effect/participant-stop proof. In
particular, a sampled absence of Job/DaemonSet children does not stop their
controllers, and a pending PVC observation does not prove the old claim could
never allocate storage. At this primitive-only checkpoint, the Pod-only ledger
remained the authority; non-Pod InFlight tickets and rich plans remained
Unknown until those additional facts could be implemented in that same path.

The provider ledger now mints one immutable nonce for each of the six brokered
Kind tickets and races the same-Kind inert reservation after the exact old
Worker stop. The common broker stamps the original request's nonce and digest;
only Pod originals gain the original-result finalizer because its existing
disposal owner can release that guard. For non-Pod reservations, the provider
checks the exact UID and metadata, policy-selected dry-run DELETE denial,
Kind-specific inert shape, and absence of Job/DaemonSet-owned Pods. Pending
PVCs may carry Kubernetes's `pvc-protection` finalizer in addition to the
provider guard; any other extra finalizer fails the exact check. The non-Pod
projection rejects extra PVC sources, ConfigMap/Secret data, executable Pod
template fields and unknown controller spec defaults. A real DaemonSet read
reported the first `deprecated.daemonset.template.generation=1` annotation;
only that exact Kubernetes-added value is admitted, while a later generation
or any other extra annotation stays Unknown. Other unrecognized observations stay
Unknown rather than borrowing an inert reservation proof. The real k3d
test joins six independently admitted crash-before-send tickets, replays all
six UIDs, and removes the task-owned objects and admission policy. Its earlier
PVC failure reproduced the built-in finalizer before this exact allowance was
added. The product runtime Role no longer grants Secret `deletecollection`,
which would bypass the retained object's exact-UID deletion path. A broad
admission generation observation or a sampled 404 remains insufficient. A
dedicated k3d RBAC readback of that exact Role returned `no` for collection
deletion on all six protected Kinds while retaining `yes` for Secret `get` and
single-object `delete`.

This closes the six admitted Create request lifetimes only in the demonstrated
reservation case. A non-Pod `Isolated` ticket remains `Unknown` to handle-free
terminal preparation because the provider has not yet proved complete
participant disposal. Rich plans now seal and join the same brokered request
ledger after exact Worker exit but still return `Unknown`; direct rich attempts
keep their existing fail-closed path. Normal non-Pod originals do not receive
an unowned finalizer. Created PVC/ConfigMap/Secret, partial projection, Job or
DaemonSet children, remote/opaque execution, and missing historical nonces
still cannot produce a Session cleanup receipt or Flow successor. The separate
late-original race test proves same-name exclusion, not a whole product retry.

The signed Worker-side Create client is assembled by the existing
`awaken-worker` process composition root, which already owns both its
`WorkerUpstream` and selected Sandbox provider. The Platform
`awaken-coordinator-client` retains its neutral Worker control clients and no
longer depends on the concrete Sandbox adapter across bounded contexts. The
same client, endpoint, Registry identity and provider ticket ledger remain
authoritative; this placement change adds no recovery or cleanup owner.

## Amendment 2026-10-12: recover a complete original inline projection

The existing brokered request join and original Pod result handoff have
different responsibilities. The join closes every admitted Create for the
Worker incarnation; the frozen Pod declaration and complete original RESULTS
select this Session's participants. Finished tickets from another Session do
not become members of this Session and are never read or deleted by its
recovery. Duplicate tickets for the original Pod or PVC remain contradictory.

For self-contained read-only Inline and InlineBytes inputs, handle-free
recovery can now reuse the existing V3 participant set after verifying the
original Pod UID, full immutable Pod projection and original effect stamps.
Creation and recovery share the same projected object builder and stamping
helper. Each member must have an exact Created ticket in the original group,
namespace and name, with the retained UID. Live readback must also match its
original nonce, request digest, Pod owner and immutable content. An already
absent member retains its original ticket-backed UID; a later GET never
supplies an original identity. Recovery does not create or publish objects.

Missing or partial RESULTS, an absent original Pod, unsupported participants,
or conflicting identity stays Unknown. The narrow joined-empty witness keeps
its original meaning and cannot stand in for a missing rich handoff. Memory,
Secret writeback, control and unresolved package or reference materialization
still require their original retained evidence. The inline recovery result
does not prove full execution closure, target access, physical disposal or
Session completion. The existing terminal authorization, two-phase driver,
UID disposal and Session receipt remain the sole path to completion.

The original Dispatch maintenance commands must retain a dead-letter row
while any physical slot, binding or closing fact remains. Both unlimited and
time-windowed GC use the same empty-slot requirement. SQLite keeps its existing
writer transaction; PostgreSQL freezes one candidate set under the original
row locks before deleting pending input and dispatch rows. Historical unbound
or corrupt facts are retained, rather than erased into a misleading empty
observation. Only the original exact close/finish transition makes an occupied
row collectible. This preserves evidence; it does not manufacture complete
execution closure from an empty slot or a retired epoch.

## Amendment 2026-10-07: reconstruct one exact retained Pod/PVC pair

The existing Worker-incarnation Create ledger and exact original Worker stop
remain the request-lifetime authority. For a frozen plan with no package, bind,
Memory or control participant, a complete join may contain one Created
canonical PVC ticket and one Created canonical Pod ticket. The K8s Sandbox
adapter requires both original UIDs still live, the PVC's original effect
identity and complete immutable storage projection, and the Pod's complete
security projection with its binding to that exact PVC UID. It then supplies
the existing terminal owner with a temporary typed V2 handle containing both
UIDs. A still-Provisioning Pod may be adopted for terminal cleanup only after
this complete reconstruction; an ordinary handle does not gain that admission.
The existing Pod resourceVersion/finalizer CAS serializes PVC deletion against
Rebuild, and UID-fenced disposal must finish before Session writes its cleanup
receipt. No Flow state, provider queue or second PVC deletion path is added.
First Create, 409 replay and terminal reconstruction use the same typed PVC
projection verifier; a copied digest cannot hide a changed storage request.
The frozen adoption fingerprint also binds direct versus brokered Create
transport. A replacement cannot upgrade a direct-created Pod into brokered
cross-process evidence; the live fixture must use brokered mode on both sides.

One missing or InFlight ticket, a missing/terminating/foreign PVC, a changed
storage projection or Pod binding, a vanished original Pod, any unaccounted
Kind, and a live PVC with Pod 404 all remain Unknown. PVC-only recovery still
needs a provider-side atomic exclusion proof against a delayed Pod Create;
name isolation or repeated 404 alone cannot supply it. Worker-wide grouping
can conservatively block this pair when unrelated Creates exist. Group
refinement requires its own concurrency counterexample and migration design.

The Worker process is inside the trusted runtime boundary: the current Create
broker verifies Worker identity, collection path and ticket admission, but does
not authorize the full Worker-supplied object against a frozen Session plan.
Sandbox workloads must not receive its token. This amendment repairs crash and
response-loss recovery; it does not claim confinement of a compromised Worker.
The recovery decision itself uses typed Pod, PVC, plan, fence, ticket and
continuation handle data. The broker's existing free JSON is confined to the
HTTP forwarding boundary; replacing it requires one coordinated migration of
the six-Kind Worker/Create wire and kube defaulting rules, not a parallel
decoder inside recovery. Provider tests cover the pair's positive and negative
UID/projection rows and reuse of the terminal disposal owner. Real API and
product-topology acceptance remain separate evidence gates.

## Amendment 2026-10-08: one guarded PVC-only terminal participant

The exact old Worker exit, Registry-frozen ledger UID and complete sealed
request join remain the only cross-process Create admission proof. For a frozen
retained plan with no package, bind, Memory or control participant, a joined
original PVC Create may precede a lost Pod handle. The existing provider ledger
now records one terminal Pod-name reservation in the **same** ConfigMap CAS
state. If no original Pod ticket exists, the provider inserts a typed terminal
guard ticket; if a Pod Create was definitively rejected, it reuses that
ticket's original nonce. An admitted InFlight Pod ticket continues through the
existing isolation transition. A Created Pod remains on the ordinary exact
Pod/PVC handle path. No second request queue or retry executor is introduced.

After the independent broker has reverified the old Worker exit, a same-name
unschedulable Pod wins or loses Kubernetes's atomic Create name race. The
existing immutable reservation projection, UID readback, active admission
policy and exact dry-run DELETE denial must all hold. The original PVC ticket
must name the canonical claim and its still-live UID, predecessor effect and
complete storage projection. The provider then constructs a terminal-only
typed handle. It does not admit a runnable Pod or issue a cleanup receipt.
The existing Session terminal owner prepares with zero user-byte harvest and
later invokes `ContainerSandbox::dispose_for_effect` with its separate current
authorization. That edge rechecks the guard and original claim, uses the one
`continuation::initiate_claim_deletion` UID/resourceVersion-fenced deletion,
waits for the exact claim to disappear, and rechecks that the guard remains
protected. The retained inert Pod is the durable exclusion proof; it is not
removed on receipt replay. A lost PVC DELETE response yields uncertainty until
the same guard and exact claim absence are read back.
The seal request carries the canonical Pod name already projected by the
Sandbox Runtime from its realization namespace and scope. The ledger uses the
existing Pod-to-PVC naming inverse to match that name against the original
Created claim ticket; re-encoding raw scope in the broker would address a
different Kubernetes object and must remain Unknown.

Unready policy, missing historical nonce, another Created or Isolated Kind,
foreign UID/spec/effect, a terminating claim before source reconstruction,
missing terminal authorization, or an unresolved request remains Unknown with
zero early Session receipt or Flow successor. The direct-process compatibility
gate does not borrow this brokered proof. Rich ConfigMap/Secret/Memory/control
plans and opaque remote execution remain explicit availability gaps. State,
handle, provider-ordering and mock-API cause/effect tables live beside their
tests; the real k3d case must prove the original PVC, guard name, late Create
409, UID-fenced deletion and response/receipt replay in one cluster.

The distributed-control deployment had selected signed/unbound Coordinator
transport at startup while its K8s Worker already forwarded Create to the
broker, making the first Sandbox request return 404. It now selects the
existing Kubernetes Worker transport before Ready. The Worker projects its
Pod-bound token/UID and has read/dispose but no direct six-Kind Create; the
Coordinator owns those Creates, the retained ledger in a separate control
Namespace and the original Worker stop evidence. Both deployments import one
shared RBAC Role source and the existing Create-isolation policy. The final
Native crash stage retains the same transport instead of changing authority
mid-run. Static render/role tests and real `kubectl auth can-i` decisions must
precede the public WorkUnit; a successful HTTP readiness response alone never
qualifies the broker route.

The Worker Pod UID is also one immutable Registry incarnation and one Create
ledger process binding. After local drain and quiescence, a Pod-bound Worker
uses the existing `PodProcessStopper` to request UID/resourceVersion-fenced
deletion before its process exits. It keeps liveness serving while retrying a
lost or unavailable deletion intent. The Coordinator still independently
observes every exact old container exit and commits the old Registry receipt
before releasing its finalizer; the Worker's DELETE cannot mint that receipt.
The distributed outage fixture therefore requires both Worker ordinals to
return with new Pod UIDs and zero in-Pod restarts.

Hard loss of a Kubernetes node is a different evidence case. A Node `Unknown`
condition and an external k3d Docker SIGKILL receipt do not by themselves give
the Sandbox provider a trusted, durable exit for each original Pod container.
In the observed three-node fault, Kubernetes returned synthetic `Unknown/255`
for agent, input-projector and memory-projector while the original Pod retained
`awaken.dev/create-result-guard`. The deployment test must retain that old
Session/PVC and may exercise later faults only through a separate Session. It
must never force-delete the guarded Pod or describe the new Session as recovery
of the old one. Automatic recovery of the original identity after this exact
hard-node fault still requires a provider-side terminal evidence source; no
timeout, Node condition, Pod 404 or test-harness Docker receipt is a substitute.


## Amendment 2026-10-10: release a never-attempted frozen preparation

A Flow WorkUnit may be cancelled before any Worker can claim its packaged
Environment. Its immutable Session remains Preparing even after the Run ends;
idle-only archive correctly rejects it. A cancelled Run is not authority to
terminate another activity, and the terminal-only command remains unchanged.

The existing profiled no-publication release may select one narrower Session
Application command. The same archive CAS reloads a Frozen WorkUnit baseline,
Preparing execution, a recorded creation time, no active activity or Running
interval, an Unmaterialized Environment, no realization lease or attempted
initial realization, and no attempted/active Resource or MCP participant.
Every accepted Event batch must already be complete. Missing, historical or
inconsistent evidence remains rejected. A competing realization or activity
CAS invalidates these preconditions on the next read. This is admission from
durable Session facts, never an old Run outcome or sampled Pod absence.

The command reuses the existing archive fence, idempotency receipt, lifecycle
fact, complete pending Resource projection and required verified cleanup. It
returns no publication receipt. Lost responses and unavailable cleanup retry
the same retained intent. Public idle-only archive, terminal-only release,
Repository publication and already-attempted physical recovery keep their
existing admission. The existing profiled route and local product adapter share
one command selection; no new wire route, queue, Session state or force-release
flag is introduced. Cause-effect tests cover each independent admission
refusal, accepted Event fences, cleanup failure/replay and the real profiled
route. Live product recovery must still retain the original Issue and root.

The pure never-attempted predicate lives on the existing `PersistedSession`
aggregate beside its activity and tombstone predicates. Session Application
remains its only release-admission consumer and rechecks it in the root CAS;
the value query cannot grant a command or certify cleanup. Repository-publication
owner, read and CAS failures share one existing error conversion instead of
four parallel matches. This consolidation preserves every refusal and retry
outcome without increasing the large application or protocol package budgets.

### Cross-process no-Create evidence for the same admission

The Cloud Worker exposed the remaining transport partition: a frozen,
never-attempted preparation can archive safely, but a cold terminal assignment
previously carried only Unmaterialized state. Runtime then reconstructed a
Create intent from the newer cleanup lease, and the container provider correctly
refused that invented predecessor as Unknown. Unmaterialized state alone remains
insufficient evidence, including for historical records and response loss.

The existing archive idempotency receipt is the durable admission witness. The
Session Application compares its exact canonical termination identity, owner,
Root, admission fingerprint and committed revision to the archived snapshot.
Only the receipt committed by the unattempted Frozen preparation CAS projects
`environment_creation_not_attempted` in the existing cold terminal assignment.
Absent/mismatched receipts project false; an unavailable receipt read fails the
assignment. This adds no persisted status, queue, receipt table or cleanup owner.
Archive mutation and realization projection share the same pure command-identity
module; neither orchestration module depends on the other for this observation.

The canonical termination fact id/type now live in the existing lifecycle-fact
contract, reused by Managed lowering and this receipt reader. Runtime consumes
that Control-authenticated, Root/lease-bound projection only with Unmaterialized
state and a Vacant owner. Any local Candidate, bound/Retiring owner or restore
stays protected. Preparation and Disposal reuse the existing definitive-absence
branch while every MCP, Artifact, Memory, checkpoint and Resource participant
retains the ordinary cleanup protocol. No provider 404, token count, Run end or
caller-supplied success flag substitutes for this committed no-Create evidence.
The realization contract owns this consistency check. Runtime reuses the
Session Environment's existing binding projection instead of maintaining a
second match over the same state variants.

### Expired cleanup-only affinity for the same no-Create admission

An initial cold cleanup claim can attach a Worker lease before source
preparation succeeds. If that Worker installation disappears, unconditional
logical-owner affinity then strands even the exact admitted no-Create root.
Lease expiry alone remains insufficient evidence for a foreign Worker.

The same terminal claim CAS may replace an expired foreign affinity only when
the existing archive receipt proves unattempted preparation, the Environment
remains Unmaterialized, and the cleanup aggregate still has its exact frozen
root-only Requested operation with no legacy completion or accepted preparation,
disposal or Repository-publication progress. The aggregate owns this narrow
request-shape observation; the application owns receipt and lease verification.
Live leases, unavailable receipt authority, unknown history, physical sources,
child targets and progressed cleanup retain the original affinity protection.

The current Registry check, lease epoch increment, root CAS and exact uncertain
claim compensation remain the sole assignment mechanism. The new Worker still
installs the same receipt-derived no-Create projection and must complete every
ordinary participant and canonical receipt. No direct lease edit, replacement
Root, provider lookup guess, cleanup acknowledgement or force-reassignment API
is introduced. Cause/effect tests cover expiry/witness/request-shape conditions,
response-loss replay and refusal without Root or Runtime effects.

## Amendment 2026-10-15: retain physical admission in the original Dispatch log

Whole-execution recovery requires an inventory of every admitted physical
attempt, not just returned or independently closed attempts. The existing
Dispatch operation log and its original cursor remain the authority. Each
physical Install or Replace appends `AttemptAdmitted` with the exact frozen
Run claim, Worker/provider binding and logical/Session Thread identities in the
same transaction that installs its physical slot. A failed operation append
rolls back admission; exact retries and refusals append nothing. Executor entry
remains downstream of this committed admission.

This extends the current operation feed and all three storage implementations;
it adds no queue, recovery state, receipt store or lifecycle owner. Existing
ordinary-return and independent-closure facts retain their meanings. Admission
is not stop evidence. Recovery joins admissions and closure evidence using the
exact original claim and binding, rather than treating an empty closure query
as a complete inventory. Historical absence of admission records remains
unknown and must not be backfilled as proof of historical containment.

Validation belongs to the shared binding lifecycle decision table (entry,
exact retries, foreign binding refusal and handoff) and durable transaction
failure/restart tests. This source change alone does not establish Flow successor
admission, complete historical recovery or deployed automatic coding acceptance.
