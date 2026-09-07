# ADR-0066: Session Service Binding and Realization

- Status: Accepted
- Date: 2026-08-02
- Current Worker-placement decision: [ADR-0075](0075-unified-managed-session-worker-execution.md)

## Context

Session desired state and physical effects require one durable owner, one root
CAS, and one phase protocol across local and remote topology. ADR-0075 later
removed the late Worker-input portion without changing those authorities.

## Decision

A Session has one aggregate root revision, one immutable finalized baseline,
one versioned Resource state, and one versioned MCP attachment set. Desired
state is normalized under `SessionApplication` and committed through the one
`ManagedSessionRepository` root CAS.

MCP desired state uses full replacement. Logical name precedence is Session
over Agent; selected targets must be unique. Physical realization is a phased,
lease-fenced protocol:

```text
Requested -> Realizing -> Active -> Draining -> Removed
                  \-> Failed
```

`SessionRealizationLease` owns the physical projection. Per-generation claims
and receipts bind the exact Session, attachment, generation, target, holder,
Worker incarnation, epoch, and expiry. The shared `drive_session_realization`
driver performs stage, activate, publish, acknowledge, drain, and failure for
both local and remote adapters.

## Durable structure

Frozen network restrictions use `SessionNetworkPolicy` in the Session contract
as their single algebra owner. `normalized` owns trimming, ASCII case folding,
deduplication and blank removal; `safe_intersection` consumes that canonical
form instead of repeating those rules. Profiled admission meets the Environment
policy with the request restriction before layout validation and root creation.
Two blank-only allowlists therefore freeze `None`, never `Allowlist([""])`.
The Host still projects the frozen fact into the existing provisioning contract;
it does not reconstruct or widen authority during realization or replay.

The public enum and wire format remain unchanged. The abstract set algebra is
proved in `SessionNetworkPolicyProof.tla`; production property tests and SQLite
root readback cover the concrete regression. This is not universal Rust
refinement, a DNS specification, or proof of provider network enforcement.

```text
PersistedSession
├── revision
├── baseline: Preparing | Frozen
├── resources: SessionResourceState
├── mcp: SessionMcpAttachmentSet
├── realization: Option<SessionRealizationLease>
├── execution
└── disposition
```

The scoped persistence envelope carries Workspace ownership beside the
aggregate. Protocol DTOs derive from durable state and do not become another
authority.

Environment binding receipts reuse the application's observed snapshot commit
boundary. Receipt identity is compiled first; the admitted replacement samples
one injected Clock value and preserves the maximum prior root update time.
Exact replay and rejected authority leave the durable root unchanged. Runtime
and coordinator composition obtain the binding sink from SessionApplication,
so replacing its Clock or repository also updates the installed sink. There is
no independent Environment wall-clock implementation or timestamp store.


## 2026-09-01 amendment: lease-only renewal

Initial assignment and desired-state progression continue to use the one
`begin -> stage -> activate -> publish -> acknowledge` realization driver.
Steady-state ownership proof does not. `RenewSessionRealization` asserts the
exact current `(owner, runtime incarnation, epoch, expiry)` and requests only a
monotonic later expiry. `SessionApplication` reads the Session without
executable refresh or credential migration, extends the aggregate lease and
same-epoch active MCP claim expiries, and commits one root CAS. Active MCP keeps
its realization id, Stage idempotency key, and publication acknowledgement.

```text
Worker heartbeat
  -> authenticated Session Work + Worker authority check
  -> RenewSessionRealization(exact lease, later expiry)
  -> Session root CAS
  -> Runtime renew_session_realization_lease
  -> continue the existing model/tool/Sandbox attempt
```

There is no `renew_existing_lease` branch in `begin`, no Worker-callable begin
route, and no renewal-driven FrozenSessionProjection rebuild or second phase
driver. A response-loss replay returns the same or a monotonic successor lease;
stale owner/incarnation/epoch fails closed. Independent Sessions are scheduled
earliest-expiry first with a fixed per-Worker concurrency bound. More cloud
Workers multiply capacity, but never remove this local backpressure.

Runtime also captures the existing Session slot identity before renewal I/O.
The exact lease tuple alone cannot distinguish removal followed by recreation
with equal fields. Success and failure responses must still name that same slot;
revocation checks it before cancelling Resource preparation and again after the
realization/lifecycle locks, before interrupting or retiring a projection.


When MCP Stage or publication overlaps a heartbeat, expiry advances but the
completed physical effect does not change identity. Activation validates a
bijection of receipt generations against current claims, reconstructs each
admitted request at its asserted older expiry, and reuses the complete exact
receipt verifier. Target, credential pin, holder, Stage key, realization id,
incarnation, epoch and logical generation remain checked. Two expiry variants
of one attachment cannot substitute for two attachments. The live root lease
remains mandatory; a receipt never extends authority by itself.

Activation commits any exactly reported pending Resource generation and then
returns Publish for the actual staged receipt, not an invented newer-expiry
receipt. Activation may supersede an older unacknowledged generation: filter
validated Stage receipts through the post-activation publication set and drain
the obsolete generation. Publishing every pre-activation receipt would make the
subsequent acknowledgement disagree with durable truth.
Runtime already authorizes this immutable request through its live
same-generation root lease. Acknowledgement accepts that completed generation
under the same monotonic relation; drains retain exact matching. Response-loss
replay preserves Idle/Running and does not repeat a root write. A different
owner/epoch, a future expiry or changed binding fails closed. An MCP-only Stage
cannot consume a pending Resource mutation, and a mismatched prepared revision
commits nothing. No new scheduler, store, wire field or phase driver is added.

