# ADR-0073: Session Environment–Owned Hand and Worker Capability Placement

- Status: Accepted
- Date: 2026-08-01
- Supersedes: ADR-0046's per-Run `ToolExecutorProvider` placement and Agent
  `declared_hand`/deployment `hand_connections` configuration
- Amends: ADR-0044; its `ToolExecutor` and relay remain reusable mechanisms, but
  a production Session obtains a Hand only from its realized Environment
- Depends on: ADR-0065, ADR-0066, ADR-0071, ADR-0072

## Context

Ingress protocols are resident Coordinator adapters. They translate HTTP/SSE or
JSON-RPC into the same protocol-neutral Session/Run application ports; they are
not execution locations. A durable claim selects a Worker. That Worker realizes
the frozen Environment and executes the selected backend:

- Native: model loop plus Sandbox-target Hand tools;
- ACP: an opaque CLI process inside the same Environment;
- outbound A2A: remote network IO, with no local Hand or Environment.

The previous design also allowed a Host-global or per-Run Hand chosen from live
Agent configuration. This was a second placement authority beside durable Worker
placement and a second Hand owner beside SessionEnvironment. It could route
Native filesystem effects outside the Environment shared with ACP.

## Decision

### Static structure

| Bounded context | Owns | Depends on |
|---|---|---|
| Protocol adapters | external method/path, wire validation and projection | `RunApplication` from `awaken-session-contract` |
| Coordinator | Session/Run orchestration, durable dispatch, Worker eligibility and ranking | immutable Agent, Environment and Resource snapshots |
| Worker | claimed attempt, capability evidence and SessionEnvironment realization | provisioning/runtime contracts |
| SessionEnvironment | one live sandbox and its one `ToolExecutor` | Workdir, Namespace or Container provider |
| Native runtime | brain loop and target-based tool routing | Session `tool_executor` for Sandbox tools |
| ACP adapter | opaque process launch | the same SessionEnvironment |
| outbound A2A adapter | authenticated remote IO | no local SessionEnvironment |

`SandboxRequirements` is the one neutral demand vector. Coordinator derives it
from the frozen Environment and resource access. `SandboxCapabilities` uses the
same monotonic predicate both for provider admission and Worker claim admission.
Ranking can order only Workers that already satisfy this predicate.

There are no `brain worker` and `hand worker` domain types. A Worker is
capability-bearing: it may advertise Native, exact ACP profiles, and generic A2A
transport. Hand is a SessionEnvironment capability, not an independently placed
microservice. AllInOne always starts the same registered WorkerNode; local ACP
discovery only enriches that Worker's capabilities.

### Dynamic behavior

```text
external client
  -> resident protocol adapter
  -> Coordinator Session/Run application
  -> durable dispatch(Agent + Environment + Resources + requirements)
  -> eligible registered Worker claims
  -> Worker realizes one SessionEnvironment when local execution needs it
     -> Native: brain inference; Sandbox tool -> Environment ToolExecutor
     -> ACP: CLI process -> same Environment
     -> outbound A2A: remote HTTP IO; skip local Environment
  -> committed result / awaiting / failed terminal outcome
```

Native `on_tool_use` may defer realization until the first Sandbox-target call.
Concurrent first calls join one lifecycle lock and publish one durable binding.
ACP is eager because the opaque process itself needs the Environment. A2A-only
execution never realizes one; local mounts, repositories, memory or executable
Skills combined with outbound A2A fail before provisioning.

Failures are fail-closed: an insufficient Worker cannot claim; a lost claim is
retried under the existing lease/epoch fence; a failed Environment binding is not
published; a remote A2A transport failure is an attempt failure and never falls
back to local Native execution.

The Environment owns the Hand process and its live channel separately from the
durable Environment identity. If an idle container exec channel expires before a
tool request is written, that same owner stops the stale binding, starts one
replacement Hand in the existing Environment, and retries the undispatched call
once. A channel loss after dispatch remains indeterminate and is never replayed by
this lifecycle path; it continues through ADR-0044's existing recovery boundary.

