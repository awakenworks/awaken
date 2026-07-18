# Architecture Overview

This document is the map for `awaken-runtime` implementation. It replaces the
earlier product-first stack with bounded contexts that keep the Apache-licensed
runtime protocol and public contract independent of server, config, admin, and
product code.

---

## 1. Context Map

```text
  Product / Control Context
  public DTOs, auth, tenant policy, vaults, resource data plane,
  external product mappings, operator UX
        |
        | anti-corruption adapters, public events, opaque references
        v
  Dispatch / Server Context
  RunIngress, DirectRunIngress, DurableRunIngress, protocol replay,
  config publication/materialization, snapshot contract adapters,
  HTTP/SSE routes, admin console
        |
        | gated runtime port: RunActivation, RuntimeRunContext, ResolvedSpec,
        | StreamSink,
        | RunExecutor, LiveRunControl, CommitCoordinator,
        | Plugin, RunWithSnapshotExecutor,
        | AgentSnapshotResolver, RuntimeCapabilitySource
        v
  Runtime Core Context
  AgentRuntime, agent loop, phases, tool abstractions, typed state/effects,
  plugin hooks, backend profiles, cancellation/stop policy,
  continuation guards, goal extension, commit boundary, store contracts
        ^
        |
        | tool invocation port (in-process)
        |
  (The runtime invokes tools in-process by id and owns their execution.
   Credential mechanics and any future remote-agent execution stay out of
   scope here.)
```

The Runtime Core is the domain center. It runs tools in-process but must not know
public protocols, registry publication workflow, vault schemas, remote execution
placement, or product-specific session names. Server and product code adapt into
the runtime through explicit ports.

Config publication is an adjacent config-side flow, not a runtime subsystem.
`ConfigPublicationCoordinator` may live in a server/config application package,
and `RegistryCompiler` belongs to the config domain. Their runtime-facing handoff
is a complete catalog install request through `RuntimeCatalogInstaller`.

Contract names follow authority, not implementation convenience. Agent-domain
truth, run-ingress delivery, protocol projection, and concrete stores are
separate boundaries even when one repository or backend implements more than one.
The detailed rule is
[D12 in key-design-decisions.md](key-design-decisions.md#d12---contract-names-follow-authority).

---

## 2. DDD Vocabulary

| DDD concept | Runtime term | Development rule |
|---|---|---|
| Aggregate | `Thread`, `RunRecord`, published config set, durable dispatch | Mutate through one consistency boundary; do not update projections as truth |
| Entity | run, thread, message, config record, credential record | Identity is not authorization |
| Value object | `ExecutableAgentSnapshot`, `RunActivation`, `ResolvedSpec`, `BackendProfile`, `StateKey`, effect payload, capability descriptor, content hash | Immutable, serializable where it crosses a boundary |
| Live context | `RuntimeRunContext`, stream/input handles, commit-source wiring | Process-local wiring recreated by the host; never durable request data |
| Domain service | resolver, continuation guard, permission evaluator, plugin hook runner, registry materializer | Stateless or explicit state dependencies through ports |
| Repository | store traits under the runtime/server contract boundary | No product policy inside repositories |
| Domain event/fact | committed runtime facts and `EventRecord` values | Emitted after the commit boundary, then projected outward |
| Anti-corruption layer | protocol adapters, external product bridges, A2A/ACP mappers | Translate public names at the edge only |

This vocabulary is intentionally boring. Use it before creating a new role word or
crate name.

## 2.1 Contract Authority Map

| Boundary | Owns | Examples |
|---|---|---|
| Agent-domain contract | replayable agent truth and runtime commit vocabulary | `RunRecord`, durable run lifecycle value, `ThreadCommit`, `CommitCoordinator`, `RuntimeResumeStore`, state/fact/event records |
| Config publication contract | config-side records, snapshots, and publication values before runtime install | `ConfigStore`, `ConfigSnapshot`, `RegistryPublication`, `RegistryCompiler` contracts |
| Runtime-facing contract | internal ports and immutable values used to enter, install, or inspect runtime execution | `RunnableConfig` (the bundled run input), `RuntimeCatalogInstaller`, `RuntimeCatalogInstall`, `RunWithSnapshotExecutor`, `ExecutableAgentSnapshot`, `AgentSnapshotResolver`, `AgentSnapshotCatalog`, `RuntimeCapabilitySource`, `PluginManifest` |
| Runtime implementation | live execution behavior over agent-domain vocabulary | agent loop, resolver implementation, provider routing, plugin execution, retry/backoff modules |
| Run-ingress contract | durable delivery and dispatch vocabulary | submit/input records, dispatch records, claims, leases, wake hints, live-command delivery stores |
| Run-ingress implementation | buffering, host supervision, recovery, and live delivery | `DurableRunIngress`, input buffer, dispatch coordinator, recovery replay |
| Protocol projection | public protocol and product-facing replay shapes outside the runtime slice | replay rows, protocol status names, DTOs when a protocol slice is added |
| Concrete stores | backend implementations of multiple ports | SQL/in-memory adapters that implement both agent-truth and ingress stores |

If a type describes durable agent truth, it belongs to the agent-domain contract.
If it describes config records, snapshots, or publication identity before
runtime install, it belongs to the config publication contract. If it describes
internal runtime entry, catalog installation, or configuration-surface
inspection, it belongs to the runtime-facing contract. If it describes delivery,
claim, lease, or wake mechanics, it belongs to run ingress. If it describes
public names or protocol replay rows, it is a projection and stays out of the
runtime contract.

---

## 3. Runtime / Server Boundary

The server consumes the runtime through one gated port:

```text
AgentRuntime
CommitCoordinator + contract::store traits
ResolvedSpec (serializable config edge; ResolvedRun and AgentResolver are runtime-internal — ADR-0002, D3)
RunExecutor / LiveRunControl / RunResolver / CommitCoordinatorSource
RuntimeCatalogInstaller / RunWithSnapshotExecutor
AgentSnapshotResolver / AgentSnapshotCatalog
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

The config-to-kernel edge is data-only. The config domain produces serializable
resolved data and executable snapshots with catalog fingerprints. The kernel
builds live execution objects from its own catalog and validates the fingerprint.
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
