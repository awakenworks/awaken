# Architecture Overview

This document is the map for `awaken-runtime` implementation. It replaces the
earlier product-first stack with bounded contexts that keep the Apache-licensed
runtime protocol and public contract independent of server, config, admin, and
product code.

The context map below is the implemented ADR-0071 boundary. Static definitions
and immutable relationships belong to Control; runtime coordination belongs to
Coordinator; resource data and reclamation belong to Resources; execution belongs
to Worker. Local and distributed adapters reach the same authorities. Worker
retains only ephemeral execution state and receives no authority database handle.

---

## 1. Context Map

```text
  Control Context
  Agent and Environment authoring, immutable revisions/publications,
  model-candidate resolution, Resource references, IAM, credential metadata,
  vaults, directory APIs
        |
        | exact executable Agent/Environment registrations
        v
  Coordinator Context
  rebuildable executable projections, Deployment/DeploymentRun,
  Session/Run, WorkQueue, durable dispatch, committed truth, protocol replay
        |
        | dispatch/claim, exact manifests, commit/settle
        v
  Worker Context  -- claim-fenced exact reads / CAS write-back --> Resources Context
  sandbox/process/mount lifecycle, exact credential             Resource Catalog,
  materialization, Runtime execution, ephemeral state            File/Memory/Skill,
        |                                                        references/purge
        v
  Runtime Core
  Agent loop, phases, typed state/effects, plugins, cancellation,
  and commit contracts; embedded by Worker, not a fifth service authority
```

AllInOne co-locates these applications but does not create another bounded
context or implementation. It calls the canonical Control, Coordinator, and
Resources application constructors; an optional local Worker uses the same
`WorkerNodeBuilder` as the split Worker process.

### 1.1 Physical deployment and authority map

| Deployment unit or context | Canonical applications | Durable authority acquired | Cross-context contracts |
|---|---|---|---|
| Control | `awaken_control::build_control_component`, `CatalogModelPublicationResolver`, `GenaiModelDiscovery` | Agent/config publication, Environment definitions/revisions and sandbox-policy versions, Catalog, Credential/secret, Admin, IAM, Data Subject consent/accountability | resolves complete secret-free model candidates once; registers exact executable Agent and Environment facts; exposes authenticated audit, credential, webhook, and consent-read contracts |
| Coordinator | `awaken_coordinator::build_coordinator_component`; currently co-deploys the canonical Resources application | executable-Agent and executable-Environment projections, Deployment/DeploymentRun, Session, WorkQueue, captured content, dispatch/commit | reads only rebuildable Control projections and published candidates; calls narrow Control contracts; dispatches to and settles Workers; mounts Resources services without owning their stores |
| Resources (currently co-deployed with Coordinator) | `awaken_resource_application::ResourcesApplication` over one `ResourceAuthorities` value | Resource Catalog/lifecycle, File logical metadata and immutable blobs, Memory content/history, Skill versions | exposes public application services and claim-fenced per-kind Worker contracts; never opens Control or Coordinator stores |
| Worker | `WorkerNodeBuilder` | none; execution state is ephemeral | claim-fenced Coordinator and per-kind Resources/Credential clients |
| AllInOne | the same Control, Coordinator, Resources, and optional Worker applications | the union of those authorities in one process | local adapters implement the same contracts |

Production process targets are role-named: `awaken-control`,
`awaken-coordinator`, and `awaken-worker`. The `awaken` target remains the
operator/AllInOne launcher and retains `awaken control` / `awaken coordinator`
as compatibility commands. Control and Coordinator targets share the canonical
source-level lifecycle in `awaken-cli`; neither entrypoint reconstructs a domain
application. Worker keeps its independent authority-store-free startup.

Package names make the application and adapter roles explicit even though several
live under the technical `crates/server/` workspace bucket:

| Package | DDD role | Rule |
|---|---|---|
| `awaken-control` | Control application | owns authored facts, Provider discovery, and catalog/credential-backed publication; never schedules Runs |
| `awaken-coordinator` | Coordinator application | owns dynamic scheduling, dispatch, settlement, and replay; consumes immutable model candidates and never opens Catalog or Credential authority |
| `awaken-worker` | Worker process | advertises capabilities and executes claim-fenced work; native, ACP, and outbound A2A are execution adapters, not public ingress owners |
| `awaken-resource-application` | Resources application | owns File, MemoryStore, Skill, and lifecycle services over one `ResourceAuthorities` selection |
| `awaken-protocol-managed` | Complete Anthropic-compatible Managed Public API anti-corruption layer | owns Agent/Session/Environment/File/MemoryStore/Skill/Model wire DTOs and routing only; domain behavior stays behind application contracts |
| `awaken-protocol-awaken` | Awaken extension protocol | owns only explicitly namespaced `/v1/awaken/*` routes |
| `awaken-protocol-a2a`, `-ai-sdk`, `-ag-ui`, `-mcp` | other ingress anti-corruption layers | each method + normalized path has exactly one source owner |