Recovery overlapping a full MCP replacement can Stage both an old Active
generation and its successor. Activation may then move the old generation to
Draining. The committed aggregate's existing publication selector partitions
the validated receipts: only still-Active/unacknowledged generations publish;
obsolete generations use the drain lane. A lost activation response may be
retried with old receipts now backed by Draining claims. Those receipts still
pass the same complete binding/fingerprint verifier, but never regain Active
status or enter Publish. Required Stage receipts remain a bijection; unrelated
or duplicate generations do not become admissible. No replay registry is added.

The application decision table covers hot/restarted ownership, replacement plus
removal plus retention, heartbeat overlap, invalid old/foreign receipts and
exact activation replay through strict acknowledgement. The existing
`SessionRealizationMutex` model's publication configuration adds recovered
generations and activation-response loss to the same phase driver. Two semantic
mutations check obsolete publication and lost-response retry; reachability
witnesses exclude vacuous recovery/loss coverage. This bounded abstraction
assumes immutable generation bindings and eventual successful retry under weak
fairness; it does not prove fingerprints, arbitrary root-CAS contention,
unbounded generations, real transport or Rust refinement.

WorkerRelay retains the immutable staged generation, not a second expiry
authority. Its production admission reads the same Session slot root decision
as MCP effect admission: a live same-incarnation/epoch root may authorize an
older receipt, but an absent, expired or replaced root denies a relay request.
The pre-existing direct realizer assertion contract does not become a relay
fallback. Authority is checked before buffering, before outbound forwarding,
and after credential refresh; Session authority is never read while holding
the route map mutex. Exact route capability and the existing call fence still
own replacement isolation, cancellation and drain. Auth-challenged effectful
calls remain non-replayable. Host/relay fault tests cover these boundaries;
the phase-driver model does not model HTTP streams or provider-side effects.

The former expiry-equality catch-up branch could livelock under continuous
successful renewal. The application test's A4 interleaves renewal before every
phase response; F1-F6 retain the Resource 2-to-3 crossover cases. The existing
SessionRealizationMutex model now permits an infinite renewal cycle and checks
conditional release under weak phase fairness; its targeted configuration and
old-branch mutation expose the non-releasing cycle. This is a bounded abstract
model check plus executable application evidence, not Rust refinement or a
guarantee of progress through permanently failing physical/Control operations.

After a completed MCP drain, the process-local Removed tombstone owns no
remaining effects. A currently authorized retry may discard that tombstone for
an exact Stage replay or a strictly later lease with the same immutable binding,
then run ordinary Stage again. This is recovery, not renewal of missing effects:
the old receipt is not returned as fresh proof. Changed bindings, non-monotonic
renewals, live/Staging/Draining effects and a closed quiescence fence remain
protected by their existing checks.

## 2026-09-07 amendment: request-relative realization evidence

The durable `SessionRealizationLease` remains unchanged and is still the only
Session ownership authority. Runtime admission must not compare a Control
absolute expiry against an unrelated Worker wall clock.

Static responsibilities:

| Treatment | Canonical owner | Responsibility |
|---|---|---|
| Reused | Session root CAS and generation comparison | Persist ownership and authorize monotonic same-generation renewal |
| Reused | `AuthorityLeaseTiming`, `AuthorityProofStarted`, Local process clock adapter | Conservative elapsed-time arithmetic and platform sampling |
| Modified | Session Control responses and shared realization/terminal drivers | Carry positive response-relative evidence and retain the request-start observation |
| Modified | Existing Host slot and renewal scheduler | Authorize and publish under the same slot lock; share local/Worker due selection and physical refresh |
| Added value objects | Session contract `SessionRealizationLeaseReceipt` and `ObservedSessionRealizationLease` | Transient wire envelope and non-serializable local observation; neither is a store or another lease owner |

Dynamic order is request-start observation -> Control read/CAS and projection
I/O -> positive `remaining_ms` sampled in Control coordinates -> local elapsed
deduction -> guarded slot publication. The final publication validates all
fallible coordinates before writing the correlated Agent, Environment,
workspace, request-context, baseline and dispatch fields. Resource preparation
retains its existing cancellable staging owner. Renewal compares the exact
pre-I/O lease snapshot; replacement, closure, exhausted old proof or exhausted
response proof cannot be overwritten by a late response. Response time metadata
does not count as realization phase progress.

Both deployment topologies use the existing slot-based renewal lane; the local
application no longer scans durable Sessions through an MCP-only renewal path.
Due selection uses observed remaining time. The requested extension uses the
asserted Control expiry plus a renewal interval, never Worker wall time.
Control is required only when a lease is due. The independent physical refresh
lane still runs for retained execution trees when no live leases or Control
client remain. A closed slot is not an uninstalled legacy slot and cannot admit
an unfenced suspension.

Terminal preparation and disposal projections also reuse the same slot's
`authorize_terminal_cleanup_lease` decision. Disposal no longer repeats a
Worker-UTC expiry check after that request-relative proof. The aggregate's
immutable preparation and current-generation fence lowering remain unchanged;
there is no new timer, owner or persisted field. The adjacent R1-R6 table
separates absolute Control expiry from local proof lifetime, including fresh
proof with past Worker-UTC expiry, exhausted proof with future expiry, replaced
generation and closed/uninstalled slots. This synchronous projection is not a
grant across subsequent async work: Provider wall clocks and backend commit
fencing remain the separate unresolved boundaries described below.

