# ADR-0065: Recoverable Embeddable Remote Worker

- Status: Accepted
- Date: 2026-07-23
- Current execution decision: [ADR-0075](0075-unified-managed-session-worker-execution.md)

## Context

Remote execution must remain recoverable and embeddable without moving Control
truth into a Worker or creating a product-specific execution stack. ADR-0075
later narrowed the Session portion of this decision to frozen-truth-only Worker
realization.

## Decision

Awaken exposes one recoverable Worker runtime. Control owns registered Worker
identity, durable dispatch, exact claim/epoch authority, committed recovery
truth, idempotent commits, and completion. A Worker owns only its leased attempt
and local physical effects.

The runtime supports Native, ACP, and A2A attempts through one
`RunAttemptExecutor` registry. An embedding application may decorate that
neutral attempt boundary to adapt product envelopes and results. It cannot
replace Worker registration, dispatch, recovery, commit, Session realization,
or backend selection.

The validated registration manifest also owns the process execution capacity.
`WorkerNode` passes its positive `max_concurrent` with the existing dispatch
transport into the Host pool. A CPU quota bounds CPU consumption; it must not
silently replace an explicitly configured concurrency for I/O-bound Session
preparation. Unregistered local Hosts retain their CPU-derived default, and
Coordinator-only Hosts still create no execution pool. Registry placement and
the existing fixed-size pool enforce their original boundaries; this projection
adds no queue, scheduler, lease, or second capacity configuration.

Standalone bootstrap, embedded CLI Workers, examples and Scenario Host select
standard Session Resource support through
`WorkerNodeBuilder::with_remote_session_resource_support`. Registration then
supplies the exact Worker upstream identity to the existing HTTP Memory adapter
and Copy mounter; WorkerNode owns the other per-kind Resource clients. The CLI's
duplicate `registered_memory_mounter_factory` export is removed: callers migrate
to that builder method. The generic registration-bound factory injection remains
available for genuinely custom adapters, not as a standard parallel composition.
No registry, Memory store, lifecycle or timing protocol changes in this migration.

```text
Control                                      Worker
  register/heartbeat <-------------------------+
  enqueue -> claim ----------------------------> verify exact epoch
  committed recovery view --------------------> execute selected backend
  claimed idempotent commit <------------------ result/state
  settle + completion tombstone <-------------- terminal attempt
```

The following invariants remain authoritative:

- execution is at least once; committed logical operations are idempotent;
- a stale owner, epoch, or expired lease cannot perform effects or commit;
- Worker recovery reads committed truth and never reconstructs it from a
  process-local cache;
- a completion tombstone prevents Run resurrection;
- storage adapters implement the same logical coordinator protocol;
- Worker code has no direct access to Control persistence.

## 2026-08-31 amendment: registry proof and Run leases stay independent

### 2026-09-07 clarification: environment evidence is not heartbeat I/O

`WorkerSupervisor` runs registry renewal and sandbox evidence monitoring as two
single-flight futures under one incarnation supervisor. Neither future waits for
the other's remote request. Either terminal proof loss cancels the other future
and enters the existing irreversible local drain; no second registry, admission
state machine or recovery worker is introduced.

Initial sandbox evidence is mandatory before Ready and is bounded by the
registration-derived request budget. Its local validity window uses the same
conservative `AuthorityLeaseTiming` policy, frozen from the initial applied
heartbeat, independently of subsequent registry renewals. Evidence success is
anchored at probe request start, not response time. A probe timeout leaves that
deadline unchanged and retries only before expiry. Explicit provider errors
remain fail-closed: `SandboxError` currently does not distinguish transport
failure from a violated NetworkPolicy, so errors must not be guessed transient
from their message text. Actual evidence expiry still stops execution even if
registry renewal succeeds. This is bounded tolerance, not indefinite reuse of
cached security evidence.

The decision tables live beside Worker lifecycle and paused-time tests.
`WorkerAuthorityEvidence.tla` checks independence, absorbing expiry, and eventual
closure without successful probes under fair time advancement. The separate
`WorkerDrain.tla` remains the owner of local-fence-before-registry-drain ordering.
Neither model proves absence of OS scheduling pauses or instantaneous physical
cancellation of an external model request.

### Registry renewal protocol

