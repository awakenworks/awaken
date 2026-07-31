# Recoverable Remote Worker Protocol

- Status: Accepted P0/P1 and P2 active-active design
- Date: 2026-07-23
- Decision: [ADR-0065](../adr/0065-recoverable-embeddable-remote-worker.md)
- Builds on: [distributed ACP execution](distributed-acp-execution.md),
  [run ingress and message delivery](run-ingress-message-delivery.md),
  [runtime persistence](runtime-persistence.md), and
  [ADR-0060](../adr/0060-durable-dispatch-completion-tombstone.md)

## 1. End-to-End Objective and Scope

Awaken shall expose one recoverable, database-independent, embeddable remote
Worker component. A Control Node owns dispatch and committed truth; a Worker
claims compatible work, obtains a consistent committed recovery view, executes
Native/ACP/A2A locally, renews its lease, and sends idempotent claimed commits
back to the coordinator. An embedding application supplies only one registered
Session projection and execution decorator.

This design closes the Worker protocol. It does not move product semantics into
Awaken. Flow continues to own Issue, Workflow, WorkUnit, acceptance, its
execution envelope, MCP capability tokens, Project/Actor/Resource policy,
`ResourceEffect`, and the conversion from technical Run success to business
success. Awaken owns the one Session environment; Flow may only project
claim-bound additions into it.

The current implementation already supplies registration, heartbeat,
drain/quiesce, placement, claim/renew/settle/checkpoint, epoch fencing, remote
claimed commit, execution backends, durable dispatch, and Worker fleet
management. The missing critical path is:

```text
claim
  -> consistent committed recovery
  -> execute from that recovery view
  -> fenced + versioned + idempotent commit
  -> advance local non-authoritative projection
  -> settle
```

The terms used here are precise:

- **logical committed-truth authority** means all writes obey one coordinator
  protocol;
- **physical single writer** means only one process can perform that protocol;
- SQLite requires both; PostgreSQL requires the first and may later remove the
  second;
- execution is at least once; committed logical operations are idempotent.

## 2. Current Protocol and Verified Gaps

### 2.1 Current interaction

```text
Worker                         Control Node                    Store
  |-- register(manifest) ---------->|                           |
  |-- heartbeat(Ready) ------------>|                           |
  |-- claim(requirements) --------->|-- dispatch claim -------->|
  |<------------- Claimed ----------|                           |
  |  constructs a remote HostCommit with no committed read view |
  |  selects fixed Native / ACP / A2A executor                  |
  |-- renew(claim) ---------------->|-- extend lease ---------->|
  |-- claimed ThreadCommit -------->|-- epoch check + commit -->|
  |<---------------- result --------|                           |
  |-- checkpoint(stream tail) ----->|-- stream checkpoint ---->|
  |-- settle(claim, result) ------->|-- completion tombstone -->|
  |-- heartbeat/drain ------------->|                           |
```

This protocol safely rejects stale-epoch writes and durably remembers terminal
dispatch completion. It does not yet reconstruct committed execution context on
another Worker.

### 2.2 Gaps and their effects

| Gap | Current evidence | Consequence |
|---|---|---|
| Remote commit read side is empty | remote `committed_messages`, `resume_ticket`, `run_state`, and committed state return empty/none | only a fresh execution context is safe |
| Claim contains no recovery prefix | `Claimed` carries dispatch ownership, not committed messages/state/tickets | Awaiting and crash recovery cannot move to a cold Worker |
| Claimed commit assembly is fixed | public router construction binds the private applier to `SharedHost` | an embedding application cannot inject its exact coordinator |
| Worker loop is private/fixed | worker assembly and session executor routing are private | Flow must duplicate generic Worker lifecycle code |
| Retry identity is incomplete | epoch fences attempts, but there is no stable nonterminal commit operation receipt | a lost commit response may cause duplicate facts |
| PostgreSQL reads use process projection | durable writes are transactional but a peer process cache is not automatically refreshed | active-active reads may be stale even when database writes are correct |

A stream checkpoint is not a recovery snapshot. It protects an in-flight stream
tail; it does not reconstruct committed Run/message/state/ticket truth.

## 3. Static Structure View

```text
Awaken Control Plane / Agent-Truth Cell
├── WorkerDirectory
├── DispatchQueue
├── StreamCheckpointStore
├── RunRecoverySource
├── ClaimedCommitService
│   ├── WorkerRequestAuthenticator
│   ├── DispatchQueue              claim/epoch authority
│   └── ThreadCoordinatorResolver  committed-truth authority
├── RunLifecycleFeed               committed Run truth
└── DispatchOperationalFeed        delivery/lease operations

Awaken Worker Node
├── WorkerNode
│   ├── WorkerControlClient
│   ├── HttpDispatchQueue
│   ├── RemoteRecoveryClient
│   ├── RecoveryProjection         non-authoritative read cache
│   ├── RemoteClaimedRunCommit
│   ├── heartbeat/renew/drain state machine
│   └── AttemptExecutorRegistry
│       ├── native
│       ├── acp:<cli-id>
│       └── a2a:<endpoint-or-profile>
└── registered application
    ├── ApplicationSessionProvisioner
    │   └── Environment snapshot / provisioning spec / service attachments
    └── RunAttemptExecutor decorator
        └── Flow envelope / ownership / output adapter
```

### 3.1 Ownership and dependency rules

| Boundary | Owns | Depends on | Must not own |
|---|---|---|---|
| Control Node | claim authority, committed truth, receipts, durable feeds | store adapters and authenticators | product envelope or business acceptance |
| Worker Node | attempt lifecycle and local execution | Control transport, executor registry, recovery cache | authoritative facts or direct Control database access |
| Store adapter | transaction/CAS/snapshot implementation | chosen storage medium | Worker placement or product policy |
| Registered application | business Session projection, input/output adaptation, and ACL | registered context, neutral Session/attempt ports | Session realization, registration, lease, recovery, commit protocol |

The Control Node is not necessarily a singleton. It is the location of the
logical coordinator protocol. Every implementation must preserve G1/G13 commit
atomicity, G5/G6 durable dispatch semantics, G32 fact authority, G35 durable
completion, and G40 executor route selection.

## 4. P0 — Correctness and Embeddability

P0 is required before a remote Worker is described as recoverable or before Flow
removes its generic Worker workaround.

### 4.1 Recovery contract

