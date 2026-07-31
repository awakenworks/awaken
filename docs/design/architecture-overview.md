# Architecture Overview

This document is the map for `awaken-runtime` implementation. It replaces the
earlier product-first stack with bounded contexts that keep the Apache-licensed
runtime protocol and public contract independent of server, config, admin, and
product code.

The context map below is the accepted ADR-0071 target. Executable Agent
registration, Deployment launch, credential projection, and per-kind File,
Memory, custom-Skill, and Repository realization now have local and distributed
adapters over the same authorities. The Worker retains only ephemeral execution
state and registration-bound clients; it receives no authority database handle.

---

## 1. Context Map

```text
  Control Context
  Agent/Resource-reference authoring, publication history, IAM,
  credential metadata, vaults, product mappings, operator UX
        |
        | immutable publications, exact references, boundary adapters
        v
  Coordinator Context
  executable Agent registration, Deployment/DeploymentRun, Session,
  durable dispatch, committed truth, protocol replay, HTTP/SSE routes
        |
        | bidirectional dispatch/claim and commit/settle protocol
        v
  Worker / Sandbox Context  -- claim-fenced exact reads / CAS write-back -->
  claim-fenced execution, exact credential materialization,                 |
  ephemeral processes and mounts                                            |
        |                                                                   v
        | gated Runtime ports                                     Resources Context
        v                                                File/Memory/Skill/lifecycle,
  Runtime Core Context                                    independent per-kind ports
  AgentRuntime, agent loop, phases, typed state/effects,
  plugin hooks, cancellation, commit boundary, store contracts
```

AllInOne co-locates these components but does not create another bounded
context or implementation. It calls the canonical Control, Coordinator, and
Resources builders; an optional local Worker uses the same `WorkerNodeBuilder`
as the split Worker process.

The Runtime Core is the domain center. It runs tools in-process but must not know
public protocols, registry publication workflow, vault schemas, remote execution
placement, or product-specific session names. Server and product code adapt into
the runtime through explicit ports.

Under the accepted target, config publication is a Control flow, not a runtime
subsystem. Control persists one immutable `StoredPublication`, then invokes
`ExecutableAgentRegistrar::register`. Coordinator stores a rebuildable
`ExecutableAgentCatalog` projection for future Session resolution. The complete
decision and transition plan is
[ADR-0071](../adr/0071-distributed-service-boundaries-and-executable-agent-registration.md).