The Worker-local SessionEnvironment owner proactively hibernates that same
recreatable Hand after its configured process-inactivity horizon. This policy is
based on actual Hand use, not Coordinator Session status: a long model turn or a
Session awaiting human/tool action may safely release an unused Hand. Each call
advances a local generation, so an old deadline cannot stop a newer invocation;
the same binding mutex serializes deadline, dispatch, replacement, and terminal
stop. Hibernation releases only the process/channel; the Environment, opaque
durable binding, and workspace remain resident, and the next tool call uses the
same serialized replacement path.

Process release reuses the ACP supervisor's one bounded `TERM -> KILL -> wait`
signal ladder. If the provider cannot prove the old process was reaped, the owner
fails closed and does not launch a possibly concurrent replacement. Terminal
Session release remains a different operation: it closes the owner and disposes
the Environment, so it can never lazily recreate a Hand. Full Pod/container
suspension is disabled while the workspace is backed by ephemeral storage;
reclaiming it would violate continuity.

## Consequences

- Protocol availability is independent of Worker placement: external clients
  always enter through a resident Coordinator adapter.
- Native and ACP share one Environment and filesystem truth; outbound A2A owns
  neither, so it cannot silently accept local mounts or executable resources.
- Worker eligibility becomes stricter but explicit: every sandbox capability is
  checked before claim, while ranking remains replaceable after eligibility.
- Removing Agent/deployment Hand coordinates is a compatibility break for those
  internal configuration fields; no shadow migration path remains.
- Brain and Hand stay runtime roles rather than independently scheduled Worker
  kinds, keeping the service model smaller and avoiding cross-Worker Session
  consistency.

## Required invariants and tests

1. Every Session has at most one Environment and at most one Environment-derived
   Hand.
2. Every public normalized `method + path` has one adapter owner.
3. Every claim uses one `SandboxRequirements`/`SandboxCapabilities` predicate.
4. Agent publications and deployment config carry no Hand placement coordinate.
5. AllInOne has one registered Worker execution owner even without ACP.
6. A Session Environment may replace an expired Hand channel, but never owns more
   than one active binding and never replays a possibly dispatched tool call.
7. Idle hibernation is reversible and retains the Environment; terminal release is
   irreversible and never recreates a Hand. A provider may not suspend an
   Environment until its workspace has a durable continuation contract.
8. Hand inactivity and residency are Worker-local runtime facts. The Coordinator
   owns no Hand timer, scan, registry, or durable Hand-state replica.
9. A failed process reap closes the Hand owner; replacement is permitted only
   after absence of the previous process is known.

Cause/effect decision tables live beside the corresponding Rust/Python tests.
The architecture fitness suite rejects retired Hand selection symbols and
duplicate public route ownership.

## Reuse, modification and new code

- Reused unchanged: `ToolExecutor`, tool relay/channel framing, durable dispatch,
  Worker registry/ranking, Sandbox providers, Native/ACP/A2A attempt executors.
- Modified: Runtime Host Session context, Worker placement contract, AllInOne
  assembly, Coordinator ranking module, Agent/config registration shapes.
- New: neutral `SandboxRequirements` and execution/route ownership fitness checks.

No separate Brain Worker or Hand Worker service is introduced.

## 2026-08-10 amendment: resource demand is part of neutral placement

`SandboxRequirements` remains the single capability predicate, but capability
and capacity answer different questions. A frozen Environment now carries
provider-neutral `ResourceRequests` separately from enforceable
`ResourceLimits`. The exact requests participate in
`SandboxCapacityShapeId`, Kubernetes Pod scheduling, and Worker eligibility.

