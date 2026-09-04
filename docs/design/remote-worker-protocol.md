# Recoverable Remote Worker Protocol

- Status: Accepted
- Date: 2026-08-12
- Decisions: [ADR-0065](../adr/0065-recoverable-embeddable-remote-worker.md),
  [ADR-0075](../adr/0075-unified-managed-session-worker-execution.md)

## End-to-end objective

Awaken exposes one recoverable Worker model for local, self-hosted, and custom
execution. Control owns durable Session and Run truth. A Worker leases work,
realizes the frozen Session projection, executes one claim-fenced attempt, and
returns idempotent mutations. Product applications build complete Session
commands and may decorate attempt inputs/results; they do not extend the Worker
protocol.

## Static structure

```text
Control
├── SessionApplication
│   ├── ManagedSessionRepository
│   ├── EnvironmentExecutionApplication
│   └── SessionRealizationControl
├── Environment WorkQueue
├── WorkerDirectory
├── DispatchQueue
├── RunRecoverySource
└── ClaimedCommitService

Worker
├── WorkerNode
│   ├── WorkerControlClient
│   ├── recovery projection
│   ├── heartbeat / renew / drain
│   └── AttemptExecutorRegistry
│       ├── native
│       ├── acp:<profile>
│       └── a2a:<endpoint>
└── Runtime Host
    ├── frozen Session projection
    ├── Session Environment
    └── optional attempt decorator
```

| Contract | Owner | Meaning |
|---|---|---|
| Work item/lease | Environment WorkQueue | long-lived Session Worker ownership |
| Run claim | DispatchQueue | one attempt under that ownership |
| frozen Session projection | Session aggregate | immutable desired execution state |
| realization lease | Session aggregate | exact physical projection authority |
| recovery view | committed Run truth | cold-Worker restart input |
| attempt decorator | embedding application | neutral envelope/result adaptation only |

## Dynamic behavior

### Session placement

```text
complete create request
  -> SessionApplication resolves and freezes desired state
  -> local Environment: local realization
  -> self-hosted Environment: enqueue stable Work item
  -> Worker claims Work and fetches the Session
```

### Attempt execution

```text
Worker registers and becomes ready
  -> first claims any retry-exhausted Run for terminal resolution
  -> otherwise claims a compatible Run for execution
  -> verifies current owner/epoch/expiry
  -> reads committed recovery prefix
  -> resumes the frozen Session projection
  -> realizes exact Resources/MCP/Environment under its lease
  -> executes snapshot-selected Native/ACP/A2A backend
  -> sends claim-fenced idempotent commits
  -> settles terminal attempt and records completion
```

A retry-exhausted claim bypasses execution placement and materialization. The
Worker uses the same claim-fenced commit transport to append
`Ended(Indeterminate)` and then the same `Done` settlement/tombstone. The
Coordinator-owned clock and retry limit select this claim; a remote Worker never
supplies either value.

The Coordinator's foreground reservation and completion observer share one
environment-free coordination context. It may retain a durable Environment
binding as projection truth, but it never adopts or requires that physical
substrate. Only the claimed Worker's realization path turns the binding into a
process-local owner. This distinction is required after the first Run changes
the request-context projection: rebuilding an observer cache must not turn a
successful resident Worker Environment into a Coordinator execution demand.

The remote Worker pool is the sole retry-exhaustion scheduler in a
coordinator-only topology. Coordinator maintenance repairs already committed
terminals but does not compete for special claims. With no Worker, an expired
row remains unchanged; the first Worker tick resolves it before ordinary work.

### Recovery

- before claim: no Worker effect exists; another Worker may claim;
- after claim, before effect: expiry permits a replacement claim;
- after retry-exhaustion claim, before terminal commit/settle: expiry permits the
  same terminal-resolution command to claim a newer epoch; execution never
  reopens;
- after effect, before commit: exact idempotency identity prevents duplicate
  logical mutation;
- after commit, before response: replay returns the committed receipt;
- after terminal commit, before settle: terminal repair reuses committed truth;
- after Session realization ownership loss: process-local projection is revoked,
  while durable Session truth remains unchanged.

## 7. Authentication and consistency

Every Worker request is bound to the registered identity/incarnation. Run
mutation also requires the exact live claim epoch. Session realization requires
the exact realization lease and generation receipts. No one of those facts
substitutes for another.

### 7.1 PostgreSQL Active-Active Control Nodes

SQLite deployments retain physical single-writer restrictions. PostgreSQL may
serve multiple Control processes, but all observe the same logical CAS,
claim/epoch, idempotency, and completion rules.

## Custom and local Workers

Managed Agents' Environment Work endpoints are the public custom-Worker seam.
An Awaken local Worker may use an in-process adapter; a remote Worker may use
HTTP. Both consume the same Work and frozen Session contracts. Transport choice
does not create a new domain type or a caller-specific Session state.

The optional registered application exposes only the attempt decorator. Local
filesystem setup, process launch, credential materialization, and MCP staging
are Worker effects derived from committed inputs.

## Compatibility

- standard Managed Session creation requires no Awaken-only flag;
- self-hosted/custom Worker queue endpoints retain their Managed semantics;
- unknown historical extension fields are not a required contract and cannot
  place a Session on another execution path;
- usage metering is a cloud concern and is not part of Worker realization;
- Anthropic custom Workers can ignore Awaken's optional Run transport, while an
  Awaken Worker uses it only as an attempt fence.

## Test matrix

| Case | Work | Run claim | Frozen projection | Expected |
|---|---|---|---|---|
| local Session | none | optional local | yes | local phase driver |
| custom Worker | live | n/a | yes | Managed Work execution |
| Awaken remote Worker | live | live exact | yes | resume and execute |
| stale attempt | live | stale/wrong | yes | reject before effects |
| incomplete creation | live | live | no | fail closed |
| response loss | same | same epoch | yes | idempotent replay |
| second foreground Run, Worker Environment resident | live | live exact | yes | Coordinator observes through an environment-free context; Worker reuses/adopts the binding |
| terminal Session | none | none | terminal | no new Work |

## 10. Remote Worker Component Catalog

| Role/component | Owner | Input | Output | Must not own |
|---|---|---|---|---|
| Environment WorkQueue | Coordinator Environment application | frozen self-hosted Session identity | leased Work item | Run outcomes or Worker-local handles |
| Worker directory | Control Worker registry | signed registration/heartbeat | current Worker incarnation/readiness | Session desired state |
| Dispatch queue | Run ingress | immutable Run activation | exact attempt claim/epoch | Session placement or business acceptance |
| Session realization control | Session application | frozen Session plus lease commands | phased directives and durable receipts | local physical effects |
| Runtime Host | Worker | directive, claim, recovery view | local projection and attempt result | Control persistence or desired-state authoring |
| Attempt decorator | embedding application | neutral attempt executor | adapted envelope/result | backend selection, queue, lease, or commit protocol |