Control returns the positive Worker-registry TTL in registration and every
applied heartbeat receipt. The Worker derives its cadence, request bound, retry
delay, and conservative proof deadline from that value. It does not duplicate a
deployment constant or treat one uncertain HTTP response as authoritative loss.

```text
applied heartbeat
  -> refresh local proof deadline
  -> keep claim admission open

transport error or timeout while prior proof remains
  -> suspend only new Pool claims
  -> retain in-flight Run execution and each exact RunClaim renewal
  -> retry with the next heartbeat sequence
  -> Applied resumes admission
  -> explicit registry rejection or proof expiry drains and cancels
```

The Pool has one admission state machine: `Open -> Suspended -> Open` is the
reversible uncertainty path, while either live state may enter the absorbing
`Draining` state. Suspension never cancels an in-flight Runtime token. Drain
does, after the existing bounded cleanup. Every Run continues to use the one
epoch-bearing `renew_lease` authority from ADR-0024; the removed owner-wide
renewal route is not a compatibility path.

Static ownership remains separated: `WorkerDirectory` owns the durable registry
lease, `DispatchQueue` owns exact Run leases, `DispatchPool` owns only local
claim admission, and `ThreadCommit` remains execution truth. Metrics expose
heartbeat outcome/duration, scheduling lag, proof time remaining, and bounded
loss cause without Worker, Run, prompt, or tenant labels.

Registry proof or Run-lease loss cancels the old Runtime token, but cancellation
is not physical quiescence until the owned executor Future returns. The signed
Worker transport begins and finishes the exact Dispatch attempt slot defined by
ADR-0024. A replacement Worker may acquire the new Run claim while it waits, but
cannot enter model, tool, or Sandbox execution through that Dispatch. If the
prior process vanished without an ACK, process/Pod termination proves only its
process-bound tool or Sandbox work stopped. An opaque remote model request also
requires an authoritative provider cancellation or terminal receipt when it can
continue external effects independently; registry TTL alone is insufficient.
Native response-only inference uses the same evidence rule from ADR-0024:
discarding a response prevents proposed tools from executing, but does not prove
provider computation or billing stopped. Cancellation retains the physical veto
even after the local executor returns; it does not permit an exact ACK. Hard
process loss likewise leaves the physical slot occupied.

Targeted crash recovery closes Registry authority through
`WorkerDirectory::expire_incarnation(identity, now_ms)`, not a read followed by
unconditional deregistration. The existing registry transaction checks the exact
incarnation and the current deadline using the same half-open lifetime as
heartbeat and replacement. A renewed live Worker is unchanged; an expired exact
Worker becomes Dead. Exact closed records acknowledge retries without mutation,
and a replacement cannot be affected by the predecessor's late request. The
unused broad expiry scan is removed rather than retained as another path.
Memory, SQLite and PostgreSQL reuse their existing mutation boundary and one
contract decision kernel; no schema, extra epoch, timer or recovery queue is
added. Kani covers the complete scalar decision, Loom races renewal/expiry/
replacement, and shared adapter traces include independent database connections.
This closes execution authority only: provider stop, remote-participant evidence
and Dispatch retirement still require their separate checks and composition.

Aborting or dropping the outer executor Future is not an ACK. The physical slot
remains occupied until the Future explicitly returns, and uncertain external
tool cancellation vetoes the ACK even after local return. No timeout or destructor
fabricates that terminal fact.

ACP retains the current `AgentSession` (process handle and duplex channel) in
its original executor scope after a bounded cleanup failure. The Step outcome
may commit once, but no next Step or queued steer is consumed while cleanup is
pending. That same scope retries the existing bounded Supervisor cleanup and
drains trailing output until the exact handle observes exit; only then can the
outer executor return and the existing Dispatch owner consider an ACK. An
agent-originated `Cancelled` or `TimedOut` frame is not a process exit receipt.
Dropping the scope before observation adds the monotonic quiescence veto;
recovering this process never clears an independent child or remote-effect veto.
This is live-owner recovery, not a detached cleanup service or a durable recovery
queue. A historical slot with no frozen execution binding and no surviving
observer remains unknown: current container inspection cannot reconstruct its
original whole-attempt evidence, and neither SQL edits nor late binding backfill
may release it. Cross-physical-host process loss, partition and reboot acceptance
requires dedicated hosts and provider evidence; local duplex, process or
container tests do not satisfy that qualification.

