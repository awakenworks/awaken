# Runtime Interface Boundaries

This document makes the runtime design explicit at the interface level. It
connects the bounded-context guidance in
[architecture-overview.md](architecture-overview.md) to the role traits and data
objects the design owns.

The goal is not to add a new abstraction layer. The goal is to name the existing
axes, the few places where they intentionally meet, and the checks that keep the
runtime small.

## Contract Boundary Overlay

This document names runtime-facing roles. Contract packaging still follows the
authority map in
[D12](key-design-decisions.md#d12---contract-names-follow-authority):

| Plane | Contract owner | Runtime boundary implication |
|---|---|---|
| Agent truth | agent-domain contract | `RunRecord`, durable run lifecycle value, `ThreadCommit`, durable events/facts, `CommitCoordinator`, and `RuntimeResumeStore` are runtime truth vocabulary |
| Catalog install | runtime contract/spec plus config publication value | `RuntimeCatalogInstall` and `RuntimeCatalogInstaller` form the runtime-facing install handoff; config-side `RegistryPublication` supplies publication identity and fingerprint data |
| Snapshot execution and inspection | runtime contract/spec | `ExecutableAgentSnapshot`, `RunWithSnapshotCommand`, `AgentSnapshotResolver`, `AgentSnapshotCatalog`, `RuntimeCapabilitySource`, and `PluginManifest` (with `validate_section`) are internal runtime-facing contracts; they expose executable snapshot and capability data, not config CRUD |
| Live execution | runtime implementation | `RunExecutor`, `LiveRunControl`, `RunResolver`, plugins, providers, and retry logic are implementation roles over agent vocabulary |
| Durable delivery | run-ingress contract/implementation | `RunIngress`, input buffering, dispatch records, claims, leases, wake hints, and recovery do not become runtime loop vocabulary |
| Protocol projection | outside this runtime slice | protocol replay rows and public names are derived from committed facts when a protocol slice is added |
| Store implementation | concrete stores | one backend may implement both agent-truth and run-ingress ports without merging the contracts |

The runtime implementation must not depend on run-ingress claim/lease/recovery
types. Durable ingress may depend on runtime execution roles because it starts and
supervises execution attempts.

For the end-to-end sequence from persisted config to run activation, resolution,
execution, commit, and projection, use
[config-to-run-execution-flow.md](config-to-run-execution-flow.md). This document
owns the role catalog; the flow document owns ordering and handoff rules.

## Boundary Matrix

| Boundary | Owner | Crosses | Must not cross | Fail-closed rule |
|---|---|---|---|---|
| Product adapter -> run ingress | Product adapter and Dispatch / Server | neutral submit/control command, public ids already translated | public DTOs, product status names, auth grants | unsupported ingress operations return typed unsupported errors |
| Config publication -> runtime catalog | Config Application plus Runtime Core adapter | `RuntimeCatalogInstall` containing publication identity, catalog fingerprint, and version | config CRUD workflow, admin DTOs, product route state, private admin tool registry, publication compiler state | invalid or conflicting publication does not replace the active runtime catalog |
| Configuration surface -> snapshot execution contract | Config surface / Server plus Runtime Core adapter | inline `ExecutableAgentSnapshot`, `ExecutableAgentSnapshotId`, agent snapshot query, runtime capability catalog, plugin config validation request | config CRUD workflow, admin publication workflow, public DTOs, live registry handles | unknown, stale, mismatched, or unauthorized executable snapshot ids fail before activation |
| Server route -> `RunIngress` | Dispatch / Server | `SubmitCommand`, cancellation, decision delivery, dispatch query | durable ingress internals or runtime internals | `RunIngressCapabilities` decides durable-only behavior |
| Direct ingress -> runtime roles | Runtime Core adapter | `RunExecutor` and `LiveRunControl` | durable queue, recovery, replay, scheduled wake | direct ingress rejects durable-only operations |
| Durable ingress -> durable delivery internals | Dispatch / Server | `DurableRunIngress`, input buffer store, runtime store, lifecycle config | product status and protocol replay as runtime truth | durable delivery still commits through the runtime commit boundary |
| Durable buffer -> execution | Dispatch / Server plus Runtime Core roles | `RunExecutionRequest` data and `RunExecutionContext` handles | live registry, resolver, commit coordinator, inbox, cancellation handles inside the request data | data-only request; live wiring travels separately in context |
| Resolution | Runtime Core plus Config edge | `ResolvedSpec`, `CatalogFingerprint`, `ResolvedRun`, `RunResolver` | live registry objects, pins, tenant scopes, factories across the config edge | fingerprint/catalog mismatch fails before execution |
| Execution | Runtime Core | `AgentRuntime`, `RunActivation`, optional pre-resolved plan, `StreamSink` | HTTP route state and public protocol names | backend requirements are checked before execution |
| Persistence | Runtime Core and Store contracts | runtime persistence policy, checkpoint access, `RuntimeResumeStore`, `CommitCoordinator` | split reader/writer pairs or side writes | read-only writes fail; read/write derives reader from coordinator |
| Plugin extension | Runtime Core extension seam | `Plugin::resolve` contributions merged into `ResolvedExecutionEnv` | direct store mutation, unregistered hooks, product labels | duplicate owners fail; hook output is validated and committed or rejected |
| Tool decision | Runtime Core plus permission extension | descriptor visibility, tool gate/policy decision, tool execution result | authorization hidden in visibility, selection, or backend location | visibility grants perception only; invocation still gates |
| Wait/resume | Product adapter plus Runtime Core live control | neutral result for one pending parked run (e.g. a client-executed tool call) | public tool-use DTOs, config/admin writes, global catalog mutation | unknown, duplicate, expired, thread-mismatched, or descriptor-mismatched results fail closed |

## Role Split

Do not treat "runtime controller" as one large interface. The current runtime
seam is smaller when split by authority:

| Role | Authority | Does not own |
|---|---|---|
| `RunExecutor` | execute a prepared activation or pre-resolved replayable plan | live steering, resolution policy, commit ownership |
| `RuntimeRunContext` | carry per-attempt live wiring needed by execution | public DTOs, immutable activation data, config publication workflow |
| `LiveRunControl` | cancel, deliver a decision, wake a pending boundary | durable queueing, replay, registry materialization |
| `RunResolver` | resolve live or pinned execution plans | running a loop, live control, commit writes |
| `RunWithSnapshotExecutor` | accept an inline executable snapshot or executable snapshot id and turn it into a runtime run request | config CRUD, snapshot storage, public protocol mapping |
| `AgentSnapshotResolver` | resolve one `ExecutableAgentSnapshotId` into an executable snapshot | config authoring, runtime loop execution, snapshot list policy |
| `AgentSnapshotCatalog` | list current executable snapshots for configuration surfaces | runtime execution, config mutation, admin workflow |
| `RuntimeCapabilitySource` | report the runtime's installed plugin/tool/backend capability surface | config publication, authorization, execution |
| `PluginManifest` | declare plugin id, config sections, and `CapabilityBound`, and validate config through the single `validate_section` | runtime handles, plugin behavior, a parallel validator |
| `RuntimeCatalogInstaller` | atomically install a complete runtime catalog publication | config CRUD, registry compilation, live control, run execution |
| `CommitCoordinatorSource` | expose the runtime commit coordinator for durable ingress construction | execution semantics |
| `RunIngress` | server-facing delivery semantics and capability reporting | runtime internals or durable ingress internals |

`DirectRunIngress` is the queue-less projection of `RunExecutor` plus
`LiveRunControl`. `DurableRunIngress` is the durable-buffer-backed
implementation that adds buffering, recovery, replay, scheduled wake, and
lifecycle wiring without turning delivery policy into public route vocabulary.

## Role Catalog

This catalog is the maintenance surface for stable runtime boundary roles. Add a
row only when the name carries authority across a boundary or is used by multiple
design documents. API signatures and parameter details stay in Rustdoc.

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `RunIngress` | server-facing boundary port | run delivery semantics and capability reporting | direct or durable ingress implementation | runtime internals, durable ingress internals, product DTOs | unsupported delivery operation hidden until runtime | G5; `RunIngressCapabilities` tests |
| `DirectRunIngress` | queue-less ingress implementation | direct submit/control over live runtime roles | `RunExecutor`, `LiveRunControl` | durable queue, recovery, replay, scheduled wake | caller assumes durability that does not exist | G5; direct rejects durable-only operations |
| `DurableRunIngress` | durable ingress implementation | durable submit/control semantics and buffering/recovery entrypoint | input buffer, dispatch coordinator, recovery/replay, runtime roles | product status, runtime loop internals, side commits | lost input, duplicate dispatch, stale recovery | G1, G5, G6, G13; durable ingress tests |
| `RunExecutor` | runtime execution role | execute a prepared activation or replayable plan | `AgentRuntime`, activation data, event sink | live steering, resolution policy, commit ownership | execution role gains unrelated control authority | G2, G14; public API surface tests |
| `RuntimeRunContext` | live execution context | per-attempt wiring such as cancellation token, input receiver, stream sink, commit coordinator source, thread context cache, pinned resolver scope, and persistence mode | `RunExecutor`, ingress/runtime execution construction | public protocol payloads, immutable activation fields, config records, publication services | data-only activation and live handles become indistinguishable | G2, G3, G5, G13; activation/context split tests |
| `LiveRunControl` | live steering role | cancel, deliver decision, wake pending boundary | active run registry or runtime handle | durable queueing, replay, registry materialization | live control mistaken for durable delivery | G5; live-control route tests |
| `RunResolver` | resolution role | resolve live or pinned execution plan | `ResolvedSpec`, catalog fingerprint, runtime catalog | running loops, live control, commit writes | execution starts from mismatched catalog | G3, G4; fingerprint mismatch tests |
| `ExecutableAgentSnapshot` | value object | complete resolved configuration for one run/thread execution scope | root agent id, resolved spec, catalog fingerprint, snapshot id | live registry handles, config authoring workflow, public DTOs | same agent id is mistaken for same executable configuration | G3, G4, G28; snapshot serde and fingerprint tests |
| `RunWithSnapshotCommand` | value object | run request that names either inline executable snapshot data or an executable snapshot id | run input, options, trace, agent snapshot input | config CRUD payloads, public protocol DTOs, route state | caller bypasses snapshot validation or smuggles config mutation into runtime | G2, G10, G28; command surface tests |
| `RunWithSnapshotExecutor` | runtime-facing boundary port | execute a run from `RunWithSnapshotCommand` after snapshot validation | `AgentSnapshotResolver`, `RunExecutor`, `RunIngress` where delivery is needed | executable snapshot storage, config publication, admin workflow | runtime accepts agent id as complete config identity | G2, G18, G28; inline/by-id execution tests |
| `AgentSnapshotResolver` | lookup port | resolve one `ExecutableAgentSnapshotId` into `ExecutableAgentSnapshot` | executable snapshot store or config service adapter | config CRUD, list policy, runtime loop execution | stale or unauthorized snapshot data enters execution | G3, G4, G28; by-id resolution tests |
| `AgentSnapshotCatalog` | query port | list current executable snapshots for configuration surfaces | snapshot index and capability filters | run execution, config mutation, admin publication | UI reimplements runtime snapshot visibility or assumes agent id uniqueness | G14, G28; catalog query tests |
| `RuntimeCapabilitySource` | query port | expose installed runtime capabilities such as plugins, tool descriptors, model capability profiles, and schema keys | runtime registry, resolved plugin manifests, model capability profiles | authorization grants, config writes, execution | configuration surface shows capabilities unrelated to the active runtime | G8, G21, G28; capability snapshot tests |
| `PluginManifest` | identity/config contract value | declared plugin id, dependencies, config sections, and `CapabilityBound`; the single `validate_section` home shared by write-time and resolve-time validation | typed config schema, declared sections | runtime handles, plugin behavior, a parallel validator | config is validated differently by UI and runtime, or a plugin reads an undeclared section | G8, G14, G28, G30; schema-derived-from-type round-trip tests |
| `RuntimeCatalogInstall` | command value | complete catalog install request with publication identity, fingerprint, source revisions, and runtime-visible catalog data | publication identity, compiled install payload | config CRUD workflow, compiler caches, admin route state, config publication semantics | runtime installs an incomplete or unauditable catalog | G3, G23, G29; install serde and fingerprint tests |
| `RuntimeCatalogInstaller` | runtime-facing boundary port | validate and atomically install one complete runtime catalog publication | `RuntimeCatalogInstall`, active runtime catalog handle | config CRUD, registry compilation, live control, run execution | incomplete or conflicting publication replaces active runtime catalog | G18, G23, G29; install transaction and rollback tests |
| `CommitCoordinatorSource` | commit-wiring role | expose the runtime commit coordinator needed by durable ingress construction | `CommitCoordinator`, optional staged commit coordinator | execution semantics, dispatch policy, input buffering | durable ingress reads and runtime commits diverge | G1, G13; same-source commit tests |
| `ResolvedSpec` | data value crossing config edge | serializable resolved runtime input and catalog fingerprint | config-domain resolver output | live registry objects, factories, tenant scopes | replay cannot prove what the model saw | G3, G4; serde/fingerprint tests |
| `StreamSink` | live stream output port | best-effort delivery of `StreamEvent` across the boundary | runtime execution, current caller/server connection | commit ownership, product protocol mapping, durable replay truth | live delivery is mistaken for durable truth or product-shaped events | G1, G10, G13; sink/projection tests |
| `EventSubscriber` | durable event delivery port | subscribe to committed `EventRecord` / `DurableEvent` values for projection | event store, committed records, downstream projection | live stream delivery, protocol naming, commit authority | projection observes uncommitted runtime output | G1, G10, G13; durable event subscription tests |
| `Plugin` | extension factory | `manifest()` plus `resolve(cx) -> Contributions`; config-dependent artifacts compiled once at resolve | resolved `AgentSpec` via `ResolveContext`, declared config sections | direct store mutation, mutable registration side effects, run handles | plugin bypasses policy, or config is recompiled per call | G8, G9, G14, G30; resolve-once and no-bypass tests |
| `Contributions` | contribution value | one plugin's resolved tools, hooks, gates, guards, transforms, and state keys | `ResolveContext` output | cross-plugin merge, durability, authorization | a plugin's contributions cannot be reasoned about as one value | G8, G30; contribution serde tests |
| `CapabilityBound` | declared bound value | the upper bound of what a plugin may contribute (set/namespace for identity-bearing kinds; flag for singleton powers) | `PluginManifest` | authorization grant, the authoritative inventory | a contribution exceeds what was declared and is caught only at runtime | G9, G21, G30; `actual ⊆ declared` fail-closed tests |
| `ResolvedExecutionEnv` | aggregate root | merge of every selected plugin's `Contributions` for one run: uniqueness scoped to the active set, declared `requires` order, and `enforce_bound` | each plugin's `Contributions`, runtime catalog | host process environment, product session state, per-run live wiring | duplicate ids, incidental ordering, or out-of-bound contributions pass silently | G8, G14, G30; merge-conflict, ordering, and bound tests |

`ResolvedExecutionEnv` is the code and design name for the per-run aggregate that
merges plugin contributions; it is immutable and reuses the activation/context
split (see key-design-decisions D21), so per-run handles never bind into it.

`RunIngress` / `DurableRunIngress` are the server-facing run delivery boundary.
`CommitCoordinatorSource` is only the narrower commit wiring role used by durable
ingress construction. It exposes the runtime's commit coordinator and optional
staged commit coordinator; it does not own input buffering.

## Publication Roles Outside Runtime

Publication coordination, compilation, and durable publication identity are
deliberately absent from the runtime role catalog. They are config-side roles and
values. The runtime-facing handoff is `RuntimeCatalogInstall`, consumed through
`RuntimeCatalogInstaller`.

| External role or value | Owner | Runtime relationship |
|---|---|---|
| `ConfigPublicationCoordinator` | Config Application / Server | orchestrates loading, discovery, compilation, versioned publish, install, and projection as one application transaction |
| `RegistryCompiler` | Config Domain | validates a config snapshot and produces a complete `RegistryPublication` / install candidate |
| `RegistryPublication` | Config Domain | immutable publication identity, version, source revisions, and fingerprint referenced by the install request |
| `RuntimeCatalogInstaller` | Runtime Core adapter | the only runtime-facing install port the coordinator may call |

The first two roles must not import runtime loop internals or active-run control.
The installer must not load config records or compile publications. This keeps a
large `ConfigRuntimeManager` from becoming a second runtime controller.

## Activation Versus Runtime Context

`RunActivation` is immutable run input. `RuntimeRunContext` is per-attempt live
wiring. Keeping them separate prevents a durable request, a retry record, or a
snapshot command from smuggling in process-local handles.

| Value | Carries | Must not carry |
|---|---|---|
| `RunActivation` | snapshot input, intent, input messages or persisted input reference, options, inference overrides, trace context, and neutral run identity | cancellation handles, channels, commit coordinator overrides, stream sinks, live registries, config stores, route state, public DTOs |
| `RuntimeRunContext` | cancellation token, runtime input receiver, decision/wake handles, stream sink, commit coordinator source, thread context cache, pinned resolver scope, persistence access mode, and optional pre-resolved plan | config authoring data, publication compiler state, public protocol payloads, model-visible descriptors not already selected by resolution |
| `ExecutableAgentSnapshot` | complete immutable executable configuration for one run/thread scope | process-local tool/backend handles, live registry objects, config CRUD provenance, active-run steering handles |

The split is stricter than a convenience struct that mixes activation, control,
capture, persistence, and inherited resolver data. A host may construct both
values at the same call site, but serialization, durable dispatch, replay, and
tests should treat activation as data and context as wiring.

## Minimal Runtime Configuration Surface

The runtime configuration surface is intentionally small. It has install,
execution, lookup, listing, capability, and validation operations; it has no
config authoring operation.

| Operation | Port | Input | Output | Authority |
|---|---|---|---|---|
| install catalog | `RuntimeCatalogInstaller` | `RuntimeCatalogInstall` | installed catalog version/fingerprint | atomically expose a complete catalog to future resolution |
| run with snapshot | `RunWithSnapshotExecutor` | `RunWithSnapshotCommand` | run handle/result according to ingress mode | start execution from inline or by-id executable snapshot data |
| resolve snapshot | `AgentSnapshotResolver` | `ExecutableAgentSnapshotId` or snapshot ref | `ExecutableAgentSnapshot` | read immutable executable configuration data |
| list snapshots | `AgentSnapshotCatalog` | `AgentSnapshotQuery` | `AgentSnapshotPage` | expose read-only executable snapshot summaries |
| inspect capabilities | `RuntimeCapabilitySource` | none or point-in-time query | `RuntimeCapabilityCatalog` | report installed runtime capability facts |
| validate plugin config | `validate_section` | plugin config key/value | validation result | one validator shared write-time and resolve-time |
| execute activation | `RunExecutor` | `RunActivation` or pre-resolved plan | runtime result and staged commit | run the resolved loop |

`RuntimeCapabilityCatalog` should be a read-only fact snapshot. Its minimum
shape is catalog fingerprint, runtime version or build identity, tool
capabilities, plugin capabilities with schema keys, and backend capability
profiles. It grants no authorization and does not imply a config write path.

## Snapshot Execution And Inspection Contract

An agent id is not the complete identity of an executable run configuration.
The executable identity is the snapshot selected for the run or thread. The same
agent id may point to different instructions, tools, plugins, model bindings, and
capability requirements in different runs.

The internal runtime-facing contract therefore accepts snapshot input in two
forms:

```text
enum AgentSnapshotInput {
    Inline(ExecutableAgentSnapshot),
    ById(ExecutableAgentSnapshotId),
}

struct ExecutableAgentSnapshot {
    id: ExecutableAgentSnapshotId,
    root_agent_id: AgentId,
    resolved_spec: ResolvedSpec,
    fingerprint: CatalogFingerprint,
}
```

`ExecutableAgentSnapshot` is immutable data. It may carry the complete resolved
configuration needed to execute one run/thread scope, but it does not carry live
registry handles, config store handles, admin workflow state, or public protocol
DTOs. Runtime validation still checks the catalog fingerprint and selected
runtime capabilities before execution.

The configuration inspection surface is a composition of small internal ports:

```text
trait RunWithSnapshotExecutor {
    fn submit_run_with_snapshot(command: RunWithSnapshotCommand) -> Result<RunHandle>;
}

trait AgentSnapshotResolver {
    fn get_snapshot(id: ExecutableAgentSnapshotId) -> Result<Option<ExecutableAgentSnapshot>>;
}

trait AgentSnapshotCatalog {
    fn list_snapshots(query: AgentSnapshotQuery) -> Result<AgentSnapshotPage>;
}

trait RuntimeCapabilitySource {
    fn runtime_capabilities() -> RuntimeCapabilityCatalog;
}

// Config validation has one home, shared by write-time and resolve-time.
fn validate_section(manifest: &PluginManifest, key, value) -> Result<ConfigValidation>;
```

These are internal APIs. They let a server, local tool, or configuration surface
run with inline snapshot data, run by snapshot id, list executable snapshots, and
inspect the runtime's current plugin/tool/backend capability surface. They are
not config CRUD, not publication workflow, and not a product management API.

The interaction model is:

```text
inline snapshot run
  -> validate snapshot data and fingerprint
  -> materialize runtime execution objects
  -> execute through RunExecutor / RunIngress

snapshot id run
  -> AgentSnapshotResolver.get_snapshot(id)
  -> same validation and execution path as inline

configuration surface
  -> AgentSnapshotCatalog.list_snapshots(query)
  -> RuntimeCapabilitySource.runtime_capabilities()
  -> validate_section(manifest, key, value)
```

This contract is a runtime execution and inspection extension point, not a
config authoring extension point. The config domain may implement the resolver
and catalog adapters, but the runtime contract does not depend on `ConfigStore`,
drafts, publication buttons, admin audit workflow, or product tenancy.

## Primary Runtime Axes

The runtime-facing model has three primary axes. They are primary because they
answer different authority questions for every run:

| Primary axis | Question answered | Owning boundary | Main ports |
|---|---|---|---|
| Configuration publication | What behavior is available to run? | Config Domain publishes; Runtime Core validates and consumes | external: `ConfigPublicationCoordinator`, `RegistryCompiler`, `RegistryPublication`; runtime: `RuntimeCatalogInstaller`, `RunResolver` |
| Live control | How may an active run be steered now? | Caller/ingress requests; active run observes at safe boundaries | `RunIngress`, `LiveRunControl`, `RuntimeInputHandle` |
| Execution | How is the resolved plan performed? | Runtime Core orchestrates; model/tool ports invoke work in-process | `RunExecutor`, `LlmExecutor`, `ToolExecutor` |

Do not collapse these axes into one runtime controller. Configuration publication
is data authority, not execution. Live control is active-run steering, not config
or admin authority. Execution owns loop orchestration, not public protocol
projection or config authoring.

The other axes are supporting axes that make the primary axes replayable,
observable, and extensible:

| Supporting axis | Relationship to the primary axes |
|---|---|
| Activation | turns adapter/server input into neutral `RunActivation` for execution |
| Snapshot execution | selects an inline or by-id `ExecutableAgentSnapshot` for a run/thread before activation finishes |
| Resolution | consumes configuration publication and materializes an execution plan |
| State | records live runtime mutations that execution may stage into commit |
| Event | carries live stream output and committed projection source data |
| Wait/resume | pauses execution on a structured waiting reason and resumes through live control after adapter projection |
| Commit | turns staged runtime truth into durable facts; all projections derive after it |
| Extension | contributes hooks, tools, transforms, guards, and state keys during resolution |

The relationship is:

```text
configuration publication -> resolution -> execution -> state/event -> commit
snapshot execution -------> activation --^
live control --------------------------> active execution boundary
wait/resume ----------------------------^
extension ---------------------> resolution and execution environment
```

## Axis Descriptions

Each axis names one authority question. The axes are not stages of a single
pipeline; a run crosses them when a value changes owner or durability. In
particular, a hook/tool/model call starts in the execution axis, may produce
state or event candidates, and becomes durable only after the commit axis accepts
the `ThreadCommit`.

| Axis | Owns | Enters through | Produces | Must not own |
|---|---|---|---|---|
| Configuration publication | the behavior catalog available to future resolution | `RuntimeCatalogInstall` consumed by `RuntimeCatalogInstaller` | runtime-visible catalog, fingerprint, install result | config authoring, admin workflow, live control, execution |
| Snapshot execution | the executable configuration identity for one run/thread scope | inline `ExecutableAgentSnapshot` or `ExecutableAgentSnapshotId` | validated executable snapshot input for activation and resolution | config CRUD, publication workflow, agent-id-only identity |
| Activation | neutral runtime intent prepared for execution | submit/resume input translated by ingress or adapter code | `RunActivation` and optional runtime context | public DTOs, live registry handles, durable dispatch internals |
| Resolution | materialized execution plan and environment | catalog data, snapshot data, backend/profile requirements | `ResolvedRun`, `ResolvedExecutionEnv`, selected backend/tool/hook set | running the loop, live steering, commit writes |
| Live control | steering for an already active run | cancel, decision, message, or wake command | observed runtime input at a safe boundary | durable queueing, config writes, replay truth |
| Execution | ordered runtime work over a resolved plan | `RunExecutor`, backend/model/tool invocations, phase hooks | live stream output, `ToolOutput`, `StateCommand`, event/fact drafts, commit plan | durable writes, authorization ownership, protocol projection |
| State | live typed state mutation requested by execution | registered `StateKey` plus `StateCommand` | `MutationBatch`, live `StateStore`, `PersistedState` export | direct durable write, product/shared resource state |
| Event | live and durable neutral runtime event shapes | `StreamEvent`, `EventDraft`, durable event staging | `EventRecord` after commit and event subscription source | public protocol names, commit authority |
| Wait/resume | pending parked-run boundary (e.g. client-executed tool) | resolved pending request and later neutral resume command | pending `RunWaitingState`, validated resume input, result fact after commit | public result ids as truth, config/admin mutation |
| Commit | durable runtime truth for thread/run/message/state/event records | `ThreadCommit` passed to `CommitCoordinator` | committed facts, events, messages, state, and resume-visible records | executing hooks/tools, protocol DTO projection, side writes |
| Extension | installable runtime behavior contribution | `Plugin::resolve` during resolution | hooks, tools, gates, transforms, keys, handlers in `ResolvedExecutionEnv` | direct store mutation, permission bypass, product labels |

## Runtime Axis Flows

Every axis that carries independent authority needs a short core flow. The flow is
not an API transcript; it is the minimum narrative needed to show where authority
enters, where it changes hands, and which port is allowed to make the next value.

| Axis | Core flow | Authority handoff | Must stay explicit |
|---|---|---|---|
| Configuration publication | config records -> config snapshot -> `RegistryCompiler` -> `RegistryPublication` -> `RuntimeCatalogInstall` -> `RuntimeCatalogInstaller` -> runtime-visible catalog | config domain owns config authoring and compilation; runtime owns only install validation and catalog consumption | admin DTOs, private admin tools, live registries, compiler caches, and route state do not cross as runtime input |
| Snapshot execution | inline snapshot or `ExecutableAgentSnapshotId` -> `ExecutableAgentSnapshot` -> fingerprint/capability validation -> activation/resolution | caller or configuration surface selects the executable snapshot; resolver supplies data; runtime validates and executes | agent id is not treated as complete configuration identity; config CRUD and admin workflow do not cross the snapshot contract |
| Activation | neutral submit/resume command -> `RunActivation` with intent, input, options, trace, control, persistence hints, and inherited resolver data -> optional `RuntimeRunContext` with commit coordinator, pinned registry set, thread context, and resolved plan -> `RunExecutor` | ingress or caller prepares data; runtime executes the owned activation | adapter DTOs and live registry handles do not enter `RunActivation`; per-run wiring stays in `RuntimeRunContext` |
| Live control | cancel/decision/message/wake command -> `LiveRunControl` -> active run input channel or cancellation token -> loop consumes at a safe boundary | caller/ingress requests steering; active run decides when it can observe it | live delivery is best-effort; durable fallback belongs to `RunIngress` |
| Resolution | config/catalog data -> `ResolvedSpec` and fingerprint -> `RunResolver` validates live or pinned scope -> `ResolvedRun` and `ResolvedExecutionEnv` | config domain owns publication; runtime owns validation and materialization | runtime does not search for an arbitrary provider after activation starts |
| Execution | resolved plan -> model capability check -> in-process loop -> LLM/tool calls -> stream output, state commands, event drafts, commit plan, final result | runtime owns loop orchestration; tool/model ports own invocation mechanics | execution ports do not grant authorization and do not own protocol projection |
| State | registered keys -> seed/import persisted state -> hooks/tools return `StateCommand` -> `MutationBatch` -> live `StateStore` -> export `PersistedState` into commit | runtime owns live revisioned state; commit owns durability | product/shared state stays behind approved resource or product ports |
| Event | execution emits `StreamEvent` to `StreamSink` -> optional `DurableEventSink` normalizes live events into `EventDraft` / `DurableEventDraft` values -> commit succeeds -> `EventRecord` / `DurableEvent` reaches `EventReader` or `EventSubscriber` for projection | stream sink owns live delivery; commit owns durable event visibility | live stream output cannot become replay truth |
| Wait/resume | parked run records pending id, fingerprint, deadline, and authorization state (e.g. a client-executed tool call) -> adapter projects a public wait after commit -> adapter maps public result to neutral resume -> `LiveRunControl` wakes the pending boundary | runtime owns pending-request validation; adapter owns public event/result names | public result ids do not become runtime truth; inbound results cannot mutate config/admin/catalog state |
| Commit | resume read via `RuntimeResumeStore` -> runtime resolves disabled/read-only/read-write persistence access -> stage `ThreadCommit` with messages, run projection, state export, and event drafts -> `CommitCoordinator` commits atomically -> facts/records become visible; durable ingress verifies same-source wiring at construction | runtime proposes a commit; coordinator owns the durable write mechanism; run ingress owns the same-source guard | read and write must come from the same commit source; no side writes |
| Extension | selected plugin ids -> `Plugin::resolve` -> `Contributions` -> `ResolvedExecutionEnv` merge (uniqueness, declared order, `enforce_bound`) -> hooks, tools, guards, handlers, transforms, and keys run through declared surfaces -> outputs validate and stage | plugins declare behavior; runtime validates and stages the effects | plugins cannot mutate stores, bypass gates, exceed their bound, or introduce product labels |

## Axis Lifecycle Catalog

Each axis's lifecycle is owned by a concrete runtime role (its sum type, status
enum, or command/result value) and one owning document or code type — not by a
state machine restated here. This is a pointer table: read the owning role and
its owner for the authoritative states, transitions, visibility points, and
failure rules. Runtime-owned durable axes use a sum type or status enum; ephemeral
axes use command/result values but still carry correlation, duplicate, expiry, and
failure rules when they cross process or retry boundaries (ADR-0001 D1: link, do
not copy).