This is a wire contract change, not a Lease rename: renewal responses include
`lease` and required positive `remaining_ms`; directives and terminal
assignments flatten the same envelope. New clients reject missing, zero or
malformed horizons rather than inventing a fallback.

Registered execution reuses the existing Worker runtime protocol range and
compatibility kernel, with current protocol v3. Manifest/dispatch envelope
versions remain v1. Registration rejects omitted/zero-based, old or otherwise
incompatible ranges before mutation; claim authority, heartbeat renewal and
application-owned terminal recovery reject pre-upgrade incompatible records.
The standard Worker advertises 1..3, retaining support for explicitly pinned
v1/v2 queued requirements without rewriting durable work. Legacy version-zero
requirements do not become explicit registered authority; the existing local
eligibility path remains unchanged.

The registration receipt declares the selected Control runtime version. A new
Worker accepts only the current version before Host factories or probes start.
Missing/malformed version metadata grants no execution but preserves a decoded
registration identity for the existing single bounded withdrawal path. Zero
registration TTL is rejected inside that same scope, not in the transport
decoder where rejection would discard the known cleanup identity. Drain,
quiesce and deregistration remain available for incompatible registrations.
This is fail-closed coordinated upgrade, not mixed-version rolling availability:
old Control replicas must stop serving before the new fleet is admitted.

Test decision tables reside beside the contract, application, Host and signed
HTTP tests. They cover elapsed/expiry boundaries, regressed observations,
response-loss progression, exact-generation races, closed-slot rejection,
delayed projection publication and malformed wire evidence. The existing
`SessionExecutionBinding` model covers registered and direct-local binding,
original lock/slot identity, revocation and guarded effects; its Session-expiry
bit does not prove the receipt adapter or all projection fields. Provider
`SandboxEffectFence` validation still has independent absolute-time boundaries:
full provider clock correspondence, live upgrade qualification, real cloud/local
fault qualification and real Windows/macOS qualification remain open. Passing
these tests or bounded models must not promote `process_tree_supervision`.

The existing Container creation retry source now captures the operation and the
two original Session slot lock allocations before cache preparation. Its
projection and physical execution now reuse the same
`SessionExecutionAuthority` implementation and the slot's `remaining_for`
decision under Worker -> Session guards. Its closed target enum distinguishes
the exact enrolled Environment required for execution from the immutable
operation required before provider creation. The previous Host-specific
slot/lease-checking closure has been removed. An absent, closed,
expired or replaced slot cannot supply a fence; neither can a changed operation,
owner, incarnation, epoch or regressed expiry. A zero Worker budget denies the
shared effect callback. Preparation checks run before and after its await, and
Container retries use that same captured source rather than looking up a new
slot by Session id. No persistent fields or lease owners are added. The
unfenced direct-provider creation entry remains unchanged.

The captured provider target also exposes the existing synchronous
`CurrentExecutionAuthority` port with the minimum current Worker/Session
budget. Both interfaces use one held-lock decision; neither captures a copied
deadline nor retains the Host or slot map. The common implementation is
platform-neutral; only scoped process enrollment remains Linux-gated.
Memory acknowledgement now uses this same authority. The existing
`CurrentExecutionAuthority::with_current_effect` method binds the assertion to
the captured operation/generation and supplies a current fence under the same
guards; its default rejects an execution-only grant. Container's fence-query
adapter delegates to that method rather than duplicating its identity checks.
The two Host callers (continuation release and terminal cleanup) capture this
provider target and forward it through `SessionEnvironment` to the Sandbox.
Local/Namespace and Container acquire their existing async Memory guard list,
then the shared `MemoryReconciliationAck` enters Worker -> Session -> ack guards
before recording/draining. There is no await within that callback. Local marker
authorization still uses the existing nonblocking filesystem-lock participant.
Empty evidence and cached same-effect success also require current authority;
Copy retirement remains idempotent and FUSE remains an ordinary disposal owner.

Reused unchanged: durable Session CAS, materialization ordering, Memory CAS and
retention, marker/removal identity, and the existing Copy/FUSE lifecycle. Modified:
the existing authority port, both Host call chains, Sandbox acknowledgement
signatures, and the shared ack kernel. Added: one feature-gated, explicit
revocable test fixture shared across provider tests; no production store, timer,
lease, registry, or alternative cleanup path. The acknowledgement no longer
compares Control expiry with Worker UTC. Disposal preparation now also uses
that same authority kernel: both Host callers capture the provider target and
forward it through the existing Sandbox preparation method. The existing
`require_for_disposal` gate checks exact Memory evidence and runs synchronous
provider preparation under one current guard; its independent UTC check and
Local's redundant forwarding helper are removed. Local/Namespace keep the
original marker participant. Container rechecks after control shutdown, the
writeback lock, each credential read/write response and before returning the
prepared fence, including empty Memory and cached writeback. Effectless
direct-provider disposal retains its explicit owner and the same physical
helper, without claiming Session authority. No new lifecycle or store is added.