Protocol packages are always-on ingress adapters when mounted by a process. They
do not become services or execution authorities themselves. A protocol handler
validates and translates, calls the owning application service, and projects the
result. Worker-side ACP/A2A adapters are different: they execute an already
admitted claim and never own the public route that admitted it.

`ProcessStores` contains optional `ControlStores` and `CoordinatorStores`
groups; it is not a shared bag of database handles. A split Coordinator receives
no Control Catalog, Credential, Config, Admin, or seal-key value. A split Control
receives no Session/Deployment or Resources content store. The legacy
`ControlStoreConfig` name is only a backend-address compatibility bundle;
role-aware validation and acquisition define authority.

Environment definitions are static Control facts. `awaken-environment-contract`
owns `EnvItem`, immutable `EnvironmentRevision`, and `EnvRegistry`; Control also
owns exact sandbox-policy versions and stores the selected policy reference in
the Environment revision. `awaken-environment-application` owns the sole
`EnvironmentApplication`, which publishes the resolved exact revision through
`ExecutableEnvironmentRegistrar`. Coordinator persists only a
rebuildable executable command log and owns `EnvironmentExecutionApplication`, image
build/realization state, and the WorkQueue. Session admission freezes an
`EnvironmentSnapshot`; Worker consumes that snapshot and never reopens either
authority.

Data Subject follows the same explicit-crossing rule.
`awaken-data-subject-application` is the sole aggregate and use-case owner for
User Profile, consent, enrollment, and accountability-preserving erasure;
`awaken-data-subject-store` contains only the durable SQLite/PostgreSQL adapters.
The Managed protocol projects that application and owns no profile state.
Coordinator reads consent through `DataSubjectConsentSource`, while Control
requests subject-content erasure through an authenticated Coordinator contract. The
single Coordinator application fans that command out to its captured-content
store and optional portable ACP session store; AllInOne calls the same contracts
locally. Aggregate writes use revision-fenced compare-and-swap, so concurrent
profile and consent changes cannot overwrite one another.

The Runtime Core is the domain center. It runs tools in-process but must not know
public protocols, registry publication workflow, vault schemas, remote execution
placement, or product-specific session names. Server and product code adapt into
the runtime through explicit contracts.

### 1.2 Runtime Host, authority, and protocol responsibilities

`awaken-runtime-host` owns execution semantics, Session environments, Worker
transport adapters, and the `RuntimeAuthority`/`LocalCommit` injection contracts.
Its default dependency closure contains no SQL/FS commit, dispatch, Session,
Resource, or Credential store and it has no backend-selection feature.

`awaken-coordinator::runtime_authority` is the sole durable implementation. It
selects SQLite, filesystem, or PostgreSQL once at Coordinator startup and returns
one capability containing commit, dispatch, wake, and stream-checkpoint access.
The Host can therefore distinguish only local injected authority from a
claim-fenced remote Worker projection; it cannot reopen or reselect a backend.

A product composition that already owns an opened
`PostgresCommitCoordinator` uses the Coordinator-owned
`postgres_local_commit` factory to erase it behind `LocalCommit`. The factory
retains the same private authoritative-query policy used by
`DurableRuntimeAuthority`: Run, committed-message, open-wait, and lifecycle
reads go to PostgreSQL rather than an in-process projection. The concrete query
policy is not public, so downstream products cannot copy or selectively replace
its SQL/read semantics. This is a composition inlet to the existing authority,
not a second backend selector or persistence owner.

Every claimed Worker drive, including an in-process SQLite/filesystem/PostgreSQL
Worker, installs one `RunRecoverySnapshot` before consulting committed execution
state. Subsequent claim-fenced commits advance that snapshot. A local Worker
therefore never treats the PostgreSQL coordinator's process-start projection as
cross-replica truth; terminal-race reconciliation refreshes the same snapshot
from the authoritative recovery source before deciding whether to settle or
re-raise. The database-independent Worker obtains the identical snapshot through
its authenticated dispatch transport.

