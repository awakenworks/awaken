# ADR-0065: Recoverable and Embeddable Remote Worker Protocol

- Status: Accepted
- Date: 2026-07-23
- Builds on: [ADR-0006](0006-fact-authority-run-record-is-cache.md)
  (committed facts are read authority),
  [ADR-0019](0019-distributed-dispatch-and-wake-signal.md) (distributed claim
  and lease renewal), [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md)
  (commit/read ports and fact-authority reads), and
  [ADR-0060](0060-durable-dispatch-completion-tombstone.md) (durable completion
  tombstone)
- Detailed design:
  [Recoverable Remote Worker Protocol](../design/remote-worker-protocol.md)

## Context

Awaken already has registered Workers, heartbeat/drain/quiesce, capability
placement, HTTP claim/renew/settle/checkpoint, claim-epoch fencing, remote
claimed commit, Native/ACP/A2A executors, and durable dispatch. These mechanisms
form most of a remote Worker, but their current composition is not yet a
recoverable and embeddable public component.

The remote commit host is write-only: it cannot supply committed messages,
state, Run records, or resume tickets. A newly claimed Worker therefore cannot
reconstruct a committed Run after Awaiting or a previous Worker crash. The
claimed-commit router also fixes its commit applier to `SharedHost`, and the
worker executable fixes its private loop and executor selection. An embedding
application must currently copy generic Worker transport and lifecycle code to
add its own claim-bound Session provisioning and execution adaptation.

PostgreSQL transactions do not remove these gaps. They make one database
transaction atomic and isolated, but they do not refresh another process's
in-memory projection, make a response-loss retry idempotent, or prove that a
snapshot assembled by several reads is one committed prefix. The required
authority is therefore a **logical committed-truth protocol**, not necessarily
one physical Coordinator process. SQLite uses one physical writer; PostgreSQL may later
run several Coordinator replicas if every replica implements the same protocol and reads
authoritative state.

## Decision

### D1: The coordinator owns committed truth; Workers own execution caches

Every remote `ThreadCommit` is submitted through a claim-fenced coordinator
endpoint. A Worker never opens or writes the Coordinator cell's agent-truth or
dispatch database. The coordinator validates ownership and versions, performs
the durable commit, and returns a receipt. A Worker may keep a local
`RecoveryProjection`, but that projection is read-only execution context and is
never authoritative.

“Coordinator-owned” is logical:

- SQLite has one physical Coordinator writer per cell.
- PostgreSQL initially uses the same deployment shape, but the protocol does not
  rely on sticky routing or one process-local projection.
- Multiple active Coordinator replicas are allowed only after authoritative recovery
  reads, durable idempotency receipts, and cross-node conformance are complete.

### D2: Claim and recovery are separate correctness steps

Claim returns the fenced dispatch ownership. Before execution, the Worker must
fetch a `RunRecoverySnapshot` authorized by that exact claim. The snapshot is
materialized from one consistent committed-log prefix and contains the claimed
Run's required Run records, messages, state commands, resume tickets, per-thread
version, and store cursor.

Failure to obtain or validate the snapshot forbids executor entry. The Worker
uses a fenced abandon operation or lets the lease expire; it does not execute a
fresh Run as fallback. A future claim-with-recovery response may combine the
round trips, but it must preserve these two domain steps.

### D3: Epoch fencing, optimistic concurrency, and idempotency are distinct

Each claimed commit carries:

- the worker identity and incarnation;
- the claim owner and epoch, which authorize the attempt;
- a stable `CommitOperationId { run_id, ordinal }`, which identifies the logical
  operation across reclaim;
- `expected_thread_version`, which prevents committing from stale recovery
  context;
- a payload hash, which detects reuse of an operation id for different facts.

The claim epoch is never part of the idempotency key. The service holds an exact
epoch guard for the entire coordinator transaction; when dispatch and truth
share a database this may be one transaction, otherwise the guard prevents
reclaim while the coordinator atomically checks the receipt/version and writes
the commit/receipt. No two-phase commit is required because the operation does
not also mutate dispatch state. Under a valid claim, repeating the same
operation and hash returns the original receipt; reusing the id with another
hash fails closed; a stale epoch cannot mutate truth; a version conflict
requires recovery refresh.

The system promises at-least-once execution and idempotent committed effects. It
does not promise exactly-once execution.

### D4: Claimed commit and Worker assembly are injectable public components

The claimed-commit service accepts explicit `DispatchQueue`,
`CommitCoordinator` (or coordinator resolver), `WorkerDirectory`, and
`WorkerRequestAuthenticator` dependencies. Its router is a thin adapter over
that service and does not manufacture a private `SharedHost` commit path.

A public `WorkerNodeBuilder` assembles registration, heartbeat, claim, recovery,
lease renewal, commit, settle/abandon, drain, quiesce, and deregistration. The
application supplies one registration-time application factory. After the
Coordinator allocates the exact Worker incarnation, the factory receives an
immutable `RegisteredWorkerContext` carrying that registration and the same
identity-bound transport used by Worker control. It returns one
`RegisteredWorkerApplication`: an optional claim-time
`ApplicationSessionProvisioner` plus the decorator that wraps each Session's
built-in Native/ACP/A2A `RunAttemptExecutor` router.