The neutral contract is one consistent prefix, not a bag of independently
loaded projections:

```rust
pub struct RunRecoverySnapshot {
    pub thread_id: ThreadId,
    pub claimed_run_id: RunId,
    pub runs: Vec<RunRecord>,
    pub latest_run_id: Option<RunId>,
    pub messages: Vec<Message>,
    pub state: Vec<StateCommand>,
    pub resume_tickets: Vec<RunResumeTicket>,
    pub thread_version: u64,
    pub store_cursor: u64,
    pub next_commit_ordinal: u64,
}
```

- `thread_version` is per thread and is used for optimistic concurrency.
- `latest_run_id` preserves the existing `RunStore::latest_run` semantics
  without requiring the Worker to infer ordering from backend-specific records.
- `store_cursor` identifies the source-store prefix for diagnostics, feed
  backfill, and future delta reads.
- `next_commit_ordinal` is derived from committed truth. If a commit was applied
  before a crash, the next snapshot advances it; if it was not applied, recovery
  reuses the same ordinal. A Worker never invents a random retry identity.
- A global cursor must not be used as per-thread CAS: unrelated Thread commits
  would create false conflicts.
- The included Run set must cover the claimed Run and every committed dependency
  required by the execution/recovery contract; it is not silently truncated.
- SQLite materializes it under its existing connection/transaction lock,
  PostgreSQL under a repeatable-read or stronger transaction, filesystem under a
  read fence, and in-memory under one lock.

The initial HTTP surface is deliberately two-step:

```text
POST /v1/worker/dispatch/claim
  -> Claimed

POST /v1/worker/recovery/snapshot
  identity + exact Claimed token
  -> RunRecoverySnapshot

POST /v1/worker/dispatch/abandon
  identity + exact Claimed token + typed recovery failure
  -> Abandoned
```

No valid snapshot means no executor entry. A combined
`claim-with-recovery` route may later reduce latency but remains an adapter over
these semantics.

### 4.2 Recovery projection

`RecoveryProjection` implements the synchronous committed read ports needed by
the runtime, including `ThreadReader`/`RunStore` compatibility:

1. atomically replace an empty attempt cache with a validated snapshot;
2. expose only that snapshot and later acknowledged commits;
3. apply a returned receipt/delta at most once and only in increasing
   `thread_version`;
4. reject gaps, hash mismatch, another Thread/Run, or regression;
5. discard the cache when the attempt loses ownership.

It is an execution cache, not a write-through repository. A restart or version
conflict refetches truth from `RunRecoverySource`.

### 4.3 Claimed commit contract

```rust
pub struct CommitOperationId {
    pub run_id: RunId,
    pub ordinal: u64,
}

pub struct CommitOperation {
    pub operation_id: CommitOperationId,
    pub expected_thread_version: u64,
    pub payload_hash: CommitPayloadHash,
    pub commit: ThreadCommit,
}

pub struct ClaimedCommitCommand {
    pub claim: RunClaim,
    pub operation: CommitOperation,
}

pub struct CommitReceipt {
    pub operation_id: CommitOperationId,
    pub commit_sequence: u64,
    pub thread_version: u64,
    pub payload_hash: CommitPayloadHash,
    pub duplicate: bool,
}
```

The authenticated `WorkerIdentity`/incarnation is request context at the
service edge, not a field passed into the fact-store coordinator. This preserves
the existing separation between Worker authority (`RunClaim`) and Thread truth
(`CommitOperation`).

`operation_id` remains stable across HTTP retry, response loss, lease expiry,
and reclaim. The claim epoch authorizes the current delivery attempt but is not
part of logical operation identity. The payload hash is computed over one
versioned canonical encoding of `ThreadCommit`; transport JSON field order or
compression must not affect it.

The claimed critical section is:

```text
authenticate worker identity/incarnation
  -> acquire and hold exact dispatch claim/epoch guard
  -> reject wrong owner, stale epoch, expiry, or terminal tombstone
  -> begin coordinator transaction
  -> find operation receipt
       same id + same hash -> return original receipt
       same id + other hash -> conflict
  -> compare expected_thread_version
  -> validate and append ThreadCommit
  -> increment per-thread version
  -> insert operation receipt
  -> atomically append lifecycle outbox row when applicable
  -> commit transaction
  -> return receipt
```

When dispatch and committed truth share a database, an adapter may implement the
guard and coordinator work in one transaction. When they are separate stores,
the existing durable epoch guard must prevent renew/reclaim/settle from changing
the claim until the coordinator transaction finishes. There is no two-phase
commit: the claimed commit does not mutate dispatch state, and settlement is a
later idempotent operation.

A response can be lost after the database commits. Retrying returns the durable
receipt under a still-valid claim and must not append again. If the lease has
already been reclaimed, the old Worker is fenced; the new owner observes the
applied commit in its snapshot instead. A version conflict causes the Worker to
stop that attempt, refetch a snapshot under a valid claim, and resume only
through the runtime's supported recovery path.

### 4.4 Injectable commit service

The public assembly shape is:

```rust
pub struct ClaimedCommitService {
    dispatch: Arc<dyn DispatchQueue>,
    coordinator: Arc<dyn OperationCoordinator>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
}

pub fn claimed_commit_router(
    service: Arc<ClaimedCommitService>,
) -> Router;
```

The coordinator implementation may cover one SQLite cell or a PostgreSQL-backed
set of Threads. It is the exact instance owned by the embedding composition
root, so a successful commit updates the same authoritative store/projection
used by that process. The service does not choose a database or own an
application projection. The public router accepts only a versioned
`CommitOperation`; there is no raw or unclaimed Worker commit surface.

### 4.5 Public Worker assembly

```rust
WorkerNodeBuilder::new(upstream)
    .with_deployment_config(deployment)
    .with_inference_materializer(inference)
    .with_credential_materializer(credentials)
    .with_remote_attempt_executor(remote_attempt)
    .with_hand_executor_factory(hand_factory)
    .with_session_container_provider("provider-id", provider)
    .with_application_factory(factory)
    .with_application_gate(gate)
    .with_registered_memory_mounter_factory(memory_mounter_factory)
    .with_standard_manifest(application_capabilities)
    .build()?
    .run_until_shutdown()
    .await
```