| Axis | Owning role(s) | Owning design (this corpus) |
|---|---|---|
| Configuration publication | `RuntimeCatalogInstall` / `RuntimeCatalogInstaller` | [config-publication-lifecycle.md](config-publication-lifecycle.md) |
| Snapshot / activation | `RunActivation` / `ExecutableAgentSnapshot` | [config-to-run-execution-flow.md](config-to-run-execution-flow.md) |
| Resolution | `ResolvedRun`, `Resolver` / `RunResolver` | [ADR-0002](../adr/0002-resolver-role-demarcation.md) |
| Live control | `LiveRunControl` | [run-ingress-message-delivery.md](run-ingress-message-delivery.md) |
| Execution | `RunExecutor`, run phases / terminal reason | [runtime-behavior.md](runtime-behavior.md) |
| State | `StateStore` / `StateCommand` / `MutationBatch` | [runtime-behavior.md](runtime-behavior.md) |
| Event | `StreamEvent` / `EventRecord` / `StreamSink` | [commit-fact-projection-taxonomy.md](commit-fact-projection-taxonomy.md) |
| Wait/resume (scheduled, client-tool, decision) | `ScheduledAction`, `RunWaitingState` + `ResumeValidator`, durable dispatch | [ADR-0003](../adr/0003-deferred-work-mechanism-selection.md) |
| Commit | `ThreadCommit` / `CommitCoordinator` | [commit-fact-projection-taxonomy.md](commit-fact-projection-taxonomy.md) |
| Extension | `Plugin` / `Contributions` / `ResolvedExecutionEnv` | [runtime-behavior.md](runtime-behavior.md) |