Adjacent decision tables cover empty/missing/acknowledged/mismatched evidence,
past/future Control expiry, faulty callbacks and response loss. Local tests
cover new/cached participants under operation/generation replacement and
renewal. Container tests poll actual Pending lock/read/write-response futures
before revocation, renewal or replacement, and verify no subsequent credential
effect or successful preparation. Already-admitted broker commits remain
possible; exact writeback identity is retained for retry. Provider marker and
physical-cleanup clocks still require migration, so this does not establish
end-to-end cross-clock disposal or backend commit fencing.

The Host source decision table and barrier tests cover direct/registered
topologies, same-byte slot replacement, renewal and closure during Worker,
cache and provider waits. `SessionExecutionBinding` includes pre-Environment
provider projections in its existing lock/authority kernel; provider-specific
guard mutants and a pre-readiness reachability witness prevent execution-only
evidence from standing in for this branch. This bounded model treats a source
response as a synchronous projection, NOT a grant for later asynchronous
physical mutation. Its Memory extension represents mount-lock wait/release,
cancellation, current-authority check/commit, and cached acknowledgement replay
using the same authority transitions. Dedicated mutants bypass the wait, skip
authority on replay, or drain twice; reachability witnesses require actual ack
and replay. This is a bounded safety model, not async-lock fairness, Memory CAS
refinement, OS timing, or real backend availability evidence. Executable tests
poll a real pending mount-lock future before revoking authority, cover empty,
new and replay acknowledgements, and retain receipts after response loss.
The same model adds one admitted preparation wait/response and re-enters the
existing held-lock effect kernel after it. Three additional mutants bypass
the pending wait, response-time authority or cached-Memory authority; empty
and cached preparation reachability witnesses exclude vacuous checks. The
two-effect bound and abstract expiry remain explicit: this checks local
admission after a response, not cancellation or authorization of an in-flight
remote commit, unbounded retries or eventual dependency availability.
The independent Provider wall-clock checks and real backend effect-boundary
authority remain unresolved by this amendment.

Physical disposal now consumes the same current authority port. Reused unchanged:
Session preparation/authorization CAS, exact marker/UID identity, retained
cleanup participants and provider dependency teardown. Modified: both Host
terminal/checkpoint callers, the existing Sandbox disposal method, Local/Namespace
physical cleanup and Container runtime observation/removal. The former Memory
authorization helper moves to the existing provisioning authority contract as
`with_current_sandbox_effect`; acknowledgement, preparation and physical cleanup
reuse this single kernel. No new registry, clock, lease or lifecycle is added.

Dynamic order is captured current authority -> retained participant lock ->
current admission -> quiescence/observation -> re-admission -> physical/dependency
effects -> current completion. Local synchronous shredding, exact root deletion
and Removed publication execute inside the current guard. Container rechecks
after observation before deleting, after a removal response, after the Memory
lock and each teardown; staging release and cached/initial completion consume
the same guard. Failure or cancellation keeps the original participants for
retry. A committed remote delete remains committed even when its response is
lost or authority is revoked; a late response grants no next operation.

Adjacent Local 5x5 and Container 6x5 decision tables cover actual Pending
participant/control/observation/removal/Memory waits crossed with revocation,
replacement, exhausted current budget, renewal and cancellation. They check
zero next effects, retained bytes/guards, exact retry and cached-result denial.
The existing model shares one provider wait across fixed preparation/disposal
selectors; 26 semantic mutants and 12 reachability witnesses include disposal
and cached completion. Its two-effect bound does not establish ordering across
both aggregate phases, callback elapsed time or asynchronous remote commit
fencing. Provider UTC validation, cold reconstruction authority and real
cloud/platform qualification remain explicit open boundaries.

### Control-publication cleanup participant

The disposal preflight must not mistake caller cancellation or a concurrent
disposer for task completion. `SandboxControlPublicationSlot` remains the sole
process-local publication registry. Its closed state now retains the original
participant until join and synchronous cleanup finish; repeated close retrieves
the same participant, and stale release cannot forget a closing task.

Reused unchanged: Session authority, physical provider disposal, Container channel
transport/retry, Namespace codec and bounded request JoinSet. Modified: the
existing publication slot and both provider adapters. Consolidated: the two
identical cancellation/JoinHandle implementations into
`SandboxControlPublicationTask` in the existing Sandbox control contract. There
is no additional registry, durable state machine, supervisor or systemd service.

Dynamic order: reserve -> install the task before asynchronous channel/socket
setup -> readiness response -> serving. Dropping an unfinished publisher or a
published handle requests cancellation; a best-effort background join releases
the exact reservation only after completion. Explicit dispose closes admission,
joins that same participant, then performs/finishes cleanup in the original slot.
If a disposal future is cancelled, another caller can resume the retained join.
Task start is rejected after cancellation. Namespace listener teardown removes
its recorded inode while the descriptor is still alive, including when the slot
is closing; the completed slot cannot repeat this cleanup against a later socket.
Normal Namespace shutdown cancels and drains request children before returning.