On graceful Worker shutdown, local admission closes before the configured drain.
If that budget expires (including a configured zero), the existing authority-loss
cleanup cancels the resident attempt through its installed claim and Runtime
signal, observes a bounded return window, then revokes only local Session
projections. The existing root authority closes synchronously before async
cleanup, and the originally selected closed revocation cohort is retained through
finalization. It must not call the Coordinator-only global queue operations:
Worker transport cannot list/requeue/cancel those rows, and local shutdown must
not become a new durable user cancellation authority. AllInOne must continue polling both HTTP surfaces
throughout that cleanup: keeping an unpolled listener bound cannot serve the
Worker's cancellation or exact return receipts. One shared shutdown composition
owns this dependency for signal, Worker completion and process-task failure.
Opaque work without return remains fenced after cleanup expires. Prompt SIGINT
and hard SIGKILL do not gain a physical completion guarantee from this policy.

A committed A2A `Accepted` operation supplies a narrower continuation proof.
When its complete Run-scoped `TaskReference` matches the immutable remote
endpoint, the A2A leaf classifies recovered execution as continuation-only: it
reattaches with `tasks/get` and cannot submit `message:send` again. The Worker
then asks the one Dispatch aggregate to atomically hand its physical slot from
the predecessor to the exact current claim. A `Prepared` operation has only the
pre-send identity and no accepted task; it therefore cannot hand off, resend, or
use an older predecessor task as completion evidence. Every other executor and
every incomplete/mismatched operation keeps the fail-closed quiescence rule.
Control still owns the slot, claim, and transaction; the Worker contributes only
the executor-specific proof.

## 2026-08-12 amendment

The former late Worker-authored Session-input design is superseded. Managed
Agents already supplies custom/local Worker placement through the Environment
WorkQueue. ADR-0075 therefore makes the complete Session command a pre-creation
input and limits Worker Control to realization of frozen truth.

This ADR no longer owns Session authoring or placement. Its remaining scope is
Worker lifecycle, Run attempt fencing, recovery, execution, commit, and
embedding through the attempt decorator.

## Static ownership

### Pod-bound transport adapter (2026-09-09)

The optional `awaken-worker-transport-security/kubernetes` adapter implements the
existing request authenticator and authorizer ports. It is not a new Worker
registry, execution lease, or recovery controller. Standalone and embedded CLI
credential assembly now lower through `WorkerBootstrap::upstream`; transport
parsing and the finite remote-security selector remain transport-owned.
The existing shared outbound-HTTP adapter owns one process-level rustls
provider selection used before constructing Worker upstream, callback, and
Kubernetes clients; feature unification cannot create a competing implicit
selector.

Static boundary: a canonical `k8s:namespace:pod-name:pod-uid` Worker ID is only a
lookup coordinate. TokenReview against one configured API-server client,
ServiceAccount, namespace and explicit audience authenticates that coordinate.
The Registry remains authoritative for the registered incarnation/generation;
Dispatch remains authoritative for physical occupancy and its exact binding.
Tokens must be isolated from untrusted Sandbox workloads. The Pod token is not
a stop receipt, and neither its claims nor the caller's headers prove process
confinement or termination.

For Pod transport, the authenticated immutable UID also fixes the registration
incarnation. The process's proposed random nonce is not a second identity:
registration retries with different nonces lower to that same UID and the
existing Registry transaction returns the unchanged record, even after expiry
or closure. Registered requests and provider bindings require that incarnation;
the Registry continues to allocate and validate its own generation. Signed and
mTLS process transports keep their existing random-incarnation replacement.
This distinction prevents registration from reopening the same physical Pod
between exact Registry expiry and an independent Pod stop request. Historical
Pod records with another incarnation reject without mutation; deployment must
create a new Pod UID rather than upgrade the old physical domain in place.
The selected authenticator is exclusive for this domain; mixing alternative
registration authorities for the same Worker id is not a supported deployment.
The HTTP tests cross concurrent nonces, expired/closed retries, name reuse,
manifest drift and historical identities using the production client and
Memory/SQLite/PostgreSQL Registry adapters. The existing provider observation
model also rejects a nonce different from the authenticated Pod; this is not a
proof of full Dispatch recovery or remote termination.