The axis flow and ownership tables above name where each value enters and changes
hands; the per-axis enforcing tests are in the guardrail index
([INVARIANTS.md](../INVARIANTS.md)).

Cross-axis lifecycle values must name their owning axis. For example, a committed
`ScheduledAction` waiting state is durable run state owned by execution and
commit, while the dispatch/server delivery lease that runs it is operational state
outside runtime truth. If a value seems to belong to several rows, split the
durable data from the live handle or operational projection before adding an API.

## Runtime Port Matrix

The port matrix is the stable maintenance surface for runtime I/O. A port may
carry, create, or hold data from an axis only when the row below grants that
authority.

| Port | Direction | Primary axis | Crosses into | Allowed values/handles | Durability | Must not own |
|---|---|---|---|---|---|---|
| `RuntimeCatalogInstaller` | input | Resolution | active runtime catalog | `RuntimeCatalogInstall`, catalog fingerprint, source revisions, publication identity | config revision and publication version before execution | config CRUD, registry compilation, execution loop, public DTOs |
| `RunWithSnapshotExecutor` | input | Activation + Resolution | runtime execution entry | `RunWithSnapshotCommand`, inline `ExecutableAgentSnapshot`, `ExecutableAgentSnapshotId` | snapshot identity may be durable; execution truth still commits normally | config CRUD, admin workflow, public protocol mapping |
| `AgentSnapshotResolver` | dependency | Resolution | snapshot lookup | `ExecutableAgentSnapshotId`, `ExecutableAgentSnapshot` | source-specific; runtime treats output as immutable data | snapshot list policy, config mutation, runtime loop execution |
| `AgentSnapshotCatalog` | output/dependency | Resolution | configuration surface | `AgentSnapshotQuery`, `AgentSnapshotPage`, executable snapshot summaries | read-only query surface | run execution, config mutation, admin publication |
| `RuntimeCapabilitySource` | output/dependency | Resolution + Extension | configuration surface | plugin/tool/backend capability catalog and schema keys | point-in-time capability snapshot | authorization, config writes, execution |
| `PluginManifest` | contract value | Extension | configuration surface and config validation | plugin config key and value, validation result | no durability | runtime handles, a parallel validator |
| `RunExecutor` | input | Activation | Resolution, Execution, State, Event, Commit, Extension | `RunActivation`, `RuntimeRunContext`, optional resolved plan | final truth only through `CommitCoordinator` | live steering API, resolver policy, store implementation |
| `RuntimeRunContext` | input | Activation + Live control | Execution, Commit | process-local handles for one attempt | context itself is not durable | public DTOs, config records, immutable snapshot data |
| `LiveRunControl` | input | Live control | active run registry, runtime input channel | cancel, decision, direct message, pending-boundary wake | ephemeral; durable fallback is ingress-owned | durable queue, replay, registry materialization |
| `RuntimeInputHandle` / `RuntimeInputReceiver` | input | Live control | Execution | `RuntimeInput` payloads for the active loop | in-process and best-effort | durable input storage, message-log append |
| `RunResolver` | dependency | Resolution | Provider, Execution, Extension | `ResolvedSpec`, catalog fingerprint, backend profile, selected live refs | pinned/fingerprinted for persistent runs | running loops, live control, commit writes |
| `CommitCoordinatorSource` | dependency | Commit | Durable ingress construction | one `CommitCoordinator` plus optional staged commit coordinator tied to the runtime store scope | same-source commit wiring | execution semantics, dispatch policy, input buffering |
| `CommitCoordinator` | output/dependency | Commit | State, Event, Message | `ThreadCommit`, `PersistedState`, `EventDraft`, facts | atomic durable write | protocol DTOs, dispatch lifecycle, provider selection |
| `RuntimeResumeStore` | dependency | Commit | Activation, State, Event | thread/run/message resume snapshot | consistent read only | writes, full CRUD/query surface, public projection |
| `StreamSink` | output | Event | Execution | `StreamEvent` | live, lossy, connection-scoped | durable truth, replay, protocol mapping |
| `DurableEventSink` | output adapter | Event + Commit | Stream, durable event staging | live `StreamEvent`, normalized `EventDraft` / `DurableEventDraft` | stages only; commit controls visibility | durable commit authority, protocol mapping |
| `EventReader` / `EventSubscriber` | output | Event + Commit | Projection | committed `EventRecord` / `DurableEvent` | after-commit only | live stream delivery, commit authority |
| `LlmExecutor` | dependency | Execution | Provider | inference request/result/stream | no durable authority | provider discovery policy, authorization |
| `ToolExecutor` / `Tool` | dependency | Execution | State, Effect, Permission | tool call, tool output, `StateCommand` | state changes stage through runtime | authorization ownership, direct store mutation |
| `Plugin` / `Contributions` / `ResolvedExecutionEnv` | dependency | Extension | Resolution, Execution, State, Event | hooks, tools, handlers, transforms, keys | only staged outputs can become durable | direct commits, product protocol names |
| `ProfileStore` | dependency | State/Profile | Extension | profile entries through registered keys | profile durability only | run/thread truth, product resource ownership |