Decision tables live beside the contract/provider tests. They cover overlapping
registry disposers, cancellation/retry, late task start, first-channel setup
cancellation, handle Drop, task panic, and a real Unix-socket request being drained.
The existing `SandboxControlPublication` model adds retained cleanup, two callers,
task start/exit/join, Drop and cancellation/retry. Its safety and conditional
liveness checks have semantic mutants and reachability witnesses in the existing
strict gate. Eventual completion assumes task exit and a fair retrying disposer;
this does not prove OS process-tree termination, Rust/Tokio refinement, remote
backend cancellation, or survival of a stopped runtime. Without a live runtime,
Drop cancels but does not claim cleanup; explicit disposal retains responsibility.
The provider authority-clock and live cloud/platform qualification gaps above
remain separate from this process-local lifecycle fix.
Fenced Local and Namespace disposal now borrow the original prepared
`RemovalGuard` under the existing slot's async mutex rather than taking it out
across I/O. Namespace authorizes before closing and joining control publication,
then revalidates before resource release. The same existing teardown helper
checks authorization after acquiring the Memory-list lock, before each mount,
and after the last mount before clearing the complete set. An operation already
admitted may finish after expiry; that completion does not authorize the next
operation. Denial, failure and cancellation retain the same participant and
guards for an exact authorized retry. Legacy disposal and ownerless compensation
reuse this helper without claiming aggregate authority.

Terminal checkpoint cleanup borrows the same participant and keeps the existing
upload journal. Removed publication changes the in-memory marker only after the
durable write succeeds; a lost acknowledgement is replayed against the exact
intended tombstone, never a foreign value. These are extensions to the existing
Provider and marker, not another store, cleanup service or lifecycle.

Adjacent decision tables exercise competing disposers, cancellation during
control/Memory/checkpoint waits, expiry across lock and per-mount waits, partial
teardown, and lost marker/checkpoint acknowledgements. `LocalProcessRecovery`
composes two callers, retained marker ownership, repeated mount admission and
physical removal/terminal acknowledgement in the existing boundary model.
Its fresh-fence bit assumes truthful Provider authorization; it does not prove
Control/Worker clock correspondence, in-flight backend commit fencing, kernel
termination or cross-platform deployment. Those remaining qualifications must
not be inferred from model checks or successful local retries.

## 2026-08-12 amendment

Session creation no longer waits for Worker-authored input. The complete
creation command contains Environment, model/runtime identity, mounts,
environment values, prompts, Resources, network policy, and initial MCP
candidates. Finalization consumes that command before WorkQueue projection.

The removed late-input path is not a compatibility mode. Existing realization
leases, MCP generations, root CAS, and recovery remain unchanged. Worker
placement and the relationship between Work leases and Run claims are now owned
by ADR-0075.

## 2026-08-13 amendment: application MCP credential admission

A trusted application that contributes an authenticated MCP endpoint
must author that bearer through the existing Credential/Vault aggregate before
it creates the Managed Session. The identity is the exact
`(Workspace, application authority, normalized MCP target)` tuple. Control
derives stable opaque Vault and credential-source ids; callers cannot supply a
second identity or persist an application-local credential mirror.

The command reuses the Managed `Idempotency-Key` contract. Its payload is
write-only and the durable credential row remains secret-free. A replay of the
same key and payload returns the same Vault id, source id, and revision. A new
key may rotate only the material revision in place; it cannot change the tuple
or create a parallel desired-state source. The existing credential mutation
intent and source CAS select one concurrent winner. Conflicting tuple state,
idempotency-key reuse with another payload, and stale concurrent rotation fail
with `409`.

Static ownership remains:

```text
application -> Managed credential command (wire only)
                   -> VaultState (normalization/projection)
                   -> CredentialRepo + SecretStore (identity, CAS, custody)
                   -> SessionApplication (exact source/revision pin only)
```

Dynamic order remains one-way:

```text
POST /v1/config/application-mcp-credentials
                        -> receive stable vault/source/revision
                        -> POST /v1/sessions with that vault id
                        -> normalize MCP target once
                        -> pin the exact source revision
                        -> realize the existing MCP generation protocol
```

Failure before Session creation creates no Session. A lost credential-command
response replays the same key. A lost Session response replays the Session's own
independent idempotency key. Neither failure falls back to an embedded/local
credential or a second Session implementation.

## 2026-08-16 amendment: authenticated MCP credential delivery

Authenticated MCP freezes one delivery mode into each exact attachment
generation. The two supported intents are:

```text
ClientInjection   -> an explicitly authorized MCP client process receives the
                     credential through its declared private delivery field
GatewayMediation  -> an installed egress mediator receives the credential
                     reference and the workload sees only a mediated route
```

The mode is selected before materialization from the exact plaintext holder and
installed realization mechanism. It is never inferred from route availability
and never changes as a runtime fallback. `ClientInjection` accepts only an
authorized Workload holder combined with `ProcessProtocolField`: material is
placed in the process-private ACP `session/new` field and never represented as
an OS environment variable, private file, durable ACP configuration, or URL.
`GatewayMediation` accepts only a Platform holder combined with `PlatformRelay`.
Historical environment/file/relay/provider wire values remain readable but are
not reclassified into either mode.

The Session aggregate remains secret-free. Stage receipts must match the exact
generation binding, selected holder, realization mechanism and delivery mode
before activation. Deployment-specific egress mediation is supplied through
the existing realization port and cannot become another desired-state owner.

## Static boundaries

| Boundary | Owns | Rejects |
|---|---|---|
| Session application | normalization, authorization edge, root commands | direct repository writes from adapters |
| Session repository | CAS, idempotency, outbox, recovery scan | protocol status authority |
| MCP attachment set | desired generations and transitions | plaintext material |
| realization driver | ordered physical phases | desired-state authoring |
| protocol adapter | wire mapping | independent merge/update semantics |