Dynamic boundary: each request rereads the rotating projected token and obtains
a fresh TokenReview. Missing/duplicate headers, mismatched Pod UID or Worker
identity, revoked tokens, API errors and bounded review timeouts fail closed;
there is no cached-success or unsigned fallback. Only POST registration can use
an identity without the allocated incarnation. Production constructors require
HTTPS, reject redirects, and reject a configured CA bundle containing no valid
certificates. Local embedding remains an explicitly selected posture.

Production startup co-selects authentication, the execution-binding source and
independent stop source through one private transport assembly and trusted client.
The read-only `PodProcessObserver` is independent of Sandbox resource startup;
Coordinator observation cannot provision a Sandbox NetworkPolicy. Its Pod read
rechecks the authenticated UID so a same-name replacement between TokenReview
and binding cannot acquire the old Worker's authority. The existing handler
still performs Registry/claim checks before lookup and fresh checks before the
atomic Dispatch admission; no database lock spans Kubernetes IO.

New Pod admissions also require the deployment's process-evidence finalizer;
the frozen binding must retain that fact before effects. A historical unretained
binding cannot gain stop capability by observing a later finalizer.

On `Blocked` admission, the same Coordinator application path checks the current
successor claim and reads the original physical slot. Only a different Worker
with a persisted binding can be considered. Exact `expire_incarnation` must
return Applied before constructing the private `ClosedWorkerExecution` command;
the slot is reread before provider IO. Live, replaced, absent or unbound subjects
do not reach the stopper. Kubernetes validates the configured provider, immutable
Worker UID and frozen coordinate, then uses the existing conditional Pod stop.
Its credentials belong to Coordinator; the dead Worker's token is unnecessary.
Normal Worker admission retries drive this path, with bounded IO and no new
queue, timer, table or broad expiry scan. Concurrent/repeated requests target
only the same closed domain. A late local observation never clears a successor.

The resulting `WorkerProcessStopStatus` is deliberately local-only and has no
Dispatch mutation capability. An Exited observation preserves the original
binding and provider evidence reference; blank references and cross-domain cached
receipts reject. Neither the closed command nor the observation is deserializable.
Unknown/Exited both retain `Blocked`; provider
failure or timeout returns retryable Unavailable and retains occupancy. This
implements independent process-stop consumption, not complete attempt recovery.
Remote participants still require their own authoritative terminal/continuation
evidence before exact Dispatch retirement can be enabled. The HTTP decision
matrix crosses Registry adapters and old-response races; production CLI tests
exercise the assembled stop source against a mock Kubernetes API. Neither is
the Native hard-crash k3d acceptance test.

Configuration is explicit and role-owned:

- A server-mode Coordinator compiled with `container-k8s` accepts the
  `[worker_kubernetes]` table with `namespace`, `service_account`, `audience`
  and `provider_id`. The authenticated Kubernetes client uses the deployment's
  kube configuration; the provider ID names that operator-controlled domain.
- A server-mode Worker uses `worker_kubernetes_identity_directory`, containing
  projected `namespace`, `name`, `uid` and the rotating `token`. Standalone and
  CLI Worker startup share the same resolver and HTTPS upstream constructor.
  Authored `worker_id`, if present, must exactly equal the projected coordinate;
  projection identity drift between resolution and upstream assembly rejects.
- Mixing Pod authentication with signing credentials or an injected Coordinator
  authenticator rejects. Missing build features, malformed policy and wrong-role
  configuration reject rather than silently selecting an unbound transport.

This is not completed Kubernetes crash recovery. Complete remote-participant
evidence, exact Dispatch retirement and the Native hard-crash k3d E2E are not established by its
mock-API tests. No historical binding or remote termination can be inferred from
successful caller authentication. Existing restartable/privileged Worker Pod
profiles do not meet the observer's non-restarting private-process contract.

The attached tests own the cause/effect matrices for TokenReview, projection
rotation, transport posture and redirect/CA rejection. The existing Kani
transport selector covers 252 finite combinations, admitting exactly HTTPS with
signed or Pod-bound credential verification and exact identity. API-server
honesty, TLS internals, startup wiring and physical stop are outside that proof;
this stateless adapter does not add a parallel temporal recovery model.
The existing provider observation TLA+ model additionally separates TokenReview
from the subsequent Pod read, explores same-name replacement between them, and
checks that only matching identities can bind. Its finite model and semantic
mutants do not prove API-server honesty or Rust/Kubernetes refinement.