`with_standard_manifest` is the sole standard capability projection. At
`build()` it derives the immutable manifest from the deployment, installed
inference materializer, resource plane, credential materializer and
remote-attempt evidence, ACP profile,
optional externally installed Session container provider, and
explicit application capabilities. An installed provider is authoritative for
its backend id and enforceable Sandbox capabilities; deployment inference is
used only when no provider was injected. Secret substitution and enforced
no-bypass networking must both be present before the same derivation can publish
Worker-relay credential evidence. A special deployment may
instead select `with_manifest(explicit_manifest)`; selecting both sources fails
closed rather than depending on call order. The process adapters parse
environment-driven deployment and manifest metadata once before installing
their typed values on the Builder.

`awaken-worker` depends only on these neutral ports and the typed Worker
transport. `awaken-cli` is the product composition root that injects the
concrete inference, A2A, relay, and Memory-mount adapters. The Memory factory is
evaluated after registration because its HTTP client must carry the assigned
Worker incarnation. File, Memory, custom-Skill, and Repository binding clients
are constructed from that identity-bound upstream. Resource capability is
derived from the installed Memory mounter; Repository credential capability also
requires the exact credential materializer. No marker object or Resource Catalog
connection is installed. The boundary check rejects any direct `awaken-worker`
dependency on `awaken-server`, `awaken-control`, `awaken-resource-contract`, or
`awaken-sandbox-memoryd`.

`build()` is synchronous and side-effect free: it derives or accepts one
manifest and runs the same contract validation before registration.
`run_until_shutdown()` owns process signals; supervisors and conformance tests
use the same `WorkerNode::run_until` state machine with an injected shutdown
future. Registration creates one immutable `RegisteredWorkerContext` from the
returned `RegisteredWorker` and the identity-bound `WorkerUpstream`; the factory
then creates one `RegisteredWorkerApplication`. Its claim-time provisioner
produces a frozen neutral plan before Session realization. The Worker submits it
over the identity-bound transport as a claim-fenced contribution; the
Control-owned Session compiler consumes the preparation intent, freezes one
baseline, and creates generation 1 state. The Worker then realizes that
authoritative Session projection and constructs its complete per-Session
Native/ACP/A2A router before applying the application decorator. There is no
public Worker/Host path that replaces the complete router or creates another
Session environment.

`WorkerNode` owns:

- register and incarnation;
- Ready/Draining/Quiesced heartbeats;
- compatible claim and recovery snapshot fetch;
- lease renewal while the attempt is active;
- claimed commit and local projection advancement;
- settle or typed abandon;
- drain admission fence, quiesce, deregistration, and shutdown.

The decorated executor receives only a validated attempt context. Run ingress
captures the exact `RunClaim` in an `AttemptOwnershipVerifier`. The Host supplies
the same verifier to claim-time application provisioning and later carries it
in `RuntimeRunContext`, allowing application code to recheck live ownership
without learning dispatch vocabulary. A provisioner produces one secret-free
plan; the Worker submits it through the identity-bound transport as a
claim-fenced `ApplicationSessionContribution` to the Control-owned Session
application service. Only Control finalizes the baseline and Resource/MCP
generation 1 state. A decorator may parse the application envelope and project
outputs. Neither can bypass claim, recovery, backend routing, Session
realization, commit, or settlement.

### 4.6 Topology validation

Assembly fails closed for:

- remote upstream plus any Worker-local/shared Control, Coordinator, or Resource
  authority database path;
- a remote commit client without a recovery source;
- a manifest capability that no registered executor can implement;
- SQLite configured with more than one physical committed-truth writer;
- active-active PostgreSQL while authoritative recovery still depends on a
  process-local projection.

The CLI composition additionally removes the Worker's durable Host storage root.
`SharedHost::new_worker_with_deployment` installs File and Memory clients before
store selection, so Worker startup cannot briefly open and then replace local
File/Memory SQLite authorities. It installs `WorkerCredentialFileResolver` for
exact `WorkerReference` material and recipient-bound projected Control
envelopes. It advertises Resource capabilities only when the corresponding
per-kind network adapters exist. A Resource-capable Worker
without the registration-bound Memory mounter factory fails during `build()`;
there is no shared-store fallback.

### 4.7 P0 acceptance

P0 is done only when:

1. Worker A awaits or crashes; Worker B claims, snapshots, and continues with the
   committed messages/state/ticket.
2. A stale Worker commit, renew, settle, checkpoint, and abandon all fail under
   the old epoch.
3. Losing any commit response and retrying cannot duplicate messages, state,
   events, or terminal facts.
4. A snapshot is demonstrably one committed prefix and a stale expected version
   cannot commit.
5. Flow can use `WorkerNodeBuilder` without implementing registration,
   heartbeat, claim, renew, recovery, settle, or drain.
6. In-memory, SQLite, and PostgreSQL adapters pass the same remote Worker
   transport/store conformance suite.

Implementation evidence as of 2026-07-23:

- `remote_worker_recovery_e2e.ts` runs one real Control process and two real
  database-less Worker processes. It loses a receipt after durable apply, proves
  the same operation is retried, kills Worker A before Awaiting settlement,
  expires its lease, and proves Worker B reclaims and resumes from the committed
  A2A context without resending the initial message.
- The same scenario replays Worker A's delayed claimed commit after Worker B has
  advanced the epoch and verifies rejection, one terminal transcript effect,
  and terminal settlement.
- `commit_ingest_http.rs` independently injects a malformed/lost first receipt
  after apply and verifies that `RemoteClaimedRunCommit` obtains the durable
  duplicate receipt without duplicating history.
- The production composition preserves the recovery port through
  `AnyDispatchStore` and authenticates registered recovery reads with the
  logical Worker id while authorizing them with the incarnation-bound lease
  owner and epoch.

## 5. Dynamic Behavior View

### 5.1 Normal and resumed execution

```text
Worker             Control transport       Dispatch       Coordinator/Store
  | register ------------>|                    |                    |
  | Ready heartbeat ----->|------------------->|                    |
  | claim ---------------->|------------------->| lock owner+epoch   |
  |<----- Claimed ---------|                    |                    |
  | recovery(claim) ------>|--------------------------------------->|
  |<-- consistent snapshot|<---------------------------------------|
  | load RecoveryProjection                                     |
  | verify ownership; prepare application Session plan           |
  | contribute(plan,claim,fp) -->| Session root CAS/finalize       |
  |<-- finalized Session projection + realization lease ----------|
  | realize one SessionEnvironment under Session lease            |
  | select exact backend_ref; enter executor                     |
  | renew --------------->|------------------->| extend same epoch  |
  | commit(op,ver,hash) -->|-------------------------------------->|
  |                        | epoch + receipt + CAS + transaction    |
  |<--------------- CommitReceipt --------------------------------|
  | apply acknowledged delta to local projection                  |
  | settle -------------->|------------------->| durable completion|
  | terminal/drain heartbeat                    |                    |
```