`WorkerCapacity.resources`, when set, is the immutable per-sandbox allocatable
ceiling of one Worker incarnation, not aggregate cluster inventory and not a
billing record. A Worker whose explicit ceiling cannot satisfy a request is
ineligible. An entirely omitted ceiling delegates resource feasibility to the
selected sandbox backend; this is the Kubernetes posture because Pod scheduling
and node inventory are cluster-dynamic. `max_concurrent` continues to bound
simultaneous claims. Kubernetes remains the authority for node-level bin packing
from Pod requests, while the Coordinator remains the authority for claim-fenced
Worker assignment.

This reuses the existing `SandboxSpec`, capacity-shape derivation,
`PlacementRequirements`, `WorkerCapacity`, and compatibility kernel. It modifies
those contracts and the Kubernetes adapter; the only new value object is the
neutral `ResourceRequests`. Product plans, prices, tenant tiers, cloud node
costs, and autoscaler policy remain outside this repository.

## 2026-08-14 amendment: resident Hand continuity and opaque ACP recovery

Kubernetes container environments may keep the Hand listener and its operation
ledger inside the Session-owned Pod. This is a different realization of the
existing SessionEnvironment-owned Hand, not a separately placed Hand service.
The Pod exposes no Service. An eligible Worker opens the private channel through
the provider's existing environment capability after validating the exact
Sandbox handle, Session generation, image evidence and current claim epoch.

The resident Hand ledger is the sole effect fence for its tool calls. A stable
operation id admits one executor, lets reconnecting callers join a live
operation, and returns a process-local completed result without executing again.
Durable files contain only claim/completion fencing metadata, never a
`HandResult`, tool output, provider credential, or MCP credential: another ACP
or tool process in the sandbox may share the same uid and must gain no secret by
reading the ledger directory. Any claim recovered after Hand process loss,
whether or not the former process reached its completion marker, is
`Indeterminate`; it is never replayed because its result deliberately cannot be
recovered from disk. The Hand still owns no model client, commit authority,
Session lifecycle state, provider credential or runtime database.

Worker failure therefore has two distinct outcomes:

1. the Session Pod survives: a higher-epoch replacement Worker adopts the same
   Sandbox and reopens the resident Hand channel; and
2. the Session Pod is gone: the provider fails continuity and the existing
   Session/Sandbox recovery policy decides whether to restore or terminate.

ACP is an opaque external executor and does not expose a durable turn receipt.
The existing same-process retry remains limited to a proven pre-session
handshake failure. A cross-process recovered ACP claim has no sound way to prove
whether its prompt or an MCP effect was dispatched, so the current claim owner
commits the existing `EndCause::Indeterminate` and settles the dispatch without
relaunching the prompt. No `AcpAttemptStage`, parallel protocol journal or new
terminal variant is introduced. A future resumable ACP path must be explicitly
capability-gated by an official idempotent receipt.

The default container realization remains attached exec. Resident selection is
admissible only when the provider truthfully exposes a reopenable channel and a
durable operation ledger. Unknown selections and resident mode on non-Kubernetes
providers fail before capability advertisement or Sandbox creation.

## 2026-08-15 amendment: recovery demand is a Worker claim requirement

An immutable Agent snapshot that selects `DurableRequest` for a canonical Hand
tool creates a hard Worker-placement demand. `PlacementRequirements` carries the
selected recovery modes and `WorkerManifest` carries the recovery capability
derived from the same typed `ContainerHandResidency` value that constructs the
SessionEnvironment executor. The existing `can_claim` compatibility kernel uses
`ToolRecoveryMode::is_supported_by`; no string capability, second registry, or
product-owned recovery vocabulary is introduced.

An incompatible Worker cannot claim the Run. Absence of a compatible Worker
therefore leaves the durable dispatch Pending so deployment repair or scale-out
can restore availability without weakening effect safety. Explicit manifests
may not advertise a recovery capability different from the installed deployment
topology. Runtime still validates the realized executor before entering it,
because a serialized manifest cannot prove the health of a live provider.

Recovery after dispatch remains unchanged: a surviving resident Hand can join
the stable request, while loss of the Hand process or its result is
`Indeterminate` and is never converted into a replay merely to preserve
availability.