`awaken-protocol-managed` is a driving anti-corruption layer. It translates the
Managed HTTP contract into the existing Session application and Runtime Host
interfaces; it is neither a Coordinator Runtime nor a persistence owner. Its
Memory and Skill routes depend on `awaken-resource-contract` and the canonical
Resources application ingestion path, with concrete stores supplied only by the
Resources application.

The `awaken`/`awaken-control`/`awaken-coordinator` targets share the single
`awaken-cli::run_service` lifecycle. `awaken-worker` remains a separate package so
its feature-resolved dependency closure can be proven authority-store-free. The
CLI assembles these deployables; it does not become another domain owner.

Under the accepted target, config publication is a Control flow, not a runtime
subsystem. The sole `CatalogModelPublicationResolver` in `awaken-control` reads
Catalog and credential metadata plus injected, secret-free ACP/Worker capability
facts and freezes complete model candidates. Control persists one immutable
`StoredPublication`, then invokes
`ExecutableAgentRegistrar::register`. Coordinator stores a rebuildable
`ExecutableAgentCatalog` projection for future Session resolution. The complete
decision and transition plan is
[ADR-0071](../adr/0071-distributed-service-boundaries-and-executable-agent-registration.md)
and the Environment-specific amendment in
[ADR-0072](../adr/0072-environment-definition-and-execution-boundary.md).

The reverse direction is equally explicit. Before Coordinator management
effects it records the stable audit identity through `ManagementAuditRepository`;
Session binding asks `SessionCredentialSource` only for secret-free credential
pins; lifecycle outbox delivery calls `LifecycleFactDelivery`. Split roles use
one authenticated HTTP adapter and AllInOne uses local adapters over the same
contracts. Authentication runs before a handler reaches any authority. Network or
5xx failures are retried only for these idempotent commands; an unavailable
webhook delivery leaves the Coordinator outbox fact pending for later drain.

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
| Domain/application service | resolver, registrar, continuation guard, Outcome controller, permission evaluator, plugin hook runner, terminal observer | Stateless or explicit state dependencies through contracts; an application service does not acquire another context's data authority |
| Repository | store traits under the runtime/server contract boundary | No product policy inside repositories |
| Domain event/fact | committed runtime facts and `EventRecord` values | Emitted after the commit boundary, then projected outward |
| Anti-corruption layer | protocol adapters, external product bridges, A2A/ACP mappers | Translate public names at the edge only |

This vocabulary is intentionally boring. Use it before creating a new role word or
crate name.

## 2.1 Contract Authority Map

| Boundary | Owns | Examples |
|---|---|---|
| Agent-domain contract | replayable agent truth and runtime commit vocabulary | `RunRecord`, durable run lifecycle value, `ThreadCommit`, `CommitCoordinator`, `RuntimeResumeStore`, state/fact/event records |
| Config publication contract | Control-owned records, model-candidate resolution, and immutable publication values | `ConfigStore`, `CatalogModelPublicationResolver`, `StoredPublication`, `ExecutableAgentSnapshot`, `ExecutableAgentRegistrar` |
| Environment contract | Control-owned static definitions, exact revisions, and policy references | `EnvItem`, `EnvironmentRevision`, `EnvRegistry`, `EnvironmentSandboxPolicyRef` |
| Coordinator execution catalog | rebuildable executable-Agent availability for new Sessions | `ExecutableAgentCatalog`, current/exact-revision/fingerprint reads, local/HTTP/PostgreSQL registrar adapters, authenticated private router, and durable command replay |
| Coordinator Environment application | rebuildable executable-Environment availability, frozen Session snapshot compilation, registration convergence, and dynamic work coordination | `EnvironmentExecutionApplication` over `ExecutableEnvironmentCatalog` and `WorkQueue`; no authoring repository, HTTP, or Managed DTO |
| Resources application contract | resource commands and per-kind materialization/lifecycle contracts | `ResourcesApplication`, `ResourceAuthorities`, `FileApplicationService`, `MemoryStoreApplicationService`, `ResourceCatalog`, `ResourceReclamationRepository` |
| Runtime-facing contract | immutable values and services used to prepare and execute one Run | `ExecutableAgentSnapshot`, `RunActivation`, `RuntimeRunContext`, `RunExecutor`, `RuntimeCapabilitySource`, `PluginManifest` |
| Runtime implementation | live execution behavior over agent-domain vocabulary | agent loop, resolver implementation, provider routing, plugin execution, retry/backoff modules |
| Run-ingress contract | durable delivery and dispatch vocabulary | submit/input records, dispatch records, claims, leases, wake hints, live-command delivery stores |
| Run-ingress implementation | buffering, host supervision, recovery, and live delivery | `DispatchQueue`, `DispatchWorker`, input buffer, recovery replay |
| Protocol projection | public protocol and product-facing replay shapes outside the runtime slice | `awaken-protocol-managed` solely owns the exact Anthropic-compatible surface; `/v1/awaken/*` lives in the separate `awaken-protocol-awaken` crate |
| Concrete stores | backend implementations of multiple repository contracts | SQL/in-memory adapters that implement both agent-truth and ingress stores |