An Awaiting Run follows the same path after wake: the new claim authorizes a
snapshot containing the committed resume ticket and pending result/input; the
ordinary runtime resume path consumes it.

### 5.2 Failure matrix

| Failure | Required reaction | Durable outcome |
|---|---|---|
| Worker crashes before commit | lease expires; another Worker reclaims and snapshots | no uncommitted output is truth |
| Worker crashes after commit before settle | new Worker sees committed truth/tombstone and completes idempotently | committed operation is not repeated |
| old Worker sends a delayed commit | epoch fence rejects it | truth unchanged |
| commit response is lost | same operation id/hash is retried | original receipt returned |
| snapshot fetch/validation fails | abandon or lease expiry; executor never starts | no fresh-context fallback |
| thread version conflicts | stop attempt and refresh through recovery | stale context cannot append |
| Worker loses registration/incarnation | stop claim admission and drain active work; unsafe operations fail closed | directory identity cannot be borrowed |
| Control Node fails with SQLite | replacement reattaches the cell store before serving | one physical writer remains |
| one PostgreSQL Control Node fails in P2 | another node serves the same protocol from authoritative DB state | no sticky-session dependency |

### 5.3 State transitions

```text
Unregistered
  -> Registered(Starting)
  -> Ready
  -> Claimed
  -> Recovering
  -> Executing <-> Renewing
  -> Settling
  -> Ready

Recovering --failure--> Abandoning -> Ready
Ready/Claimed/Recovering/Executing --drain--> Draining
Draining --no active attempt--> Quiesced -> Deregistered
any state --identity lost--> fail closed; no new claim
```

## 6. P1 — Production Extensibility

P1 is secondary to P0 correctness but required for a broadly reusable Worker
fleet.

### 6.1 Attempt executor registry and application decoration

`AttemptExecutorRegistry` is public and selects the frozen `backend_ref` by exact
registered key:

```text
native-runtime
acp:claude
acp:codex
acp:gemini
acp:opencode
a2a:<endpoint-or-profile>
```

`SessionAttemptExecutor` is the one backend router. It builds an
`AttemptExecutorRegistry` from the immutable Session publication and the
Native/ACP/A2A executors actually installed by the Host. Duplicate keys, native
refs registered as exact routes, empty ACP/A2A targets, or unknown refs fail
closed.

The Worker exposes only `WorkerNodeBuilder::with_application_factory`. The
factory runs after registration and receives `RegisteredWorkerContext`; its
returned `RegisteredWorkerApplication` supplies at most one claim-time Session
provisioner and one function that wraps the complete Session router for execute,
resume, and cancel. The removed
`with_attempt_executor`/`with_attempt_executor_registry` Worker paths are not
retained as compatibility tracks.

Current-attempt authority remains in `DispatchQueue`:

- `claim_is_current(claim, now_ms)` reuses the exact epoch guard and verifies the
  lease remains live;
- `worker_owns_run(identity, run_id, now_ms)` supports a server-side application
  capability edge that has authenticated Worker identity but must not trust a
  caller-supplied epoch;
- `HttpDispatchQueue` exposes only the first operation to the owning Worker. The
  second remains a Control-side query.

### 6.2 Worker identity

The header-only worker id remains local/test configuration. Production adapters
provide:

- `MtlsWorkerAuthenticator`, which consumes a verified
  `MtlsWorkerPrincipal` request extension supplied by the TLS acceptor and binds
  it to `worker_id` plus the registered incarnation;
- `WorkerSigningCredential`, the shared provisioning object for one Worker,
  rotation key, credential id, and redacted HMAC secret;
- `SignedWorkerRequestAuthorizer`, which creates a fresh short-lived assertion
  for every request and, after registration, binds it to the allocated
  `WorkerIdentity`;
- `SignedWorkerAuthenticator`, which supports overlapping credentials for key
  rotation, immediate credential/key revocation, expiry/skew policy, route and
  method binding, constant-time HMAC verification, and request-id replay
  rejection;
- the client-side `WorkerRequestAuthorizer` port, carried by `WorkerUpstream`
  through registration, dispatch/recovery/checkpoint, and claimed commit.

The signed flow is:

```text
bootstrap WorkerUpstream
  -> signed register assertion(worker_id only)
  -> RegisteredWorker(identity)
  -> bind the same request authorizer to identity
  -> fresh signed assertion(method, path, identity, issued/expiry, request_id)
     on every heartbeat / claim / renew / recovery / commit / settle
  -> authenticator verifies signature + time + route + replay
  -> handler independently verifies directory identity + claim/epoch
```

An mTLS deployment configures the same `WorkerUpstream` with a client
certificate and configures its server TLS acceptor to publish
`MtlsWorkerPrincipal`; it does not trust a proxy-supplied certificate header.
Signed assertions must still travel over TLS. They can be layered over mTLS when
both proof-of-possession at the transport edge and application-level replay
fencing are required.

The composition decision table is:

| Input at composition root | Registration result | Later lifecycle transport | Decision |
|---|---|---|---|
| complete `WorkerUpstream` with custom client and Worker id | accepted | the same clone-shared client and allocated identity serve heartbeat, claim, recovery, commit, settle, drain, and deregister | allow |
| Local mode with URL only | accepted | helper constructs the explicit header-compatible local `WorkerUpstream` | allow only for local/test use |
| Server Worker with `worker_request_credential_file` and matching `worker_id` | accepted | every lifecycle, Resource, and commit request carries a fresh signed assertion | allow |
| Server Coordinator/AllInOne with non-empty `worker_trust_credentials_file` | enrolled Workers accepted | one shared authenticator verifies every Worker-facing route | allow |
| Server mode missing either role-owned credential file | none | no Worker-facing request is attempted or admitted | reject startup |
| empty upstream URL | none | none | reject at `build()` |
| application attempts a second manifest source | none | none | reject topology conflict |
| registered identity is lost or replaced | registration/liveness fence fails | claims and mutations fail closed | stop admission and drain |