## Port-Axis Intersection Matrix

Legend: `R` means the port may read or consume the axis data. `W` means the port
may create authoritative values for that axis. `R/W` means changes are allowed
only through the port's declared command or commit mechanism. `-` means no direct
interaction.

| Port | Activation | Live Control | Resolution | Execution | State | Event | Commit | Extension |
|---|---|---|---|---|---|---|---|---|
| `RuntimeCatalogInstaller` | - | - | W | - | - | - | - | - |
| `RunWithSnapshotExecutor` | W | - | R/W | R | - | - | - | - |
| `AgentSnapshotResolver` | - | - | R | - | - | - | - | - |
| `AgentSnapshotCatalog` | - | - | R | - | - | - | - | - |
| `RuntimeCapabilitySource` | - | - | R | - | - | - | - | R |
| `PluginManifest` | - | - | - | - | - | - | - | R |
| `RunExecutor` | R | R | R | W | R/W | W | W | R |
| `RuntimeRunContext` | R | R | - | R | - | - | R | - |
| `LiveRunControl` | - | W | - | R | - | - | - | - |
| `RuntimeInputHandle` / `RuntimeInputReceiver` | - | W | - | R | - | - | - | - |
| `RunResolver` | R | - | W | R | - | - | - | R |
| `CommitCoordinatorSource` | - | - | - | - | - | - | R | - |
| `CommitCoordinator` | R | - | - | - | W | W | W | - |
| `RuntimeResumeStore` | R | - | - | - | R | R | R | - |
| `StreamSink` | - | - | - | R | - | W | - | - |
| `DurableEventSink` | - | - | - | R | - | W | R | - |
| `EventReader` / `EventSubscriber` | - | - | - | - | - | W | R | - |
| `LlmExecutor` | - | - | R | W | - | - | - | - |
| `ToolExecutor` / `Tool` | - | - | R | W | R/W | - | - | R |
| `Plugin` / `ResolvedExecutionEnv` | - | - | R | R | R/W | R/W | - | W |
| `ProfileStore` | - | - | - | - | R/W | - | - | R |