| Component | Owns | Must not own |
|---|---|---|
| Worker directory | Worker identity, incarnation, readiness, drain | Session desired state |
| Dispatch queue | Run attempt assignment and epoch | business outcome or Session placement |
| Recovery/commit services | committed attempt view and idempotent mutation | local executor handles |
| Worker runtime | local execution and non-authoritative cache | Control database or aggregate truth |
| Registered application | attempt envelope/result decoration | a second executor or realization path |

## Consequences

- A Run that awaited or lost its Worker can resume on another database-less
  Worker from committed truth.
- Response loss no longer turns a nonterminal commit retry into duplicate facts.
- SQLite keeps its simple single-writer cell. PostgreSQL can later remove the
  physical singleton without introducing direct Worker database writes.
- Embedding systems such as Flow supply only their attempt decorator; they do
  not own generic Session
  realization, registration, claim, lease, recovery, or settlement.
- Commit latency includes the Worker-to-Coordinator hop, and recovery adds a
  snapshot read after claim. Batching, deltas, and streaming may optimize those
  costs only after correctness is closed.
- A database transaction remains necessary but not sufficient: protocol-level
  identity, fencing, versioning, and consistent snapshot semantics are explicit.
- NATS, Kafka, and etcd may support wake, feed, or directory roles but do not
  replace the committed-truth store without a new adapter that satisfies the
  full coordinator and recovery contracts.

## Implementation Evidence

P0 and P1 are implemented by the public recovery projection, durable operation
receipts, injectable claimed-commit service, `WorkerNodeBuilder`, exact executor
registry inside the Session router, registered attempt-decorator factory,
frozen Session realization, neutral ownership verification, and the separated
lifecycle/dispatch feeds described in the detailed design.

The P2 active-active boundary is covered by a real PostgreSQL test that launches
two independent Coordinator processes over one schema and deliberately routes
registration-independent protocol calls across both. Coordinator A exits after
durably applying a claimed commit but before delivering its HTTP receipt;
Coordinator B returns the durable duplicate receipt, serves authoritative recovery,
and settles the same claim. The test asserts one receipt, one message, and one
completion tombstone and runs in `scripts/ci/pg_tests.sh`, which provisions an
ephemeral PostgreSQL instance so the case cannot silently self-skip.

## Rejected Alternatives

- **Workers write a shared SQLite/PostgreSQL database directly.** This bypasses
  the coordinator's claim fence and can leave another process's projection
  stale.
- **Use claim epoch as the commit idempotency key.** Reclaim changes the epoch
  for the same logical operation, so response-loss recovery could duplicate it.
- **Treat PostgreSQL transactions as the whole distributed protocol.** They do
  not define retry identity, external lease authority, or process-cache
  coherence.
- **Return an empty recovery view and interpret the claim as a fresh Run.** This
  loses Awaiting, historical context, delegation state, and cold-node recovery.
- **Put Flow envelopes and ACLs into Awaken.** Those are product semantics above
  the neutral Worker boundary.

## Amendment: Kubernetes drain and hard-crash evidence are distinct (2026-08-12)

Normal Kubernetes scale-in invokes the Worker's existing `/admin/drain` before
SIGTERM. The hook closes process-local claim admission, and the Pod termination
grace exceeds the Worker's bounded in-flight drain. The hook is an HTTP lifecycle
action rather than an in-image shell command, so a distroless production image
does not need curl, wget, or a shell.

A hard-crash conformance test must instead stop the exact CRI container process.
Deleting the Pod object first can withdraw networking while userspace briefly
retains its current claim, producing a real provider transport failure rather
than the intended crash-before-commit condition. The test observes an increased
container restart count, then requires lease expiry, a newly registered Worker
incarnation, recovery from committed truth, and one terminal response. This is a
test-injection distinction only; durable claim, commit, and settlement authority
remain unchanged.

Awaken remains embeddable without creating an application-specific Worker
protocol. The same Worker binary can execute locally or remotely, while
products keep their business aggregates and acceptance rules outside Awaken.