If a type describes durable agent truth, it belongs to the agent-domain contract.
If it describes config records, snapshots, or publication identity, it belongs
to the config publication contract. If it describes Coordinator availability of
an exact executable snapshot, it belongs to the execution catalog projection. If
it describes runtime entry or configuration-surface inspection, it belongs to
the runtime-facing contract. If it describes delivery,
claim, lease, or wake mechanics, it belongs to run ingress. If it describes
public names or protocol replay rows, it is a projection and stays out of the
runtime contract.

## 2.2 Aggregate And Lifecycle Ownership

| Domain object | Authority and lifecycle | Downstream processing component |
|---|---|---|
| Agent definition | Control: draft → revised → compiled → published → archived; every publication remains immutable | executable-Agent registrar/catalog makes exact snapshots available to Coordinator Session admission |
| Environment | Control: create revision 1 → append update/policy-binding revisions → terminal archive; `env_local` is immutable built-in truth | executable-Environment registrar/catalog supplies current/exact facts; withdrawal denies new Sessions while exact history remains |
| Sandbox policy | Control: create v1 → append versions; Environment stores one exact reference | Control resolves the body into the executable Environment registration; Worker receives only the frozen Session projection |
| Deployment | Coordinator: create/update → active/paused → terminal archived | scheduler/manual trigger creates a stable DeploymentRun; it never executes an Agent itself |
| DeploymentRun | Coordinator: started → succeeded with `session_id` or failed with exact error | process-selected `ManagedDeploymentSessionLauncher` lowers stored Managed input and reaches the sole Session creation command, idempotent by `deployment_run_id` |
| DreamProcess | Coordinator: requested → preparing → auxiliary Session linked → cleaning → terminal; usage remains an ordinary Session fact | `DreamApplication` schedules/reconciles while `DreamExecutor` calls Resource and Session authorities directly |
| Session / Run | Coordinator: admit frozen Agent/Environment/Resource facts → enqueue → claimed/running/awaiting → committed terminal settlement | Worker executes under a lease epoch; Coordinator owns commit, replay, and public projection |
| File | Resources: logical create → active/readable → logical delete → purge intent → safe physical reclaim | `FileApplication` is the sole HTTP/artifact command path; Worker reads immutable bytes through `FileContentSource` |
| MemoryStore | Resources: create/retention → bind/freeze config version → active CAS use → tombstone → fenced reclaim | `MemoryStoreApplicationService` owns identity/lifecycle; `MemoryRepository` owns content; Agent `memory` plugin config owns recall/extraction behavior |
| Skill | Resources: canonical ingest → immutable version publication → Session exact pin → tombstone → reclaim after pins drain | `SkillStore` and `SkillBundleSource`; built-in Skills remain Runtime extensions |
| Repository definition | Resources: create/config versions → Session exact config/credential pin → ephemeral clone/use → tombstone/local cleanup | `ResourceCatalog`, credential materializer, and `RepositoryRealizer`; remote Git is never deleted |

The only cross-domain data used for a Run is immutable or fenced: exact Agent and
Environment revisions, a frozen secret-free resource manifest, exact credential
references, and a live dispatch claim. Mutable authoring repositories never cross
the boundary.

---

## 3. Runtime / Server Boundary

The server consumes the runtime through one gated contract:

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
providing Hand tools, the single native `agent_run` delegation tool, and the
fixed Managed `list_agents`/`send_to_agent` coordination pair. There is no
generic builtin Task command family.
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

Server execution has two intentionally different entry shapes:

```text
direct request  -> DirectAttemptDriver -> RunExecutor
durable request -> RunDispatch -> DispatchQueue -> DispatchWorker -> RunExecutor
```

`DirectAttemptDriver` is intentionally narrow: it executes one queue-less
attempt and exposes live control through `RunExecutor` and `LiveRunControl`. It
has no durable queue, replay, recovery, supersede, or scheduled-wake operation.

The durable path is data and workers rather than a stronger implementation of
the same interface. Input first becomes `RunDispatch`; `DispatchQueue` owns its
claim/lease state and `DispatchWorker` freezes pending messages, materializes
resolved config data, recovers wake hints, and activates runtime execution
through the narrow runtime roles.