Tests attach a caller-defined signed request authorizer to the complete upstream
and require its assertion on every observed lifecycle request. The production
mTLS test separately proves the TLS peer identity and custom client behavior at
the acceptor.

Authentication does not replace claim authorization: every state-changing
request still validates owner, epoch, and operation semantics.

### 6.3 Lifecycle feeds

Do not mix agent truth and delivery operations into an ambiguous status stream:

```rust
trait RunLifecycleFeed {
    async fn events_after(
        &self,
        cursor: LifecycleCursor,
        limit: usize,
    ) -> Result<LifecyclePage, RunLifecycleFeedError>;
}

trait DispatchOperationalFeed {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
    ) -> Result<DispatchPage, DispatchError>;
}
```

The Run feed contains committed `running`, `awaiting`, `resumed`, `completed`,
`failed`, and `cancelled` transitions. The dispatch feed contains `claimed`,
`reclaimed`, `lease_lost`, `settled`, and `dead_lettered`. Consumers persist
their cursor and backfill after reconnect. Live notification is only a wake
hint; it never replaces the durable cursor.

`DispatchOperationalFeed` is implemented by the memory, SQLite, and PostgreSQL
dispatch stores. Its `DispatchOperation` payload is a tagged product type:

Every dispatch event also carries `recorded_at_ms`, the store-assigned wall
clock time at which the authority mutation was durably appended. The timestamp
is metadata for elapsed-time projections and audit; cursor order and the
operation product type remain the authority for state. Legacy rows may expose
no timestamp and consumers that require elapsed time must fail closed or park
them rather than inventing a duration.

| Applied dispatch mutation | Durable operation facts |
|---|---|
| fresh or awaiting claim | `claimed { claim }` |
| expired lease is claimed at a higher epoch | `lease_lost { previous, expired }`, then `reclaimed { previous, claim }` |
| current epoch settles | `settled { claim, outcome }` |
| expired lease exhausts its retry budget | `lease_lost { claim, retry_exhausted }`, then `dead_lettered { claim, attempt_count }` |
| cancellation revokes a live lease | `lease_lost { claim, cancelled }` |
| fenced settle, duplicate/no-op command, successful renewal | no operation fact |

SQLite and PostgreSQL append the operation row in the same transaction that
changes dispatch authority. The memory reference does both under the same lock.
Consequently a consumer cannot observe `settled` while the dispatch mutation
rolled back, and a stale owner cannot publish a false settle. `DispatchCursor`
is exclusive and scoped to one dispatch store; `DispatchPage.next_cursor` is
the last event actually returned, or the requested cursor for an empty page.
The SQL outbox survives process restart. `AnyDispatchStore` exposes it for its
built-in SQLite/PostgreSQL backends and fails explicitly when an injected
adapter implements only `Dispatch`.

`CheckpointRunLifecycleFeed` is the portable single-process Run implementation. It reads
`EventScope::All` from the existing committed-event projection, accepts an
exclusive `LifecycleCursor`, and returns `LifecyclePage.next_cursor` equal to
the last event actually returned. It projects only committed
`RunStateChanged` facts:

| Committed transition | Feed kind |
|---|---|
| first/non-awaiting → `Running` | `running` |
| any → `Awaiting` | `awaiting` |
| `Awaiting` → `Running` | `resumed` |
| `Ended(NaturalEnd)` | `completed` |
| `Ended(Cancelled)` | `cancelled` |
| every other `Ended` cause | `failed` |

The complete neutral `RunState` rides beside this classification, so a consumer
does not need to parse strings or lose failure detail. Malformed lifecycle
payloads and events referencing an unknown Run fail the page read; they are not
silently skipped.

A feed instance addresses one committed-truth partition. A PostgreSQL schema
and a SQLite database file each define one shared partition; a filesystem host
that creates one coordinator per Thread remains Thread-partitioned. A cursor is
valid only for the feed instance that produced it. A merged feed must use a
durable outbox or partition-qualified cursor and must not manufacture a global
order across independent partitions.

If a deployment exposes one merged feed, it must be a durable projection/outbox
with explicit ordering semantics. It must not pretend that independently stored
agent truth and dispatch facts share a natural global transaction.

## 7. P2 — Active-Active and Performance

P2 removes physical Control Node constraints and reduces transfer cost while
retaining the P0 protocol.

### 7.1 PostgreSQL active-active Control Nodes

Definition of done:

- two or more Control Nodes run behind a non-sticky load balancer;
- every claimed commit and recovery read uses authoritative database state or a
  version-validated cache;
- dispatch claim, per-thread CAS, operation receipt, and lifecycle outbox are
  transactionally enforced;
- one Control Node may fail without preventing a Worker from recovery or retry;
- cross-node notifications are hints followed by durable backfill;
- multi-process failure-injection tests prove response-loss, stale-cache,
  reclaim, and concurrent-commit behavior.

PostgreSQL transactions are the storage mechanism for this protocol, not a
substitute for it. Row locks/serializable transactions protect database state;
operation receipts solve ambiguous responses; versions solve stale execution
context; durable reads solve process-cache coherence.

Implementation evidence as of 2026-07-23:

- `active_active_postgres.rs` re-executes its test binary as two independent
  Control processes with separate coordinator/dispatch instances over one
  PostgreSQL schema.
- The Worker alternates enqueue, claim, recovery, commit retry, and settle
  across the two origins; no request-affinity state is used.
- Control A exits in the ambiguous window after commit apply and before receipt
  delivery. Control B returns the persisted duplicate receipt, reads the
  committed snapshot from PostgreSQL, and settles the still-valid claim.
- The final database assertion requires exactly one operation receipt, message,
  and dispatch completion. `scripts/ci/pg_tests.sh` supplies a disposable real
  PostgreSQL instance and runs the scenario single-threaded.
- `PostgresCommitCoordinator` and `SqliteCommitCoordinator` implement
  `RunLifecycleFeed` with authoritative SQL window queries. A peer constructed
  before another node's commits still pages `Running`, `Awaiting`, `Resumed`,
  and `Completed` without refreshing its synchronous compatibility projection.

### 7.2 Recovery optimization

These are optional transfer/performance optimizations after full snapshots are
correct, and remain deferred until profiling justifies them:

- serve checkpoint plus ordered committed deltas;
- bound and compress snapshot size;
- compact state without discarding authoritative history;
- cache by `(thread_id, thread_version, store_cursor)`;
- invalidate or reject a cache entry whose version is not authoritative.