When a new implementation wants to pass a value across a boundary, first mark the
row and column it needs. If the cell is `-`, either route through an existing port
that already owns the axis or make the new authority an explicit design change.

## Naming Quality

High-quality runtime names should reveal five things: domain context, authority,
durability, failure semantics, and the industry role being played. Prefer short
industry names inside precise modules, then export clear aliases at public
boundaries.

| Industry role | Preferred suffix | Use when | Avoid using it for |
|---|---|---|---|
| active work | `Executor` | a component performs model, tool, backend, or run execution | lookup, policy, storage |
| materialization | `Resolver` | a component turns specs/catalog data into an execution plan | durable dispatch, invocation |
| lookup table | `Registry` | a component returns installed/live objects by id | selection policy or execution |
| pushed output | `Sink` | caller pushes stream/event records outward | input buffering or storage |
| pulled input | `Source` | caller obtains an existing stream/handle/object | ownership of the produced object |
| durable persistence | `Store` | a component persists or reads durable records | policy, protocol projection |
| ordering/transaction | `Coordinator` | a component owns atomic ordering across writes | arbitrary orchestration |
| active steering | `Control` / `Command` | a component changes a running attempt | durable dispatch semantics |
| queued input | `InputBuffer` | external input is buffered before safe delivery | commit source or live channel |