## Failure and retry

- normalization conflict fails before root insertion;
- a stale root revision or realization lease commits nothing;
- stage-before-CAS failure records the existing retry state;
- stale physical receipts are disposed or rejected and cannot publish;
- response loss replays the same command/generation identity;
- terminal Session cleanup fences admission and drains/removes physical state.

## Consequences

Resource, MCP, environment, and execution state remain separate value/state
objects under one Session root. Local and remote realization share one phase
protocol, and no adapter carries a second desired-state registry.

## 2026-08-24 amendment: hosted Session identity and credential adoption

An application that must issue a Session-bound credential before Session create
uses the Managed adapter's exported
`managed_session_id_from_idempotency(owner_scope, key)` function. The public
create path calls that same pure function; there is no copied formula or
metadata lookup identity. This predicts only the opaque address. The scoped
Session repository remains the payload-match and replay authority, so the
application still performs direct `GET /v1/sessions/{id}` and, only on `404`,
the ordinary idempotent `POST /v1/sessions`.

The application MCP credential receipt now carries the existing closed
credential-adoption state, `converged | pending`. Creation has no predecessor
generation and is converged. Rotation synchronously attempts the exact durable
outbox event through the already configured `ManagedCredentialRolloutTarget`.
Only target convergence followed by the repository's exact-event
acknowledgement returns `converged`; an absent target, a busy Session, or target
failure returns `pending` and leaves the same event for the existing supervised
reconciler. Command responses read only the existing primary event id; full
outbox enumeration remains exclusive to that reconciler and is never amplified
per HTTP replay. A hosted application must wait/retry on `pending` before
starting a Run that requires the new bearer.

The write command also carries a required positive, caller-monotonic
`credential_generation`. The existing deterministic material reference records
that generation beside the command-key fingerprint and before its existing
writer-attempt fence; no receipt row or counter is added. A legacy reference is
generation zero and may be upgraded by the first positive generation. Only the
exact current `(generation, key fingerprint, sealed material)` replays. A
strictly newer generation uses the existing WAL/CAS rotation; an equal
conflicting or delayed older generation fails before writing. This narrows the
earlier “new key rotates” statement: a key change alone is never authority to
restore an older bearer.

This adds no Session mapping table, metadata identity, credential store, route,
scheduler, lease, compatibility fallback, or second rollout vocabulary. The
dynamic order is:

```text
derive canonical Session id -> direct GET
  404 -> issue Session-bound MCP bearer with positive monotonic generation
       -> enter/replay/rotate existing Vault source
       -> receipt pending: retry after existing Idle-only adoption
       -> receipt converged: idempotent Session create or replay
  200 -> reuse the exact durable Session
```

## 2026-08-27 amendment: immutable post-create mutation authority

### Decision and static structure

`SessionBaseline.mutation_policy` is the one durable authority for which
Agent/Resource authoring operations may change a Session after creation. The
closed `SessionMutationPolicy` value is selected in the complete
`ControlSessionCreationInputs`, fingerprinted into every non-Managed baseline,
and frozen by `SessionCreationIntent::finalize` before the first root insert.
No protocol, product adapter, metadata field, or current caller identity may
reinterpret it later.

| Policy | Creation owner | Post-create authoring admitted |
|---|---|---|
| `Managed` | ordinary Managed Session creation and historical rows | the existing public Session Agent update, complete Resource-manifest mutation, and Repository credential update paths |
| `Frozen` | profiled WorkUnit and built-in fixed-purpose Sessions | none of those Agent, Resource, or Repository-credential mutations |
| `FileResources` | profiled interactive Sessions | only File binding attach, replace, or delete through the ordinary item-level Resource API; every non-File input and exact Skill pin remains equal |

Title, metadata, budget, Events/Runs, archive, and delete remain owned by their
existing Session commands and state machines. They do not rewrite the baseline
and are not reclassified as Agent/Resource authoring by this policy.

```text
ordinary Managed create ------------------------------> Managed
private profiled mode: WorkUnit ----------------------> Frozen
private profiled mode: Interactive -------------------> FileResources
                                                            |
complete create command -> SessionCreationIntent::finalize  |
                         -> immutable SessionBaseline <-----+
                                      |
public Agent/Resource command --------+-> policy gate -> existing root CAS
Vault lifecycle event -> SessionMcpUpdate::CredentialLifecycle
                                      `-> bounded existing MCP generation path
```

The existing `SessionUpdateCommand`, Resource manifest command, and Repository
credential ingress remain the command owners. `SessionMutationPolicy` adds no
repository, route, lifecycle, generation, or realization mechanism.

`SessionMcpAttachmentSet::desired_attachments()` is the sole selector for the
MCP desired set used by creation, typed updates, comparison, and realization.
Public replacement and credential lifecycle commands do not maintain another
filtered view or choose current MCP truth independently.

### Credential lifecycle is not public re-authoring

The Managed adapter lowers a public MCP desired-set change to
`SessionMcpUpdate::PublicReplacement`. Only `Managed` admits that operation.
The existing Vault rollout instead uses the distinct typed
`SessionMcpUpdate::CredentialLifecycle` command, so an immutable profiled
Session cannot prevent an authorized credential rotation or revocation.

That lifecycle command is deliberately narrower than public replacement. It may
replay or monotonically advance the same source revision, or remove only the
attachments affected by revocation. It cannot add an attachment, change name,
target, prompt exposure, origin, or an unrelated credential, and it cannot be
combined with title, metadata, budget, or tool mutation. The Vault child's
`ManagedCredentialLifecycle` and its WAL/CAS remain the credential authority;
the Session command only adopts the resulting exact MCP generation.

Repository credential replacement is product authoring rather than rollout
adoption. It is admitted only by `Managed`, and a profiled Session rejects it
before Vault ingress or another external effect.

### Dynamic behavior and failure boundary

```text
create
  -> receive the complete Agent profile, direct Resource attachments,
     Repository inputs, MCP candidates, Environment, tools, and policy
  -> resolve and normalize once
  -> finalize baseline + initial Resource/MCP truth
  -> atomically insert one root and its IdempotencyRecord