### 7.3 API/config cleanup

Completed:

- core Worker and commit assembly accept explicit typed dependencies;
- the CLI bridge parses deployment environment into `DeploymentConfig`;
- `WorkerNode` exposes the reusable lifecycle state machine;
- every HTTP dispatch client is constructed with a registered
  `WorkerIdentity`; the owner-string compatibility client and its local owner
  cache are removed;
- each `SharedHost` explicitly owns an injected Worker dispatch transport; no
  process-global injected-dispatch slot remains.

Optional gRPC/bidirectional streaming may batch renew/commit/feed traffic, but
it must carry the same identity, claim, epoch, operation id, version, hash, and
receipt.

## 8. Storage and Component Choices

The domain protocol is store-neutral; the implementation effort and operational
trade-offs are not.

| Component | Best fit | Advantages | Costs/limits | Decision |
|---|---|---|---|---|
| PostgreSQL | default durable committed truth and dispatch | existing adapters/schema/tests; strong transactions, row locks, portable operations | active-active still needs authoritative reads, retry receipts, and tuning | default P0/P2 path |
| SQLite | one cell / embedded / edge | simplest operations, strong local transactions, excellent test/dev fit | one physical writer; no multi-node HA | supported P0 cell, never shared by Workers |
| CockroachDB | distributed-SQL replacement | serializable distributed transactions and SQL ergonomics | retry/ambiguous-result handling, locality/latency, compatibility testing | viable new store adapter |
| Cloud Spanner | global strongly consistent control plane | external consistency, managed multi-region, change streams | cost, cloud lock-in, schema/query adaptation | viable for global-scale deployments |
| FoundationDB | custom transactional ordered-KV backend | strong transactions and flexible subspaces | build indexes/projections/schema tooling; transaction/value limits | viable only with substantial adapter work |
| DynamoDB | AWS-native key/value deployment | conditional writes, transactions, managed scaling | aggregate/data-model rewrite, transaction action limits, global ordering complexity | viable for a deliberately redesigned adapter |
| etcd | Worker directory/election/short leases | linearizable CAS/watch and lease primitives | not suited to large Run transcripts or event history | supporting component only |
| Kafka | lifecycle/fact distribution and replay | durable ordered partition logs and consumer cursors | needs materialized query/CAS store; cross-aggregate transaction complexity | feed/outbox transport, not sole truth |
| NATS JetStream | wake, work notification, lightweight feed | simple operations, retention and consumer primitives | dedupe windows and KV semantics do not satisfy the whole coordinator contract | wake/transport component only |

Switching the main store does not change the public protocol. A candidate must
pass the same atomic commit, per-thread CAS, consistent recovery snapshot,
operation-receipt, dispatch-fence, and lifecycle cursor conformance suite.

## 9. Formal Verification and Executable Evidence

This protocol has interacting lease, crash, retry, CAS, and projection states;
bounded formal verification is justified. It complements rather than replaces
database and HTTP failure-injection tests.

`RemoteWorkerProtocol.tla` composes the protocol with:

- variables for Worker incarnation/state, dispatch owner/epoch/lease/status,
  committed thread version/Run state/receipt map, Worker projection version, and
  pending input/cancellation;
- actions for Register, Heartbeat, Claim, Snapshot, Execute, CommitRequest,
  CommitApply, ResponseLost, Retry, Renew, Expire, Reclaim, Settle, Abandon,
  Drain, Quiesce, input delivery, and cancellation request.

Safety properties:

1. one live claim owner per dispatch;
2. stale epoch never mutates committed truth;
3. one logical operation id maps to at most one payload hash and append;
4. terminal Run state is absorbing;
5. every snapshot/projection is a prefix of committed truth;
6. execution never starts before a valid snapshot;
7. a Worker observes its acknowledged writes monotonically;
8. pending input is consumed only with the commit that records its result;
9. Workers never directly mutate committed truth.

Liveness under explicit fairness assumptions:

- claimable work is eventually claimed;
- an expired owner is eventually reclaimed;
- an accepted terminal commit is eventually settled and feed-visible;
- a draining Worker eventually quiesces when executor calls terminate.

Executable evidence includes:

- shared in-memory/SQLite/PostgreSQL state-machine specs;
- simulated-clock lease and reclaim tests;
- HTTP tests that drop responses after durable apply;
- the two-Control-process PostgreSQL active-active failure-injection test;
- property tests for operation id/hash/version rules;
- concurrency model checking for process-local projection code where applicable.

TLC results must state finite bounds and cannot be described as an unbounded
liveness proof.

Implementation evidence as of 2026-07-23:

- TLC exhaustively checked two Workers, two lease epochs, two logical commit
  operations, two payload hashes, two committed versions, and one incarnation
  per Worker.
- The complete graph generated 2,155,183 states, found 258,524 distinct states,
  reached depth 30, and left zero states on the queue with no invariant or
  temporal-property violation.
- The checked progress properties are conditional: weak fairness applies only
  while claim, snapshot, commit application, settlement, or quiescence remains
  enabled. Network recovery, Worker availability, and external input remain
  environmental assumptions; this is not an unbounded liveness proof.
- `formal/coverage.json` links all nine protocol safety obligations to the TLA+
  model and the production claim fence, commit ingest, operation receipt,
  recovery projection, and Worker execution-admission code.

## 10. Remote Worker Component Catalog