An embedding composition root installs the existing typed `DeploymentConfig`
and its materializers/Resources ports on `WorkerNodeBuilder`, then selects
`with_standard_manifest(application_capabilities)`. `build()` is the sole
standard manifest derivation point; the same installed values configure Host
routing, so advertised inference/resource capabilities, ACP routes, their
default, and sandbox selection cannot drift through a second assembly path.
Downstream deployments may additionally inject the existing
`ContainerEnvironmentProvider` through
`with_session_container_provider`; its backend id and capability evidence then
replace deployment-derived Sandbox inference in both the manifest and Host. It
is not a second credential or environment lifecycle port.
Special deployments retain `with_manifest(explicit_manifest)`, but the two
manifest sources are mutually exclusive and share contract validation.

The provisioner returns only a frozen, secret-free `ApplicationSessionPlan` of
neutral mounts, environment values, prompt context, MCP inputs, and a network
restriction. Because it is produced after claim, the Worker submits the complete
plan as one claim-fenced `ApplicationSessionContribution` to the Coordinator-owned
Session application service while the Session is `Preparing`. The Control edge
verifies the claim/epoch and fingerprint; the one Session creation compiler then
consumes Control and application inputs, freezes the baseline, and creates
generation 1 Resource/MCP state in one root CAS before environment realization.
Native and ACP consume the same resulting `SessionEnvironment` and attachment
projection.

The application cannot create another sandbox, executor registry, MCP registry,
or Worker-local Session overlay. Re-delivery of the same plan fingerprint is
idempotent, while a different plan fails closed. Runtime MCP enters Native/ACP
only through the exact-generation `SessionRuntime` stage/publish/drain path in
ADR-0066; the application neither constructs `PreparedMcpServer` nor opens a
competing connection.

Run ingress captures the exact claim behind the neutral
`AttemptOwnershipVerifier` installed in `RuntimeRunContext`. Application code can
therefore fail closed immediately before an external side effect without
receiving dispatch, Worker, HTTP, or database types. Product envelopes, MCP
capability tokens, resource ACLs, and business outcome mapping remain
application-owned projections/decorators and do not enter the Worker protocol.

### D5: Extensibility and active-active operation are phased

P0 closes recovery correctness, commit idempotency, dependency injection,
public Worker assembly, invalid-topology rejection, conformance tests, and a
bounded formal model.

P1 adds a public exact-match `AttemptExecutorRegistry` used by the authoritative
Session router, production Worker identity, registered application decoration,
neutral current-attempt ownership checks, and durable lifecycle feeds split
between committed Run truth and dispatch operations. Dispatch operation rows
carry their store-assigned durable wall-clock time so downstream audit and
elapsed-time projections reuse the claim/settle authority instead of creating
a parallel Worker lifecycle. Timestamp-free legacy rows remain readable for
state reconstruction but cannot authorize a fabricated duration.

P2 permits PostgreSQL active-active Coordinator replicas and makes their asynchronous
recovery and lifecycle reads authoritative. Versioned recovery caches/deltas,
snapshot compaction, further configuration cleanup, and streaming transports
are optional follow-up optimizations: they may reduce cost, but are not allowed
to weaken or redefine the P0 protocol invariants.

## Consequences

- A Run that awaited or lost its Worker can resume on another database-less
  Worker from committed truth.
- Response loss no longer turns a nonterminal commit retry into duplicate facts.
- SQLite keeps its simple single-writer cell. PostgreSQL can later remove the
  physical singleton without introducing direct Worker database writes.
- Embedding systems such as Flow supply only their registered application
  projection and execution decorator; they do not own generic Session
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
registry inside the Session router, registered Worker application factory,
claim-bound Session plan, neutral ownership verification, and the separated
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

## Amendment: application input is a claim-fenced Session contribution (2026-07-25)

[ADR-0066](0066-session-service-binding-and-realization.md) replaces the
`ApplicationSessionPlan.mcp_servers: Vec<PreparedMcpServer>` shape described
above. The registered application retains the ability to contribute initial
mounts, env, prompts, network restriction, and MCP, but now returns secret-free
`mcp_inputs: Vec<McpAttachmentInput>` with the other neutral values. After claim,
the Worker submits the whole plan to Control as one `ApplicationSessionContribution`.
It does not mutate a Worker-local slot. The Control-owned creation compiler
consumes the temporary preparation intent, freezes the baseline, safely
intersects networking, and sends MCP values through the one Managed
`McpAttachmentNormalizer`; they become generation 1 of the durable
`SessionMcpAttachmentSet`.

The application never materializes a bearer, constructs a relay/connection, or
owns MCP desired state. Re-delivery of the complete application plan remains
fingerprint-idempotent. Later add/replace/remove operations are independent
Session commands and never mutate or resubmit `ApplicationSessionPlan`.