Naming rules for future runtime changes:

1. Put context in the module path, not in long type prefixes.
2. Use `Stream*` for live, best-effort output and `Event*` aliases for committed neutral records; concrete durable storage types remain `DurableEvent*`.
3. Use `RunIngress` for the server-facing delivery boundary; keep delivery routing and input buffering as internal durable-ingress responsibilities unless they become externally testable contracts.
4. Use `Store` only for durable persistence and `Channel` for in-process ephemeral delivery.
5. Use `Context` only for per-attempt live wiring; use `Activation` for immutable
   run input and `Snapshot` for immutable executable configuration.
6. State names must include or imply scope: run, thread, profile, or product/shared.
7. Provider names must separate configured model-access instance, adapter family,
   model capability, backend profile, and executor. Do not name a future driver,
   discovery result, endpoint, or probe output as a provider unless it is literally
   the model-call target.
8. Do not introduce design-only type names when a clear stable domain name already exists.

## Plugin Contribution Matrix

Plugins are not a separate runtime. A plugin is an installable contribution set
collected during resolution and materialized into a `ResolvedExecutionEnv`
concept.

| Contribution | Registration surface | Runtime result | Boundary rule |
|---|---|---|---|
| Config schema | `Plugin::config_schemas` / `PluginConfigKey` | eager section validation | config is data; behavior still comes from registered code |
| State key | `register_key` | typed key and merge policy | plugin cannot mutate state without `StateCommand` or activation seed |
| Profile key | `register_profile_key` | typed profile access | profile storage remains behind its store contract |
| Phase hook | `register_phase_hook` | observe/mutate phase via `StateCommand` | no hook at `ToolGate`; gate uses the gate seam |
| Tool gate/policy | `register_tool_gate_hook` / `register_tool_policy_hook` | block, suspend, set result, or allow | policy is authorization; visibility is not |
| Continuation guard | `register_continuation_guard` | structured continuation decision | no product outcome semantics in runtime |
| Plugin tool | `register_tool` | tool enters the resolved tool set | still passes catalog visibility and permission gate |
| Request transform | `register_request_transform` | pre-inference request mutation | transform is runtime behavior, not protocol mapping |
| Scheduled action | `register_scheduled_action` | runtime request for later work | durable wake belongs to dispatch/server |
| Effect handler | `register_effect` | typed side-effect handling | durable truth still commits through runtime facts |
| Lifecycle seed | `on_activate` / `on_deactivate` | restricted state seed | no batch splicing or commit bypass |

