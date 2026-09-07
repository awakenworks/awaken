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
A live Native response-only inference is narrower: Runtime can cancel its wait,
discard the response before any proposed tool crosses the governed tool boundary,
and return the owned executor. That actual local return permits the exact ACK;
it does not prove provider computation or billing stopped. Hard process loss
without that return still leaves the physical slot occupied.

Aborting or dropping the outer executor Future is not an ACK. The physical slot
remains occupied until the Future explicitly returns, and uncertain external
tool cancellation vetoes the ACK even after local return. No timeout or destructor
fabricates that terminal fact.

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

A committed A2A task identity supplies a narrower continuation proof. When the
complete Run-scoped `TaskReference` matches the immutable remote endpoint, the
A2A leaf classifies recovered execution as continuation-only: it reattaches with
`tasks/get` and cannot submit `message:send` again. The Worker then asks the one
Dispatch aggregate to atomically hand its physical slot from the predecessor to
the exact current claim. Every other executor and every incomplete/mismatched
reference keeps the fail-closed quiescence rule. Control still owns the slot,
claim, and transaction; the Worker contributes only the executor-specific proof.

## 2026-08-12 amendment

The former late Worker-authored Session-input design is superseded. Managed
Agents already supplies custom/local Worker placement through the Environment
WorkQueue. ADR-0075 therefore makes the complete Session command a pre-creation
input and limits Worker Control to realization of frozen truth.

This ADR no longer owns Session authoring or placement. Its remaining scope is
Worker lifecycle, Run attempt fencing, recovery, execution, commit, and
embedding through the attempt decorator.

## Static ownership

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