The host stores this choice once as a private sum value. There are no parallel
direct/durable fields and no capability query whose answer can disagree with
the selected path.

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

The Credential domain owns refresh policy and durable records. The existing
`awaken-credential-materializer` infrastructure adapter is the sole executor of
an exact pinned OAuth refresh/reseal access; Runtime and Coordinator receive only
its narrow contract and do not recreate Vault refresh mechanics.

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

## 7. Operational Convergence And Deployment Topology

The four bounded contexts and their persistence authorities are complete in the
current deployment. Distributed deployment uses role-named Control,
Coordinator, and Worker executables; Resources is a canonical sibling application
currently hosted by the Coordinator process. This is process co-location, not
shared ownership: its stores, migrations, application services, and contracts
remain Resources-owned.

### 7.1 Static registration recovery

`ControlComponent` starts one `StaticRegistrationSupervisor` over the existing
Agent `PublicationBindingReconciler` and Environment `EnvironmentApplication`.
It carries no registration payload and owns no repository. Environment startup
replays its durable intent log once; after that, each pass selects pending intents
only. Both modes invoke the same drainer and registrar.

```text
Control component starts or receives a wake signal
  -> recover every durable Agent publication
  -> first Environment success: replay every durable intent; later: drain pending
  -> both succeed: ready, reset failures, record success time
  -> either fails: preserve serving readiness, record degraded pending domains,
     bounded exponential retry
```

The business listener and `/readyz` remain available during recovery so durable
last-known-good projections keep serving. The process exports registration
ready, pending-domain, consecutive-failure, and lag gauges; pending/failure are
the degraded signal until both domains complete a successful pass. Control and
AllInOne receive the health source from the same component; Coordinator has no
Control registration source and therefore no such recovery projection.

### 7.2 Active-active executable projection refresh

Each PostgreSQL executable command log has a versioned, monotonic
`command_sequence`. A Coordinator replica keeps only the last applied sequence
in memory; the command log remains durable authority. One contract-owned,
stateless async port composes Agent then Environment refresh and is shared with
the application consumers that can admit work or read executable inventory:

```text
read MAX(command_sequence)
  -> equal to local cursor: continue without loading commands
  -> greater: load commands WHERE sequence > cursor, ordered by sequence
       -> ordered batch reaches high-water: apply to a cloned projection, swap, advance
       -> missing/out-of-order tail: replay the complete log, swap, advance
  -> less than local cursor: replay the complete log, swap, advance
  -> any replay/apply failure: return 503 before admission
```

Database identity gaps are valid; failing to reach the observed high-water is
not. `awaken-durable-projection` owns this cursor and validation decision once.
Agent and Environment adapters retain separate command codecs and canonical
catalog state machines. Managed create/cold recovery, Session profile
creation/recovery/realization, Deployment commands/scheduling, and the existing
Models/Dream/warmup source ports call the shared handle before their first
catalog read, mutation, or external effect. Warm cached and already-frozen hot
paths do not refresh. No URL predicate or Axum middleware participates in this
correctness boundary. AllInOne omits the handle because local registration and
admission share those same catalog instances.

### 7.3 Conditional Resources process split

There is deliberately no standalone `resources` CLI role today. Co-deployment
avoids another availability and credential boundary while independent scaling
and credential isolation are not required. A future split requires all of the
following before adding the role:

1. authenticated, claim-fenced per-kind Worker transports around the existing
   `ResourcesApplication` contracts;
2. an explicit reference/grant protocol so Agent bindings and Coordinator
   extraction/activation facts reach the Resources-owned reverse-reference
   index without Resources opening Coordinator storage;
3. a Resources-only migration manifest and credentials, with no Control,
   Session, dispatch, or commit database access;
4. the same `ResourceAuthorities`, router, lifecycle repository, and reclaimer—no
   alternate File, Memory, Skill, or purge implementation.

Until those deployment requirements exist, configuration rejects `resources` as
a process role and Coordinator continues to host the canonical application.

### 7.4 Process modules

The CLI shell remains integration-only. `runtime_process_router` starts
Coordinator and optional AllInOne Control/Resources applications; standalone
Control uses `control_component`; both receive their business routers from the
same domain builders. Scenario-only ACP setup lives in `acp_scenarios`. These
module splits change neither authority nor call order.

Further work is demand-driven: database notification may replace the high-water
poll only if admission-query load becomes material, and a Resources process may
be introduced only after the topology conditions above are observed. Large
Managed application modules can continue to be separated by lifecycle concern
without moving aggregate ownership.
