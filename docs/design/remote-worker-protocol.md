# Recoverable Remote Worker Protocol

- Status: Proposed P0/P1/P2 implementation design
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
back to the coordinator. An embedding application supplies only an execution
decorator.

This design closes the Worker protocol. It does not move product semantics into
Awaken. Flow continues to own Issue, Workflow, WorkUnit, acceptance, its
execution envelope, MCP capability tokens, Project/Actor/Resource policy,
`ResourceEffect`, and the conversion from technical Run success to business
success.

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
└── application-supplied RunAttemptExecutor decorator
    └── Flow envelope / MCP / resource / output adapter
```

### 3.1 Ownership and dependency rules

| Boundary | Owns | Depends on | Must not own |
|---|---|---|---|
| Control Node | claim authority, committed truth, receipts, durable feeds | store adapters and authenticators | product envelope or business acceptance |
| Worker Node | attempt lifecycle and local execution | Control transport, executor registry, recovery cache | authoritative facts or direct Control database access |
| Store adapter | transaction/CAS/snapshot implementation | chosen storage medium | Worker placement or product policy |
| Application decorator | business input/output adaptation and ACL | public attempt context | registration, lease, recovery, commit protocol |

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
used by that process. The service does not construct `SharedHost`, choose a
database, or own an application projection. The public router accepts only
versioned `CommitOperation`; raw claimed `ThreadCommit` remains a compatibility
surface and is not part of the embeddable protocol.

### 4.5 Public Worker assembly

```rust
WorkerNodeBuilder::new(upstream)
    .with_manifest(manifest)
    .with_attempt_executor(executor)
    .with_inference_materializer(materializer)
    .with_resource_plane(WorkerResourcePlane::new(resources, validator))
    .build()?
    .run_until_shutdown()
    .await
```

`build()` is synchronous and side-effect free: it validates the upstream and
immutable manifest before registration. `run_until_shutdown()` owns process
signals; supervisors and conformance tests use the same `WorkerNode::run_until`
state machine with an injected shutdown future. `SharedHost::with_attempt_executor`
replaces the complete per-Session attempt boundary, so an application decorator
wraps Native/ACP/A2A selection rather than accidentally decorating only one
backend.

`WorkerNode` owns:

- register and incarnation;
- Ready/Draining/Quiesced heartbeats;
- compatible claim and recovery snapshot fetch;
- lease renewal while the attempt is active;
- claimed commit and local projection advancement;
- settle or typed abandon;
- drain admission fence, quiesce, deregistration, and shutdown.

The injected executor receives only a validated attempt context. A decorator may
add Flow envelope parsing, Run-scoped MCP, resource delivery, and output
projection, but cannot bypass claim, recovery, commit, or settlement.

### 4.6 Topology validation

Assembly fails closed for:

- remote upstream plus a Worker-local/shared Control database commit path;
- a remote commit client without a recovery source;
- a manifest capability that no registered executor can implement;
- SQLite configured with more than one physical committed-truth writer;
- active-active PostgreSQL while authoritative recovery still depends on a
  process-local projection.

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

### 6.1 Attempt executor registry

`AttemptExecutorRegistry` is public and selects the frozen `backend_ref` by exact
registered key:

```text
native
acp:claude
acp:codex
acp:gemini
acp:opencode
a2a:<endpoint-or-profile>
```

The Worker manifest is derived from the registry. Configuration may narrow
advertisement but cannot add an unimplemented capability. Duplicate keys,
ambiguous matchers, unknown refs, or registry/manifest drift fail at build or
registration time.

### 6.2 Worker identity

The header-only worker id remains local/test configuration. Production adapters
provide:

- mTLS identity bound to `worker_id` and incarnation;
- short-lived signed Worker leases or registration credentials;
- bootstrap, rotation, revocation, and expiry;
- one client/server identity configuration object;
- redacted audit fields and replay protection.

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
    ) -> LifecyclePage;
}

trait DispatchOperationalFeed {
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
    ) -> DispatchPage;
}
```

The Run feed contains committed `running`, `awaiting`, `resumed`, `completed`,
`failed`, and `cancelled` transitions. The dispatch feed contains `claimed`,
`reclaimed`, `lease_lost`, `settled`, and `dead_lettered`. Consumers persist
their cursor and backfill after reconnect. Live notification is only a wake
hint; it never replaces the durable cursor.

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

### 7.2 Recovery optimization

After full snapshots are correct:

- serve checkpoint plus ordered committed deltas;
- bound and compress snapshot size;
- compact state without discarding authoritative history;
- cache by `(thread_id, thread_version, store_cursor)`;
- invalidate or reject a cache entry whose version is not authoritative.

### 7.3 API/config cleanup

- core assembly accepts explicit typed configuration; CLI adapters read env;
- remove process-level shared-store initialization where dependency injection
  is sufficient;
- expose the Worker lifecycle state machine as a reusable component;
- optional gRPC/bidirectional streaming may batch renew/commit/feed traffic but
  must carry the same claim, epoch, operation id, version, hash, and receipt.

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

Add a small `RemoteWorkerProtocol.tla` model with:

- variables for Worker incarnation/state, dispatch owner/epoch/lease/status,
  committed thread version/Run state/receipt map, Worker projection version, and
  pending input/cancellation;
- actions for Register, Heartbeat, Claim, Snapshot, Execute, CommitRequest,
  CommitApply, ResponseLost, Retry, Renew, Expire, Reclaim, Settle, Abandon, and
  Drain.

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
- two-Control-node PostgreSQL tests in P2;
- property tests for operation id/hash/version rules;
- concurrency model checking for process-local projection code where applicable.

TLC results must state finite bounds and cannot be described as an unbounded
liveness proof.

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
| `WorkerNodeBuilder` | assembly API | validated explicit Worker dependency assembly | manifest, executor, materializers/resources | environment parsing or hidden global stores | invalid topology starts successfully | construction/fail-closed topology tests |
| `AttemptExecutorRegistry` | public registry | exact frozen backend-ref to executor mapping and capability export | Native/ACP/A2A executors | arbitrary advertised capability or routing policy | manifest drifts from real execution support | G40; registry/manifest conformance |
| `RunLifecycleFeed` | boundary port | durable cursor over committed Run lifecycle | commit outbox/projection | dispatch lease operations or product status | consumers infer store tables or miss reconnect events | cursor/redelivery tests |
| `DispatchOperationalFeed` | boundary port | durable cursor over claim/lease/settle operations | dispatch store/outbox | Run outcome truth | operational status is mistaken for agent truth | cursor and separation tests |

## 11. Required, Secondary, and Deferred Work

| Priority | Required decision | Completion boundary |
|---|---|---|
| P0 | recovery snapshot/projection, operation receipts, injectable claimed commit, public Worker assembly, topology rejection, conformance + bounded model | remote Worker is correct, recoverable, and embeddable |
| P1 | exact executor registry, derived manifest, production identity, separated lifecycle feeds | fleet is extensible and production-operable |
| P2 | PostgreSQL active-active, cache/delta optimization, explicit config cleanup, optional streaming transport | physical singleton and performance constraints are removed |
| Deferred | cell sharding/rebalancing, alternative primary store adapters, a merged business event feed | requires measured scale or a separate accepted ADR |

The first vertical slice is one Worker, one Control Node, one claimed Run that
Awaits, crashes, is reclaimed by another Worker, resumes from a consistent
snapshot, retries a deliberately lost commit response, and settles exactly one
set of committed logical effects.