This catalog owns the stable remote Worker boundary roles. Private HTTP DTO
helpers and CLI parsing do not belong here.

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `RunRecoverySource` | boundary port | consistent committed recovery prefix for a valid claim | fact/read store and dispatch fence | executor policy or mutable Worker cache | cold Worker executes without truth | G6/G32; snapshot-prefix conformance |
| `RunRecoverySnapshot` | value object | portable committed Run/message/state/ticket prefix and versions | agent contract values | live handles, secrets, product envelope | independently read projections are combined inconsistently | serialization and consistent-read tests |
| `RecoveryProjection` | Worker component | attempt-local synchronous read projection | validated snapshot and commit receipts/deltas | commit authority or persistent truth | stale/local state is treated as authoritative | monotonic projection/property tests |
| `CommitOperationId` | value object | stable logical commit identity across retries/reclaims | Run id and ordinal | claim epoch or transport request id | response loss duplicates a commit | retry/reclaim idempotency tests |
| `ClaimedCommitService` | application service | authenticated, fenced, versioned, idempotent commit orchestration | directory, dispatch, coordinator resolver, authenticator | `SharedHost` construction or product projection | embedding system cannot use its coordinator; stale owner writes | G1/G13; dependency-injection and stale-epoch tests |
| `ThreadCoordinatorResolver` | boundary port | coordinator selection for the addressed Thread/cell | configured commit backend | placement, auth, or application cache | request commits through a different authority | same-source and multi-node tests |
| `WorkerNode` | public component | Worker lifecycle from registration through drain | control client, recovery client, commit client, executor | product envelope or Control database | every application rewrites lifecycle and diverges | G5/G6; lifecycle state-machine suite |
| `WorkerNodeBuilder` | assembly API | validated Worker dependency assembly, mutually exclusive explicit/standard manifest selection, one optional existing Session container-provider seam, and one post-registration application factory | typed deployment, neutral Host/credential/resource ports, application capabilities/factory | server/control crate, database opening, process environment parsing, hidden global stores, or a second capability derivation | application duplicates Worker/router lifecycle or advertises capabilities absent from the installed topology | construction, manifest-source, missing-port preflight, capability-derivation, and registered-context lifecycle tests |
| `RegisteredWorkerContext` | immutable assembly value | one allocated Worker incarnation plus its identity-bound request transport | `RegisteredWorker`, `WorkerUpstream` | mutable liveness truth, product ACL, or a second credential | application uses an unsigned/stale identity or parallel trust path | signed transport and registration-order tests |
| `RegisteredWorkerApplication` | immutable assembly value | the one provisioner/decorator pair installed after registration | registered context and neutral Host ports | Worker lifecycle or a second execution router | application hooks are assembled under different identities | registered-context lifecycle tests |
| `ApplicationSessionProvisioner` | Worker application port | produce one claim-bound, secret-free neutral plan | activation and neutral ownership verifier | Sandbox creation, Session cache, product persistence, materialized credential, or a second environment plan | Native and ACP receive different inputs or a stale claim contributes effects | application-input ordering, fingerprint, and stale-claim tests |
| `ApplicationSessionContribution` | Worker-to-Control command | carry the complete plan fingerprint and secret-free mounts/env/prompts/network/MCP inputs before Session finalization | identity-bound Worker transport and exact Run claim/epoch | desired state, plaintext, live handles, or direct database write | Worker-local overlay diverges from Control or response loss duplicates creation input | G42; replay/conflict/stale-claim/no-application tests |
| `EnvironmentSnapshot` | Session value object | exact reusable Managed Environment revision/fingerprint frozen for one Session, including the sole effective `NetworkPolicy` | normalized Environment definition | live Sandbox handles, mutable latest-config lookup, plaintext credentials, or MCP routes | Environment edits or compatibility network fields mutate an existing Session | G42; pin/fingerprint, safe-policy-meet, and update-affects-new-Session tests |
| `SessionCreationIntent` | temporary Session aggregate state | durably collect Control inputs plus absent/required/exact claim-fenced application contribution until one finalization CAS consumes it; ordinary Session delete is the only cancellation/terminal command | Session root mutation and creation compiler | runtime specification, external realization, independent TTL state machine, indefinite authoring history, or post-finalization mutation | claim-time input cannot join creation, survives as a second Session authority, or is resurrected after delete | G42; absent/replay/conflict/finalization-consumption/delete-before-contribution tests |
| `SessionBaseline` | Session value object | immutable Environment, Agent/model/runtime, application receipt, delegate/toolset, mount/env/prompt, and baseline fingerprint facts | one creation compiler over Managed/Session/application inputs | Skill/Resource/MCP dynamic state, live handles, protocol DTOs, or a second network authority | a hot attachment mutates a supposedly frozen specification or Skill gains a second owner | G42; immutable-baseline and creation-projection tests |
| `SessionMcpAuthoringContext` | Session baseline value | freeze ordered secret-free compatibility Vault references needed by later canonical full replacement | Session creation request | MCP desired state, material, authorization decision, or mutable Vault snapshot | hot definitions cannot use original compatibility scope or store a second desired list | G42; ordered resolution, new-generation, and no-reselection tests |
| `SessionMcpAttachmentSet` | Session entity state | one revisioned authority for initial and hot MCP generations and lifecycle | exact output of the one Managed MCP normalizer | plaintext, Model/Repository access, independent registry, or Runtime extension vocabulary | initial and hot MCP diverge or stale tool calls reach replacements | G42; root-CAS, generation, add/replace/remove, and recovery tests |
| `McpAttachmentNormalizer` | Managed anti-corruption/domain service | deterministic Agent/Session/application MCP precedence, canonical target, exact credential revision, usage, policy, and payload fingerprint | published Agent binding, compatibility Vault input, secret-free application input | plaintext, Host lookup, realization, or durable state | different ingress paths select different credentials or silently merge collisions | G42; normalization table, ambiguity, revision, and fingerprint tests |
| `SessionMutation` / root-CAS repository operation | Session repository contract | one atomic expected-revision replace/tombstone update of aggregate, idempotency receipt, and lifecycle/outbox facts | `ManagedSessionRepository`, SQLite/Postgres adapters | direct field updates, authorization policy, or per-subaggregate commit authority | Resource/MCP/environment writes overwrite each other, delete loses receipts, or response loss duplicates a command | G42; shared store conformance, delete replay, and two-Control conflict tests |
| `SessionRealizationLease` / `McpRealizationClaim` | Session application state | fence continuing Session projection by opaque Runtime owner/incarnation/epoch independently of a transient Run claim | Session repository; local Host or registered Worker adapter supplies the owner mapping | Run business ownership, Worker protocol vocabulary, credential choice, or live connection | an expired/replaced Host/Worker stages or publishes a generation | G42; local/remote replacement/expiry/restage/orphan tests |
| `McpAttachmentRealizer` | Session application port | one injectable local-or-downstream port for invisible exact-generation staging, post-CAS safe-boundary publication, and idempotent drain | durable generation, Session realization lease, private relay/transport/Runtime refresh | desired-state persistence, credential selection, `SessionRuntime` duplication, or error fallback | a staged route leaks before commit, an injected failure falls back locally, or an old call reaches replacement credentials | G42; local/external selection, no-fallback, stage-crash, stale-CAS disposal, replacement, and stale-call tests |
| `CredentialExecutionPolicy` *(target)* | published value object | exact allowed plaintext-holder trust domains plus `Forbidden`/`VirtualOnly` exposure | existing `CredentialAccess` and `CredentialUsage` | authorization grant, holder ordering/fallback, secret bytes, or protocol-specific duplication | an adapter treats a different trust domain as automatically stronger | target G43; allowed-set, serialization, exposure, and no-fallback tests |
| `PlaintextHolder` *(target)* | credential value object | one exact Workload/Worker/Platform boundary plus opaque trust-domain identity | publication policy and Environment/adapter admission | IAM role/principal, global strength rank, or delivery mechanism | plaintext moves to an unauthorized deployment boundary | target G43; trust-domain mismatch and capability-conformance tests |
| `CredentialRealizationProfile` *(target)* | Environment/deployment execution value | exact inference, MCP, and Resource holders requested by trusted composition and frozen into Session/attempt execution | Environment definition and installed adapter/provider facts | credential policy, runtime ranking, fallback, or secret material | a runtime iterates allowed holders or changes boundary after failure | target G43; deterministic profile, unsupported-holder, and retry-pin tests |
| `CredentialMaterialSource` / `CredentialEnvelope` / resolver *(target)* | credential value objects and port | material resolver location separately from recipient-bound sealed payload reference | exact credential ref, payload fingerprint, and trust-domain recipient | usage semantics, holder authorization, plaintext persistence, or fallback | an envelope is treated as permission, lacks a payload identity, or opens at the wrong boundary | target G43; resolver conformance, schema/fingerprint, recipient, expiry, and replay tests |
| `CredentialRefreshAccess` *(target)* | credential execution value | exact revision/fingerprint and opaque access/refresh/client-secret references for OAuth refresh and reseal | credential publication and material store | MCP URL rediscovery, current-Vault scan, plaintext persistence, or newer-revision adoption | migration deletes refresh support or a reconnect silently changes credentials | target G43; public/confidential refresh, reseal, restart, and exact-revision tests |
| `AttemptCredentialBinding` / realization receipt *(target)* | dispatch attempt-epoch value and execution receipt | atomically pin candidate, revision, holder, planned mechanism, Worker/lease epoch before materialization; record actual mechanism after | published candidate, frozen execution profile, and claim transaction | Model/credential selection, allowed-holder policy authorship, or failure fallback | one attempt changes plaintext boundary after response loss or a stale epoch materializes | target G43; claim atomicity, Native/ACP parity, retry/reclaim, and no-fallback tests |
| `AttemptOwnershipVerifier` | Runtime live port | claim-bound current-attempt verdict | private run-ingress adapter over `DispatchQueue` | claim/epoch, Worker registry, HTTP, database, or product vocabulary | application performs an external effect after losing ownership | cross-backend current/expired/stale tests; signed HTTP test |
| `AttemptExecutorRegistry` | public registry | exact frozen backend-ref to executor mapping and capability export | Native/ACP/A2A executors | arbitrary advertised capability or routing policy | manifest drifts from real execution support | G40; registry/manifest conformance |
| `RunLifecycleFeed` | boundary port | durable cursor over committed Run lifecycle | commit outbox/projection | dispatch lease operations or product status | consumers infer store tables or miss reconnect events | cursor/redelivery tests |
| `DispatchOperationalFeed` | boundary port | durable cursor over claim/lease/settle operations | dispatch store/outbox | Run outcome truth | operational status is mistaken for agent truth | cursor and separation tests |