post-create Agent/Resource request
  -> load the frozen baseline and current Resource/MCP truth
  -> classify the typed mutation against SessionMutationPolicy
  -> denied: return the existing typed bad-request owner; no Vault, catalog,
     realization, Runtime, or root-CAS effect
  -> admitted: reuse the existing command, root CAS, realization, and receipt

credential rollout
  -> receive one exact Vault lifecycle event
  -> validate same-source monotonic rotation or exact affected revocation
  -> reuse the existing MCP replacement/generation/realization path
  -> reject topology or unrelated-credential widening before root mutation
```

The private whole-manifest extension cannot be used to complete or mutate a
profiled Session after creation. `FileResources` changes enter through the
ordinary item-level File verbs, whose lowering produces a complete candidate
manifest; the Session application then proves that only File bindings changed.

Policy failures reuse the existing `SessionUpdateError::Rejected`,
`SessionResourceManifestError::Rejected`, or
`SessionPreparationError::Rejected` owner and its neutral `RunError`. No new
wire error union or adapter-owned authorization state is introduced.

### Repository receipt identity remains atomic

The existing `ManagedSessionRepository` remains the only classifier for a
Session id, owner scope, tombstone, aggregate, and operation receipt. Its create
transaction writes one revision-1 receipt with the new root and returns
`SessionCreateResult::{Applied, Replayed}`; the application never decides replay
from metadata or from separate owner/receipt/aggregate reads. The same atomic
snapshot is exposed for deterministic preflight through `replay_create`.

A create receipt must name committed revision 1 even when an exact replay later
returns a more advanced current aggregate. A mutation receipt must name revision
2 or later and exactly equal that command's expected next revision. Reusing a
receipt across an operation or expected revision is
`IdempotencyMismatch`. A receipt ahead of durable truth, a dangling receipt, or
simultaneous live and tombstone identity is `Corrupt`; the application maps that
to its typed internal failure and performs no mutation or external effect.

### Compatibility and maintenance cutover

An absent policy decodes as `Managed`. `Managed` remains omitted when serialized
and retains the historical baseline fingerprint algorithm and ordinary Managed
wire/update request fingerprints. Non-Managed fingerprints are domain-separated
by the exact policy. Historical profiled rows are therefore not guessed or
silently tightened; they remain `Managed`, and products that need a stricter
guarantee create a new post-cutover Session.

The baseline independently freezes the Managed request's exact
`SessionSystemPromptSelection::{Inherit, Clear, Replace}`. `Inherit` is the
omitted legacy default and retains the historical fingerprint; `Clear` and
`Replace` are explicit, domain-separated authoring facts. A later GET resolves
that frozen choice against the pinned Agent publication, so an explicit clear
cannot be mistaken for inheritance. This is baseline projection through the
existing realization contract, not another mutation policy or Runtime Host
authority.

The first non-Managed row and atomic profiled receipt are a new beta aggregate
contract. A default lets the new reader consume old Managed rows, but old
binaries cannot safely read and rewrite the new field. Reuse the existing
ADR-0075 beta maintenance cutover: close Flow/public admission, drain and stop
every old Coordinator writer, deploy the new readers/writers, complete the
canonical lifecycle-supervisor validation, and only then create profiled
Sessions and reopen admission.

This cutover is full-stop and forward-only. It does not adopt or backfill an old
profiled metadata fingerprint or its default aggregate-payload create receipt.
After upgrade, retrying such an old deterministic profiled identity returns the
typed HTTP 409 conflict; Flow creates a new post-cutover identity. Ordinary
public Managed metadata-backed replay remains unchanged. There is no
rolling-write compatibility mode, guessed receipt equivalence, or Cloud-side
migration owner.

### Cause-effect decision table

| Rule | Frozen policy | Mutation cause | Effect |
|---|---|---|---|
| P1 | absent / `Managed` | ordinary public mutation | preserve legacy fingerprint/wire and reuse the existing command |
| P2 | `Frozen` or `FileResources` | public tools or MCP replacement | reject before projection or root mutation |
| P3 | `Managed` | valid complete Resource replacement | reuse the canonical Resource CAS and realization |
| P4 | `Frozen` | any new Resource replacement | reject before Resource/Vault/realization effects |
| P5 | `FileResources` | only File bindings differ | admit the ordinary item-level operation and canonical manifest CAS |
| P6 | `FileResources` | non-File input or exact Skill pins differ | reject without durable or physical change |
| P7 | non-Managed | Repository credential replacement | reject before Vault ingress or Repository effects |
| P8 | any | valid typed credential rotation/revocation | adopt only the exact affected MCP generation |
| P9 | any | lifecycle command widens topology or an unrelated credential | reject before root mutation |
| P10 | new profiled create | complete inputs valid | freeze policy and all desired inputs in one original root insert |
| P11 | Managed create | system prompt is omitted | freeze `Inherit`, omit the field, and retain the historical fingerprint |
| P12 | Managed create | system prompt is explicitly cleared | freeze `Clear`, return no inherited prompt, and use a distinct fingerprint |
| P13 | Managed create | system prompt is replaced | freeze `Replace(value)`, return that exact value, and use a distinct fingerprint |
| P14 | post-cutover profiled retry | only old metadata/default-payload identity exists | return typed 409; do not adopt, backfill, or perform effects |
| P15 | any receipt replay | receipt is dangling, ahead, or paired with two identities | return typed internal corruption; do not mutate or perform effects |

## 2026-08-29 amendment: one effective publication and request-only Session context

### Whole-system decision

The Session aggregate continues to own durable desired state; Agent publication
continues to own executable policy; Thread commits continue to own transcript
truth. Their only Runtime seam is one complete `FrozenSessionProjection` and one
backend-neutral `NormalizedModelInput`. Local realization and claimed Worker
recovery may transport or validate these values, but may not reconstruct an
Agent override, install individual projection fields, or append derived context
to a Thread delta.

### Static structure

| Classification | Authoritative owner | Decision |
|---|---|---|
| Reused unchanged | `ManagedSessionRepository`, `SessionBaseline`, Resource/MCP generations, `ThreadCommit` | remain the only durable authorities |
| Modified | `FrozenSessionProjection` and `SessionRuntime` port | carry the exact effective publication and install the complete projection through one operation |
| Modified | Runtime request context and Native/ACP/A2A executors | consume one normalized ordering: frozen instructions, current request-only context, durable input |
| Modified | Coordinator Dream exporter | reuse the same visibility rule while preserving tool payloads and durable ordering |
| Added | pure `project_effective_agent_publication` and content-free `ModelInputProjectionTrace` | one lowering rule plus auditable counts; no repository, state machine, fallback, or feature flag |

Resource, MemoryStore, Skill, transcript-prefix, and automatic-memory recall
prompts are rebuildable request context. Stable content-addressed message ids make
an attempt replay idempotent. They are never inserted into `RunActivation.input`
and therefore never enter `ThreadCommit.new_messages`. Historical messages whose
reserved id starts with `session-baseline:` remain in append-only Thread truth,
but the shared normalized projection hides only those records from every new
Native, ACP, A2A, and Dream model request. An explicit protocol-authored System
message with an ordinary id remains visible.

The Managed protocol adapter contains no Dream-specific availability or metadata
state machine. Dream lifecycle remains owned by the Dream application and uses
the ordinary Session/Run realization boundary.

### Dynamic behavior and failure boundary

```text
Session root snapshot
  -> Coordinator resolves one baseline Agent publication
  -> pure effective-publication projection applies model/inference/prompt choice
  -> complete FrozenSessionProjection crosses local or Worker boundary
  -> Host validates every immutable coordinate and prepares fallible Resources
  -> one logical slot publication; optional realization lease is installed
  -> each attempt snapshots current Resource/MemoryStore/Skill context
  -> NormalizedModelInput filters only historical derived ids
  -> Native structured messages OR ACP/A2A text envelope
  -> ThreadCommit appends only durable activation input and model/tool results
