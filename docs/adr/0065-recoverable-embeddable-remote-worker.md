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