## 11. Required, Secondary, and Deferred Work

| Priority | Required decision | Completion boundary |
|---|---|---|
| P0 | recovery snapshot/projection, operation receipts, injectable claimed commit, public Worker assembly, topology rejection, conformance + bounded model | remote Worker is correct, recoverable, and embeddable |
| P1 | exact Session executor registry, registered application provisioning/decoration, neutral ownership verification, production identity, separated lifecycle feeds | fleet is extensible and production-operable |
| P2 | PostgreSQL active-active with authoritative recovery/lifecycle reads and multi-process failure injection | the physical Control singleton and sticky-routing constraint are removed |
| Deferred | recovery cache/deltas, snapshot compaction, optional streaming, cell sharding/rebalancing, alternative primary stores, merged business event feed | requires measured scale or a separate accepted ADR |

Accepted ADR-0066 defines a target plus a mandatory contract-closure Slice 0
before feature coding. The target Session has one root revision, a temporary
consumed preparation intent, immutable finalized `SessionBaseline`, existing
`SessionResourceState`, and one `SessionMcpAttachmentSet`. A claim-time
application submits one secret-free contribution through the identity-bound
Worker-to-Control transport; only Control finalizes the baseline and generation
1 state. Initial and hot MCP normalize into the same set; one
`SessionRealizationLease` fences local/remote stage/publish/drain projection.
Managed networking, legacy Sandbox networking, and `deny_egress` normalize once
by safe intersection into the baseline `NetworkPolicy`.
Runtime carries the resulting `EnvironmentSnapshot` intact through `SessionInit`;
Native and ACP consume one Session-owned `SessionEnvironment`. The former
`ThreadEgress`/`ThreadSandbox` registries and their late setters have been
removed.

Accepted ADR-0067 extends existing `CredentialAccess`/`CredentialUsage` with a
material source, recipient-bound sealed payload reference, exact resolver,
optional OAuth refresh/reseal access, and explicit allowed plaintext-holder trust
domains. Holders are not ordered; model exposure is `Forbidden` or
`VirtualOnly`; inference stays authoritative in `ResolvedModelCandidate`, while
the dispatch claim transaction pins each attempt epoch's exact holder and
planned realization before materialization. The root Session mutation,
Environment realization profile, exact material resolver, claim-epoch binding,
and Repository Resource credential pin are implemented slices; G42/G43 remain
target guardrails until the remaining behavior and E2E evidence are complete.
Generic Service, a public generic realizer, automatic LLM Vault authoring,
dynamic HTTP/WebSocket service attachments, general Git service attachments,
and downstream platform custody remain deferred until concrete second
implementations justify their boundaries. Existing Repository Git activation is
a Resource lifecycle and is not deferred by that Service decision.

The first vertical slice is one Worker, one Control Node, one claimed Run that
Awaits, crashes, is reclaimed by another Worker, resumes from a consistent
snapshot, retries a deliberately lost commit response, and settles exactly one
set of committed logical effects.