`plugin_ids` controls which plugins are loaded. The active plugin scope controls
which loaded plugins contribute runtime behavior. An empty active scope means all
loaded plugins are active. When non-empty, it is a fail-closed allow-list by
canonical plugin id. Runtime-required plugins may be added to the effective scope
so core stop/context behavior cannot be accidentally filtered out.

## Tool Decision Ladder

The tool path has separate decisions. Keep them separate in code, docs, and UI.

```text
registered tool catalog
  -> merge global, official-builtin, unified delegation, dynamic, and plugin tools
  -> descriptor visibility (`ToolVisibilityPolicy`)
  -> model-visible descriptor list
  -> tool call arguments
  -> `ToolGateHook` / permission policy
  -> tool execution
  -> `ToolOutput` / `StateCommand`
  -> runtime commit
```

Visibility answers "can the model perceive this descriptor?" Authorization
answers "may this specific invocation run?" Execution answers "where and how does
the tool run?" These are independent decisions with explicit intersections:

- plugin tools are only a tool source; they do not bypass catalog visibility;
- official builtin tools are plugin tools from `awaken-ext-builtin-tools`, not
  runtime-core defaults;
- delegation is represented by one `agent_run` tool; target agent selection is
  the `agent_id` argument and must be checked against the resolved roster;