```

Missing or mismatched pinned publication, conflicting baseline/environment, and
failed Resource/Skill preparation fail before a new logical Session projection is
published. A Resource revision replacement is visible only to attempts that
start after that complete revision; an attempt that already captured the old
context remains immutable. Projection audit logs record backend, Run, Thread,
publication id, supplied/visible counts, and filtered count, never content.

### Cause-effect rules

| Rule | Cause | Effect |
|---|---|---|
| C1 | `Inherit`, `Clear`, or `Replace` system-prompt choice | the one pure publisher returns the exact effective snapshot and fingerprint |
| C2 | current derived Session context | visible for this request, absent from every new Thread delta |
| C3 | historical `session-baseline:*` record | retained durably, hidden from every new model request |
| C4 | explicit ordinary-id System event | retained durably and visible |
| C5 | Resource generation R1 is replaced by R2 | a fresh attempt sees only R2; an already captured R1 attempt does not mix revisions |
| C6 | invalid publication or Resource preparation fault | no partial new baseline/workspace/manifest/prompt/lease projection |

## 2026-09-06 amendment: repository receipt is the post-create effect gate

The existing `ManagedSessionRepository::create` result remains the only create
authority. The Session application now routes that typed `Applied | Replayed`
receipt and the existing synchronous/asynchronous completion choice through one
closed `session_create_effect_plan` kernel. Only `Applied + AwaitRealization`
may continue into realization, activation, and disposable Work projection.
`Applied + AcceptDurableRoot` returns the complete root and wakes the existing
recovery supervisor. Either replay returns repository truth and performs no new
post-create effect; an awaited `ActivationFailed` replay retains its typed
terminal conflict.

This change extracts the existing policy from control flow for direct
production use and bounded proof. It adds no create record, job store, phase,
repository, scheduler, or compatibility path. Database atomicity, physical
realization, and the wake delivery remain covered by their existing adapter and
deployment boundaries rather than being claimed by the small receipt kernel.