Contract names follow authority, not implementation convenience. Agent-domain
truth, run-ingress delivery, protocol projection, and concrete stores are
separate boundaries even when one repository or backend implements more than one.
The detailed rule is
[D12 in key-design-decisions.md](key-design-decisions.md#d12---contract-names-follow-authority).

---

## 2. DDD Vocabulary

| DDD concept | Runtime term | Development rule |
|---|---|---|
| Aggregate | `Thread`, `RunRecord`, Agent configuration, Deployment, Session, durable dispatch | Mutate through one consistency boundary; do not update projections as truth |
| Entity | run, thread, message, config record, credential record | Identity is not authorization |
| Value object | `ExecutableAgentSnapshot`, `RunActivation`, `ResolvedSpec`, `BackendProfile`, `StateKey`, effect payload, capability descriptor, content hash | Immutable, serializable where it crosses a boundary |
| Live context | `RuntimeRunContext`, stream/input handles, commit-source wiring | Process-local wiring recreated by the host; never durable request data |
| Domain/application service | resolver, registrar, continuation guard, Outcome controller, permission evaluator, plugin hook runner, terminal observer | Stateless or explicit state dependencies through ports; an application service does not acquire another context's data authority |
| Repository | store traits under the runtime/server contract boundary | No product policy inside repositories |
| Domain event/fact | committed runtime facts and `EventRecord` values | Emitted after the commit boundary, then projected outward |
| Anti-corruption layer | protocol adapters, external product bridges, A2A/ACP mappers | Translate public names at the edge only |

This vocabulary is intentionally boring. Use it before creating a new role word or
crate name.

## 2.1 Contract Authority Map

| Boundary | Owns | Examples |
|---|---|---|
| Agent-domain contract | replayable agent truth and runtime commit vocabulary | `RunRecord`, durable run lifecycle value, `ThreadCommit`, `CommitCoordinator`, `RuntimeResumeStore`, state/fact/event records |
| Config publication contract | Control-owned records and immutable publication values | `ConfigStore`, `StoredPublication`, `ExecutableAgentSnapshot`, `ExecutableAgentRegistrar` |
| Coordinator execution catalog | rebuildable executable-Agent availability for new Sessions | `ExecutableAgentCatalog`, current/exact-revision/fingerprint reads, local/HTTP/PostgreSQL registrar adapters, authenticated private router, and durable command replay |
| Runtime-facing contract | immutable values and ports used to prepare and execute one Run | `ExecutableAgentSnapshot`, `RunActivation`, `RuntimeRunContext`, `RunExecutor`, `RuntimeCapabilitySource`, `PluginManifest` |
| Runtime implementation | live execution behavior over agent-domain vocabulary | agent loop, resolver implementation, provider routing, plugin execution, retry/backoff modules |
| Run-ingress contract | durable delivery and dispatch vocabulary | submit/input records, dispatch records, claims, leases, wake hints, live-command delivery stores |
| Run-ingress implementation | buffering, host supervision, recovery, and live delivery | `DurableRunIngress`, input buffer, dispatch coordinator, recovery replay |
| Protocol projection | public protocol and product-facing replay shapes outside the runtime slice | replay rows, protocol status names, DTOs when a protocol slice is added |
| Concrete stores | backend implementations of multiple ports | SQL/in-memory adapters that implement both agent-truth and ingress stores |

If a type describes durable agent truth, it belongs to the agent-domain contract.
If it describes config records, snapshots, or publication identity, it belongs
to the config publication contract. If it describes Coordinator availability of
an exact executable snapshot, it belongs to the execution catalog projection. If
it describes runtime entry or configuration-surface inspection, it belongs to
the runtime-facing contract. If it describes delivery,
claim, lease, or wake mechanics, it belongs to run ingress. If it describes
public names or protocol replay rows, it is a projection and stays out of the
runtime contract.

---

## 3. Runtime / Server Boundary

The server consumes the runtime through one gated port:

```text
AgentRuntime
CommitCoordinator + contract::store traits
ResolvedSpec (serializable config edge; ResolvedRun is runtime-internal — ADR-0002)
RunExecutor / LiveRunControl / RunResolver / CommitCoordinatorSource
ExecutableAgentSnapshot / RunActivation / RuntimeRunContext
RuntimeCapabilitySource / PluginManifest
StreamSink
Plugin / Contributions / ResolvedExecutionEnv
RunActivation / RuntimeRunContext
StateKey / effect staging
```

The runtime core does not ship concrete model-callable tool ids. Official
first-party tools are runtime extensions, with `awaken-ext-builtin-tools`
providing hand tools, task tools, and the single `agent_run` delegation tool.
The core owns descriptor, registry, resolver, permission, execution, and commit
semantics; extension packages own concrete tool ids and any environment-specific
execution assumptions.

Concrete durable dispatch, transport encoders, config publication coordination,
registry compilation, admin routes, and protocol replay stay in the
server/config/project layer. A runtime crate may not import
dispatch/server/product contracts except through explicitly approved store
implementation bridges.

The config-to-execution edge is data-only. Control produces serializable resolved
data and executable snapshots with fingerprints. Coordinator registers the exact
snapshot and carries it into Session and dispatch data. The Worker builds live
execution objects and validates the fingerprint.
No `Arc<dyn ...>`, live registry set, config CRUD handle, admin workflow, or
product DTO crosses into the runtime core. `AgentId` is not enough to identify
the executable configuration for a run; `ExecutableAgentSnapshot` is the
run/thread configuration identity.

The exact role split is defined in
[runtime-interface-boundaries.md](runtime-interface-boundaries.md). Use those
role names when adding server/runtime code; broad controller names are only
explanatory aliases.

The complete sequence from persisted configuration through run parsing,
resolution, execution, and commit is defined in
[config-to-run-execution-flow.md](config-to-run-execution-flow.md). That flow is
the reference for deciding whether a value is config data, activation data,
resolved runtime input, live execution wiring, or committed truth.

Real-cluster deployment fixtures and their non-overlapping ownership rules are
defined in the [K3D distributed test topology guide](../../deploy/k3d/README.md).
The guide is the fixture authority; architecture documents do not duplicate its
cluster lifecycle or manifest layout.

---

## 4. Dispatch Boundary

Server run ingress has one public entrypoint with two implementations:

```text
RunIngress
  |- DirectRunIngress   -> direct runtime submission/control
  `- DurableRunIngress  -> durable buffered submission/control
```

`DirectRunIngress` is intentionally weak: it projects runtime execution and live
control through `RunExecutor` and `LiveRunControl`. It has no durable queue,
replay, recovery, supersede, or scheduled-wake guarantees.

`DurableRunIngress` is stronger: input first enters the durable input buffer, which
claims dispatches, freezes pending messages, materializes resolved config data,
recovers wake hints, and then activates runtime execution through the narrow
runtime roles.

Routes choose behavior from `RunIngressCapabilities` and fail closed when a
durable-only operation is requested through the weak ingress.

Runtime behavior that needs durable wakeup, such as scheduled actions,
reminders, deferred tools, awaiting runs, or cancellation from an external client,
enters through the same ingress boundary. The runtime owns the command semantics;
the server owns durable delivery and wake reconciliation.

---

## 5. Product Boundary

Product protocols are downstream specializations. The runtime and server
substrate stays generic; downstream products own:

- public DTOs, routes, beta headers, event names, and public error schemas;
- hosted tenancy, quota, billing, sharing, and operator workflow;
- `awaken-admin-assistant-tools`, admin assistant registries, audit semantics,
  and publish/draft workflows;
- vault schemas, OAuth refresh policy, credential handling policy;
- resource data-plane contents and artifact lifecycle;
- Anthropic outcome fields and product completion semantics.

The anti-corruption layer maps those product terms to neutral runtime values and
back. Product names such as `requires_action`, product sessions, or
Anthropic outcome result enums must not appear in the runtime core.

---

## 6. Capability And Resource Boundary

Capabilities are split into segments:

| Segment | Owner | Runtime role |
|---|---|---|
| Decision-surface descriptor | control/server | pinned in `ResolvedSpec` and fingerprinted |
| Execution behavior | runtime/extension | concrete tools run in-process, invoked by id |
| Operator overlay | config/admin/product | mutable permission and visibility policy |
| Secrets/credentials | data plane/product | referenced opaquely, never embedded |
| Session data | runtime facts | replayed from committed state |

This keeps replayability simple: the runtime validates what the model saw and the
content hash of execution material, and runs the tool in-process. Out-of-process
or remote agent execution is added only when a future ADR introduces it.

---

## 7. Development Flow

For every change:

1. Pick the bounded context first.
2. Name the aggregate/entity/value object being changed.
3. Reuse an existing port before adding a new one.
4. Add only the first vertical slice needed to make the behavior executable.
5. Add the guardrail test or dependency check from [INVARIANTS](../INVARIANTS.md).
6. Keep projections and public DTOs out of the core domain.

Designs that skip this flow are not ready to guide implementation.