- catalog allow/exclude fields and step-time include/exclude filters use the
  same visibility semantics;
- permission rules can only authorize or block invocation after a call exists;
- backend or tool location is never an authorization grant.

## Independent Axes

These axes should stay independent unless a row below names the intersection.

| Axis | Values | Cohesion rule | Known intersection |
|---|---|---|---|
| Ownership | runtime, config domain, dispatch/server, admin/product, orchestration layer above | each context owns its vocabulary and truth | adapters translate public protocol terms |
| Delivery | direct, durable | delivery changes reliability, not runtime semantics | durable delivery requires commit and recovery wiring |
| Resolution | live, pinned | resolution chooses a catalog scope | persistent runs require a durable effective scope |
| Snapshot identity | inline snapshot, snapshot id | executable snapshot is the run/thread configuration identity | activation and resolution validate snapshot fingerprint before execution |
| Execution location | local, remote backend | execution location is invoked by id | backend capabilities must satisfy declared requirements |
| Tool decision | visibility, authorization, execution | each answers one question | permission preview must account for active plugin scope |
| Plugin activation | loaded, active, runtime-required | plugins contribute only through resolved `Contributions` | active scope affects hooks, gates, tools, transforms, and keys |
| Persistence | disabled, read-only, read-write | each call site holds only needed authority | read/write pairs derive from one coordinator |
| State scope | run, thread, profile, product/shared | state keys define merge and durability behavior | shared/product state stays behind approved ports |
| Public protocol | adapter input, runtime command, projected event | public names stop at adapters | streams and webhooks follow committed facts |

## Non-Orthogonal Junctions

These are deliberate joins. Make them visible instead of hiding them in helper
code.

- `plugin_ids` x active plugin scope: loading a plugin is not enough; a filtered
  plugin contributes nothing except runtime-required overrides.
- plugin tools x catalog visibility: plugin-provided tools join the same tool
  set as global, dynamic, and unified delegation tools, then visibility filters
  run once.
- builtin tools x runtime core: first-party hand, task, and delegation tools are
  installed from `awaken-ext-builtin-tools`; the core registry starts without
  concrete model-callable ids.
- unified delegation x permission: `agent_run` is the only delegation tool id;
  the resolved delegate/multiagent roster constrains the `agent_id` argument,
  and permission may further restrict that argument.
- permission x visibility: permission preview must first compute visible
  candidate tools, then apply unconditional permission effects.
- durable delivery x resolution: durable local execution needs a concrete pinned
  resolution id; remote endpoint execution may be pin-free when replay is owned
  by the remote backend.
- snapshot identity x activation: a run may carry inline executable snapshot data
  or a snapshot id; both paths converge before execution and must not treat
  `AgentId` alone as the resolved configuration.
- snapshot contract x configuration surface: snapshot listing, runtime capability
  reporting, and plugin config validation are read/validate surfaces, not config
  authoring or publication authority.
- durable delivery x commit: durable ingress construction requires a commit
  source, but commit ownership stays outside `RunExecutor`.
- backend capabilities x declared resources: MCP servers, skills,
  client-executed tools, decisions, and delegated tool execution are requirements
  checked before execution.
- activation x runtime context: immutable activation data and process-local wiring
  may be created together, but they cross different boundaries and must remain
  separately typed.

## Broad Interface Smell

A broad facade is a design smell when it can run, resolve, cancel, send
decisions, wake pending boundaries, expose registry versions, and expose commit
coordinators from one trait. That shape is convenient for one host, but it hides
authority and makes dependency checks weak.

Prefer a thin composition of the stable ports in the role catalog:

```text
RunWithSnapshotExecutor
  + RunExecutor
  + RunResolver
  + LiveRunControl
  + RuntimeCapabilitySource
  + CommitCoordinatorSource
```

One object may implement several ports internally. Call sites should depend only
on the port that matches the axis they need. This keeps implementation convenience
from becoming the public runtime contract.

## Simple Design Evaluation

The four rules of simple design are applied here in this order:

1. Passes the tests.
2. Reveals intention.
3. Has no duplication.
4. Has the fewest elements.

### Passes The Tests

The design satisfies this rule when every boundary row has an executable
enforcer:

- dependency checks keep product/server names out of runtime crates;
- serde and fingerprint tests prove `ResolvedSpec` is data-only and stable;
- inline/by-id snapshot tests prove `ExecutableAgentSnapshot` is the executable
  configuration identity and `AgentId` is not treated as enough to run;
- `RunIngressCapabilities` tests prove direct ingress rejects durable-only work;
- commit tests prove durable writes go through one coordinator;
- hook/filter/no-bypass tests prove plugin contributions cannot skip permission
  or commit staging.

If a new boundary has no test, the design is not ready even if the prose looks
correct.

### Reveals Intention

The role split improves intention because each trait names one authority:
execute, steer, resolve, expose commit source, or deliver through ingress. Use
the role names above in new designs and avoid broad controller names.

### Has No Duplication

The design mostly satisfies this rule by centralizing repeated decisions:

- `ResolvedSpec` is the canonical resolved data payload inside executable
  snapshots and pinned replay input;
- `ToolVisibilityPolicy` is the canonical descriptor visibility rule;
- `Plugin::resolve` returning `Contributions` is the only way plugins contribute
  runtime behavior;
- `CommitCoordinator` is the durable write boundary.

Duplication returns when UI preview, server validation, and runtime filtering
reimplement the same rules separately. The fix is to call the canonical policy
or add a shared value object before copying logic.

### Has The Fewest Elements

The design satisfies this rule if the role split replaces larger abstractions
rather than adding a second framework. `RunExecutor`, `LiveRunControl`,
`RunResolver`, and `CommitCoordinatorSource` are justified because each carries
different authority and a different failure mode. A universal execution framework,
`CapabilityProfile`, or plugin runtime would violate this rule until multiple
implemented slices need the same invariant.

The test for a new element is simple: if it does not own a distinct authority,
failure mode, and guardrail, fold it into an existing port.

## Development Checklist

Before changing runtime interfaces, name:

1. the axis being changed;
2. the role or value object that owns the new authority;
3. whether the value crosses as data or as a live handle;
4. the fail-closed behavior;
5. the guardrail and test;
6. the first vertical slice.

If any answer is "several things", split the proposal before implementing it.
