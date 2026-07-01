# Config To Run Execution Flow

This document defines the target end-to-end flow from persisted configuration to
run activation, resolution, execution, and commit. It is design guidance, not an
implementation transcript.

Use this document when a change touches config publication, protocol run
parsing, agent resolution, tool catalog materialization, or the execution loop.

## Purpose

The flow has four separate jobs:

1. Configuration is data owned by an independent config domain.
2. Publication compiles a config snapshot into versioned runtime-facing data.
3. Runtime install accepts a complete publication and atomically exposes it to
   resolution.
4. Execution consumes an executable snapshot through runtime ports and commits
   durable truth.

Do not collapse these jobs into one "runtime controller." The runtime core should
not own config CRUD, public protocol DTOs, admin assistant tools, durable
delivery internals, or concrete model-callable builtin tool ids.

These jobs sit inside the broader runtime-facing axis model:

| Primary axis | Role in this flow |
|---|---|
| Configuration publication | produces the published catalog and fingerprint that resolution may trust |
| Live control | delivers cancellation, decisions, wakeups, and tool results into active execution |
| Execution | consumes activation plus resolution output and stages durable runtime truth |

Activation, resolution, state, event, wait/resume, commit, and
extension are supporting axes. They must remain explicit because they determine
where public protocol input ends, where runtime authority begins, and where
durable truth becomes visible.

## Flow Contract

```text
ConfigStore records
  -> ConfigSnapshot
  -> RegistryCompiler
  -> RegistryPublication
  -> RuntimeCatalogInstall
  -> RuntimeCatalogInstaller
  -> ExecutableAgentSnapshot or ExecutableAgentSnapshotId
  -> RunWithSnapshotCommand
  -> RunActivation + RuntimeRunContext
  -> ResolvedSpec / ResolvedRun / ResolvedExecutionEnv
  -> prepared agent loop
  -> LLM and tool execution through ports
  -> ThreadCommit and committed runtime facts/events
  -> protocol or product projection
```

Each arrow has one owner and one allowed interface:

| Stage | Owner | Input | Output | Interface |
|---|---|---|---|---|
| Config authoring | Config Domain | admin/config API input, builtin seed, operator draft | versioned config records | `ConfigStore`, `ConfigRecord`, typed specs |
| Config load | Config Domain | namespaced config records | `ConfigSnapshot` plus fingerprint and source revisions | `ConfigSnapshotLoader` |
| Publication coordination | Config Application | config snapshot plus environment discovery results | complete publication attempt | `ConfigPublicationCoordinator` |
| Registry compile | Config Domain | `ConfigSnapshot` and prepared catalog inputs | `RegistryPublication` plus runtime install data | `RegistryCompiler` |
| Runtime catalog install | Runtime Core adapter | complete `RuntimeCatalogInstall` | active runtime catalog version and resolver view | `RuntimeCatalogInstaller` |
| Executable snapshot selection | Runtime-facing contract plus config surface adapter | inline executable snapshot data or `ExecutableAgentSnapshotId` | `ExecutableAgentSnapshot` for one run/thread scope | `RunWithSnapshotExecutor`, `AgentSnapshotResolver`, `AgentSnapshotCatalog` |
| Run parsing | Product adapter / Server route | public protocol payload, resume decisions, client-executed tools | neutral `RunActivation` data plus optional runtime context wiring | anti-corruption adapter, `RunIngress` |
| Run delivery | Dispatch / Server | `RunActivation` and `RuntimeRunContext` | direct execution attempt or durable dispatch | `DirectRunIngress` or `DurableRunIngress` |
| Run resolution | Runtime Core | activation agent id or pinned resolved data, registry fingerprint | `ResolvedRun` and `ResolvedExecutionEnv` | `RunResolver`, `AgentResolver` |
| Execution preparation | Runtime Core | resolved agent, messages, inherited context | prepared loop state | `RunExecutor`, phase runtime |
| Step execution | Runtime Core and runtime extensions | prepared loop state | stream events, state commands, tool outputs | `LlmExecutor`, `Tool`, plugin hooks, tool gates |
| Commit and projection | Runtime Core, Stores, Protocol adapters | commit plan and event drafts | committed facts/events and projected public stream | `CommitCoordinator`, event reader/subscriber |

No stage may smuggle live handles through a data boundary. If a later stage needs
a live object, it rebuilds or looks it up through the approved runtime catalog.

## Runtime Configuration Axis

There is a configuration axis that affects runtime behavior, but it is not
runtime-owned config CRUD or authoring.

Use this split:

| Concern | Owner | Runtime relationship |
|---|---|---|
| config CRUD, drafts, user overrides, builtin seed writes | Config Domain | outside runtime core and outside admin/product ownership |
| config validation and publication | Config Domain | produces `RegistryPublication` |
| registry install and version swap | Runtime Core adapter | accepts `RuntimeCatalogInstall` and replaces the active catalog after validation |
| run-time resolution | Runtime Core | consumes the published catalog and fingerprint |
| per-step live catalog refresh | Runtime Core resolver | observes registry version changes only at safe step boundaries |

The explicit axis is:

```text
ConfigStore
  -> ConfigSnapshot
  -> ConfigPublicationCoordinator
  -> RegistryCompiler
  -> RegistryPublication
  -> RuntimeCatalogInstall
  -> RuntimeCatalogInstaller
  -> RunResolver
```

This axis must be documented and tested because it is where many accidental
boundary leaks happen. The wrong design is a runtime service that edits config,
publishes registries, parses public DTOs, and runs loops. The clean design is an
independent config publication pipeline plus a resolver inside runtime.

## Config Graph Model

Configuration is a graph of versioned specs. The graph is authored and validated
inside the config domain, then frozen into a `ConfigSnapshot` before publication.
Runtime consumes only the compiled publication, executable snapshots, and
resolved runtime-facing data.

The core graph is:

```text
ModelProviderSpec
  -> model-access provider instance capability evidence
  -> opaque CredentialRef

ModelSpec
  -> ModelProviderSpec ref
  -> model capability metadata

ModelPoolSpec
  -> ordered or policy-selected ModelBinding candidates
  -> explicit fallback/routing policy

AgentSpec
  -> model selection ref: ModelSpec or ModelPoolSpec
  -> ToolSpec / SkillSpec / plugin refs
  -> instructions, visibility policy, and capability requirements

RegistryCompiler
  -> validates graph references
  -> produces RegistryPublication and RuntimeCatalogInstall data

ExecutableAgentSnapshot
  -> freezes the resolved agent graph for one run/thread scope
```

### Spec Responsibilities

| Spec | Owns | References | Must not own |
|---|---|---|---|
| `ModelProviderSpec` | configured model-access provider instance identity, endpoint/config knobs, declared model/backend capability evidence, opaque credential refs | credential domain by `CredentialRef`; model-provider adapter family id | secret material, authorization grants, agent behavior, runtime executor handles, agent runtime family |
| `ModelSpec` | model identity, provider-specific model name, model capability metadata, limits such as context window/modalities/structured-output support | one `ModelProviderSpec` or model-provider instance ref | fallback policy, credential selection, runtime invocation state |
| `ModelPoolSpec` | explicit model selection policy such as priority, weights, fallback order, allowed downgrade rules, and health/availability policy input | `ModelSpec` refs or `ModelBinding` candidates | ad hoc provider search during execution, authorization, hidden fallback |
| `AgentSpec` | agent behavior assembly: instructions, model selection ref, tool visibility policy, plugin activation scope, skill/resource refs, capability requirements | `ModelSpec` or `ModelPoolSpec`, `ToolSpec`, `SkillSpec`, plugin ids, resource refs | provider credentials, tool implementations, concrete launch/endpoint fields, live registries, public protocol state |
| `ToolSpec` / `SkillSpec` | model-visible descriptor data, content hash, execution reference, and runtime-visible requirements | plugin/runtime catalog entries, environment or remote execution refs | permission grant, provider selection, direct store mutation |

This keeps the model simple: model providers describe where model calls can go,
models describe what can be called, model pools describe explicit selection
policy, and agents assemble behavior by reference.

This graph is intentionally ready for later integrations without naming them
today. Future discovery, driver, platform, or environment records may feed
capability evidence, backend profiles, tool descriptors, or executable snapshot
data into publication. They must not be hidden inside `ModelProviderSpec` or
`AgentSpec` just because they help an agent run. Add a new config record only when
it has a distinct authority boundary, lifecycle, and tests; until then, use the
existing slots: model capability on `ModelSpec`, selected
model-provider/model/backend data on `ModelBinding`, backend requirements on the
resolved run, and immutable execution identity on `ExecutableAgentSnapshot`.

### Model Selection And Binding

`ModelSpec` is configuration data. `ModelBinding` is the selected runtime-facing
binding for a run. `ModelPoolSpec` may produce a `ModelBinding`, but only before
activation or during a documented safe refresh boundary.

```text
AgentSpec.model_ref
  -> ModelSpec
  -> ModelBinding { model_provider_ref, model_ref, backend_ref, capability profile }

AgentSpec.model_pool_ref
  -> ModelPoolSpec
  -> selected ModelBinding according to explicit policy
```

Runtime may validate the selected binding and reject it. Runtime must not search
for a different model provider or model during execution. If fallback is allowed,
the fallback candidates and downgrade rules must be explicit in `ModelPoolSpec` or
in adapter input accepted before activation.

### Reference Rules

1. Specs reference other specs by stable ids plus source revisions or publication
   fingerprint, not by live handles.
2. A config snapshot must reject missing model provider, model, model pool,
   agent, tool, skill, plugin, backend, or resource refs before publication.
3. A model pool must make fallback order, weighting, health inputs, and downgrade
   rules explicit; hidden provider search is invalid.
4. `AgentSpec` may select a model or a model pool, but the executable snapshot
   must contain the effective binding or enough resolved data to reproduce it.
5. Credential refs remain opaque and are resolved only by approved credential
   ports; compatibility does not authorize use.
6. Tool visibility, tool authorization, and tool execution location stay separate
   even when `AgentSpec` references tools.
7. Concrete process, endpoint, probe, and transport details may enter
   runtime only as validated publication inputs or snapshot data, never as mutable
   fields on `ModelProviderSpec` or `AgentSpec`.
8. The compiled publication records enough source revisions and fingerprints to
   explain which model-provider/model/tool graph produced a run.

### Compilation Output

`RegistryCompiler` validates the graph and produces runtime-visible data:

```text
validated config graph
  -> resolved agent graph
  -> selected or selectable model bindings
  -> descriptor fingerprints
  -> capability requirements
  -> RegistryPublication
  -> RuntimeCatalogInstall
```

The compiler may prepare provider capability evidence, backend profiles, and
other runtime-visible catalog inputs, but runtime still performs final binding and
capability validation against the installed catalog before execution. Discovery
or probe outputs are evidence; they do not become authorization grants and do not
rewrite authored specs during execution.

## Publication Objects Are Outside Runtime

`ConfigPublicationCoordinator` and `RegistryCompiler` are not runtime objects.
They belong to the config side of the boundary. `RuntimeCatalogInstaller` is
listed here only to make the handoff explicit.

| Object | DDD role | Owner | Owns | Must not own |
|---|---|---|---|---|
| `ConfigPublicationCoordinator` | application service | Config Application / Server | ordering the publish transaction, invoking loaders, discovery adapters, compiler, versioned store, and runtime installer | run execution, live control, public protocol DTOs, agent truth commits |
| `RegistryCompiler` | domain service | Config Domain | validating the config graph and producing a complete `RegistryPublication` / install candidate | runtime loop state, active-run steering, HTTP/admin routes, mutable runtime handles |
| `RegistryPublication` | value object | Config Domain | immutable publication identity, version, source revisions, and fingerprint | live runtime handles, runtime loop state, config CRUD workflow |
| `RuntimeCatalogInstall` | command value | Runtime-facing contract | complete install request derived from a publication and compiled runtime-visible catalog data | config authoring, publication compilation, admin workflow |
| `RuntimeCatalogInstaller` | runtime-facing port | Runtime Core adapter | validating and atomically installing a complete catalog publication | config CRUD, publication compilation, versioned registry storage, admin workflow |

The coordinator may call runtime through `RuntimeCatalogInstaller`, but it does
not become part of runtime. The compiler may produce runtime-facing data, but it
does not execute runs and does not hold live runtime registries. Runtime stays on
the right side of the boundary: install a complete catalog, resolve executable
snapshots, run, and commit.

## Implemented Run Input: RunnableConfig (ADR-0032)

The names above (`RegistryPublication`, `RegistryCompiler`,
`ConfigPublicationCoordinator`) are design-level. The implemented seam bundles the
runtime-facing data into one value object: **`RunnableConfig`**
(`awaken-runtime-contract`), pairing the `ExecutableAgentSnapshot` with its
`RuntimeCatalogInstall` under one fingerprint. The runtime consumes a
`RunnableConfig`; it does not juggle the two parts. This is the owning description
of the implemented run input — other docs link here rather than restate it.

`RunnableConfig` has two producers and one consumer:

- **`RunnableConfig::builder`** — the single assembly path. A direct caller builds
  one by hand (no config store), stamping the agent id as the consistency token.
- **`compile()`** (`awaken-config-store`) — a thin wrapper over the builder that
  resolves tool ids and stamps the content hash (`sha256`); the config side of the
  boundary, and optional.
- **`Runtime::run`** (or `install_catalog` + `execute` for the durable path) — the
  runtime installs the config's catalog (idempotent) and resolves the snapshot
  against it, fail-closed (G4/G28). The runtime never computes the fingerprint.

The earlier in-memory `Publication` value is removed — `RunnableConfig` subsumes
it. The durable `StoredPublication` and the publication lifecycle
([config-publication-lifecycle.md](config-publication-lifecycle.md)) are unchanged.

**Driving a run (ADR-0033).** The runtime consumes a `RunnableConfig` through two
in-process entries over the `execute`/`resume` primitives: `run` (single-shot) and
`run_to_completion(config, thread, input, ctx, decide)`, which owns the
`execute → (park → decide → resume)* → end` loop. A parked run is a question
(`WaitingTicket`); the answer is a `ResumeResult`, supplied in-process by the
`decide` closure or across a boundary by the durable dispatch queue — the same
protocol, two drivers.

## Snapshot Execution And Inspection Contract

The runtime also has an internal execution and inspection contract for
executable agent snapshots. It is separate from config authoring and
publication.

An `AgentId` is only a selection key. It is not the full executable
configuration identity. The executable identity for a run or thread is an
`ExecutableAgentSnapshot`, and the same agent id may map to different snapshots
in different runs.

The contract supports two execution inputs:

```text
AgentSnapshotInput::Inline(ExecutableAgentSnapshot)
AgentSnapshotInput::ById(ExecutableAgentSnapshotId)
```

Both paths converge before execution:

```text
inline snapshot
  -> validate fingerprint and capability requirements
  -> RunActivation / resolution
  -> execution

snapshot id
  -> AgentSnapshotResolver::get_snapshot(id)
  -> validate fingerprint and capability requirements
  -> RunActivation / resolution
  -> execution
```

Configuration surfaces may also call read/validate ports:

```text
AgentSnapshotCatalog::list_snapshots(query)
RuntimeCapabilitySource::runtime_capabilities()
validate_section(manifest, key, value)
```

These are internal runtime-facing APIs. They are useful for configuration
surfaces, admin routes, tests, and local tooling, but they are not config CRUD,
not publication workflow, and not a public protocol management surface. A config
service may implement the snapshot resolver or catalog, but the runtime only
depends on the contract and the immutable snapshot data returned by that
contract.

The minimal runtime configuration surface is:

| Operation | Port | Runtime authority |
|---|---|---|
| install a complete catalog | `RuntimeCatalogInstaller` | atomically expose a complete catalog to future resolution |
| run from inline or by-id snapshot data | `RunWithSnapshotExecutor` | start execution from immutable executable configuration |
| resolve one snapshot id | `AgentSnapshotResolver` | read immutable executable snapshot data |
| list executable snapshots | `AgentSnapshotCatalog` | expose read-only summaries for configuration surfaces |
| report runtime capabilities | `RuntimeCapabilitySource` | return catalog fingerprint, runtime version/build identity, tools, plugins, schemas, and backend profiles |
| validate plugin config | `validate_section` | validate one plugin-owned config section against its declared schema |

None of these operations creates, edits, publishes, deletes, or drafts config.

## Stage 1 - Config Authoring

Config authoring creates versioned records. It is not runtime execution.

The config domain owns:

- `ModelProviderSpec`, `ModelSpec`, `ModelPoolSpec`, `AgentSpec`, `ToolSpec`,
  `SkillSpec`, MCP server specs, A2A server specs, and related config records;
- draft and publish workflows;
- admin/operator validation and audit policy;
- builtin seed application and user override merge rules.

The runtime core sees none of the authoring workflow. It only sees the published
data selected for a run.

The storage interface is a namespaced config repository:

```text
ConfigStore::get(namespace, id)
ConfigStore::list(namespace, offset, limit)
ConfigStore::put(namespace, id, value)
ConfigStore::put_if_revision(namespace, id, value, expected_revision)
ConfigStore::delete_if_revision(namespace, id, expected_revision)
```

The store owns config durability and revision checks. It does not own runtime
execution semantics.

## Stage 2 - Config Loading And Publication

The snapshot loader loads config records into a single snapshot:

```text
providers
models
model pools
agents
a2a servers
mcp servers
tools
skills
source config revisions
fingerprint
```

The snapshot is still data. The publication coordinator may prepare adjacent
execution catalog inputs, such as MCP tool descriptors, A2A discovered agents,
skill specs, and provider capability evidence, but those preparation steps belong
above the runtime core.

The registry compiler then compiles the snapshot into a complete publication
candidate. Runtime installation is a separate handoff through
`RuntimeCatalogInstaller`; a failed compile or install leaves the previously
active runtime catalog intact.

Target interface:

```text
trait ConfigPublicationCoordinator {
    async fn publish(snapshot: ConfigSnapshot) -> Result<ConfigPublicationReport>;
}

trait RegistryCompiler {
    fn compile(snapshot: ConfigSnapshot) -> Result<RegistryPublication>;
}

trait RuntimeCatalogInstaller {
    fn install_catalog(install: RuntimeCatalogInstall) -> Result<RuntimeCatalogInstallResult>;
}

struct RegistryPublication {
    publication_id: PublicationId,
    version: PublicationVersion,
    fingerprint: CatalogFingerprint,
    source_revisions: Vec<ConfigRevisionRef>,
}
```

The names are design-level; implementation may split the service by module. The
authority is not optional: publication coordination and registry compilation are
config-side behavior, not runtime loop behavior and not public protocol behavior.

## Stage 3 - Registry Materialization

`RegistrySet` is the runtime-facing aggregate of lookup ports:

```text
agents
tools
models
providers
plugins
backends
```

The runtime consumes the registry set through resolver ports. It must not know
which admin workflow, seed, draft, tenant policy, or route produced it.

Tool registry composition follows the same rule:

```text
base registered tools
  + official builtin tools from awaken-ext-builtin-tools
  + dynamic MCP or federated tools
  + plugin tools
  + unified delegation tool agent_run
```

Collisions fail closed. Provenance should be explicit metadata on catalog
members, not inferred from string prefixes.

The runtime core starts with no concrete model-callable tool ids. Official tools
come from `awaken-ext-builtin-tools`; admin assistant tools come from a private
admin registry and are not part of ordinary agent catalogs.

## Stage 4 - Run Activation

Run parsing is the adapter step that converts public payloads into neutral
runtime input. It ends at `RunActivation`. When the caller supplies an
executable snapshot or snapshot id, snapshot resolution occurs before or during
activation construction, but the result remains immutable runtime input.

Protocol adapters own:

- request DTO decoding;
- public thread/session/id mapping;
- client-executed tool descriptor conversion;
- resume decision conversion;
- tool result conversion;
- public error and stream encoding;
- auth, tenant, and product policy checks before calling runtime ports.

`RunActivation` owns only neutral immutable run input:

```text
agent snapshot input: inline ExecutableAgentSnapshot or ExecutableAgentSnapshotId
intent
input messages or already-persisted input reference
options and inference overrides
trace context
```

It must not contain public protocol DTOs, route state, admin config records, live
registry handles, config CRUD handles, channels, commit coordinator overrides,
stream sinks, or durable queue internals.

`RuntimeRunContext` carries per-attempt live wiring:

```text
cancellation token
runtime input receiver and decision/wake handles
stream sink
commit coordinator source or persistence access mode
thread context cache
pinned resolver scope or optional pre-resolved plan
```

The context is process-local wiring, not durable input. Direct execution may build
it beside `RunActivation`; durable delivery must persist only the data needed to
recreate it later. This split is the boundary between adapter parsing and runtime
wiring.

Delivery then goes through `RunIngress`:

```text
RunIngress::submit(RunActivation, RuntimeRunContext)
RunIngress::submit_background(RunActivation)
RunIngress::cancel(run_id)
RunIngress::send_decision_live(run_id, tool_call_id, decision)
```

`DirectRunIngress` may execute inline. `DurableRunIngress` may buffer, claim,
lease, recover, and replay dispatches. Those are delivery semantics; they do not
change the runtime execution model.

If an implementation wants one convenient request object, keep it data-only and
name live wiring separately. A request that mixes activation data with live
channels, commit coordinators, registry handles, and dispatch records is not a
runtime contract; it is host glue.

## Stage 5 - Run Resolution

Resolution materializes the run. It does not execute the loop.

The target resolution pipeline is:

```text
receive ExecutableAgentSnapshot or resolve ExecutableAgentSnapshotId
  -> validate catalog fingerprint and runtime capability requirements
  -> lookup or materialize agent spec from resolved snapshot data
  -> resolve model/provider binding
  -> resolve plugins and active hook scope
  -> merge tool sources
  -> apply descriptor visibility
  -> produce ResolvedRun / ResolvedExecutionEnv
```

Resolution outputs may include:

- resolved agent spec or serializable `ResolvedSpec`;
- selected upstream model and provider executor;
- backend profile and capability checks;
- visible tool descriptors and executable tool map;
- selected plugins, hooks, gates, transforms, keys, handlers, and state
  registrations;
- catalog fingerprint or live registry version.

The config edge remains data-only: `ResolvedSpec` plus a catalog fingerprint.
The runtime validates the fingerprint and builds live execution objects from its
own catalog. Snapshot ids are handles for immutable executable snapshot lookup,
not proof that an agent id has one globally fixed configuration.

## Stage 6 - Execution Preparation

Execution preparation starts after resolution. It installs only the state needed
for the loop:

1. Resolve or receive the initial agent.
2. Register plugin state keys.
3. Run plugin activation seeds through the state command path.
4. Trim or restore history according to committed compaction boundaries.
5. Merge loop-owned client-executed tools without publishing them into
   the global registry.
6. Establish `RuntimeRunContext` wiring for cancellation, decisions, checkpoint,
   sink, and thread context.

Preparation is still not a protocol adapter. It must not know public protocol,
admin assistant, or public session names.

## Stage 7 - Step Execution

Each step follows the same high-level order:

```text
cancellation check
  -> StepStart hooks
  -> BeforeInference hooks
  -> LLM inference
  -> AfterInference hooks
  -> natural end or tool-call handling
  -> tool gate / permission
  -> tool execution
  -> tool output and StateCommand staging
  -> continuation, suspension, blocked, or next step
```

`LlmExecutor` owns model invocation. `Tool` owns one tool implementation.
`ToolGateHook` and permission policy own invocation authorization. Plugin hooks
may return state commands, but they do not commit directly.

Client-executed tools are ordinary resolved tool descriptors whose
execution suspends on a pending result instead of running local code.
The runtime owns the pending call id, authorization state, deadline, and resume
validation. Protocol adapters may project the wait to their public tool-use event
and convert the later result back into `RunIngress` / `LiveRunControl`, but public
tool-use vocabulary and public result ids never enter the runtime core.

Tool lifecycle remains explicit:

```text
visible descriptor
  -> model tool call
  -> argument validation
  -> gate and permission decision
  -> execution through the selected executor
  -> ToolOutput
  -> StateCommand
  -> commit boundary
```

Visibility is perception. Permission is authorization. Execution location is
where the call runs. These must remain separate even if one implementation
computes several of them.

## Stage 8 - Commit And Projection

The runtime proposes durable changes; the commit coordinator writes them.

Commit input includes:

- new messages;
- run lifecycle and terminal reason;
- persisted state export;
- event drafts and runtime facts;
- tool and hook effects represented as state commands or scheduled effects.

`CommitCoordinator` owns atomic durable visibility. Public streams, protocol
replay rows, webhooks, admin views, and product outcomes are projections over
committed facts/events.

Live `StreamSink` output is useful for user experience, but it is not durable
truth. Replay must be reconstructed from committed state, facts, and event
records.

## Interface Summary

| Interface | Belongs to | Clean responsibility |
|---|---|---|
| `ConfigStore` | config contract | versioned config record durability |
| `ConfigPublicationCoordinator` | config application | coordinate one publish transaction outside runtime |
| `RegistryCompiler` | config domain | compile and validate config snapshots into complete publications |
| `RegistryPublication` | config contract | immutable publication identity, version, source revisions, and fingerprint |
| `RuntimeCatalogInstall` | runtime-facing data value | complete install request derived from one publication and its runtime-visible catalog data |
| `RuntimeCatalogInstaller` | runtime-facing internal port | atomically install a complete runtime catalog publication |
| `ExecutableAgentSnapshot` | runtime-facing data value | complete resolved configuration for one run/thread scope |
| `RunWithSnapshotExecutor` | runtime-facing internal port | accept inline executable snapshot or executable snapshot id and submit execution |
| `AgentSnapshotResolver` | runtime-facing internal port | resolve `ExecutableAgentSnapshotId` into immutable executable snapshot data |
| `AgentSnapshotCatalog` | runtime-facing internal port | list current executable snapshots for configuration surfaces |
| `RuntimeCapabilitySource` | runtime-facing internal port | report installed runtime plugin/tool/backend capabilities |
| `PluginManifest` | runtime-facing contract value | declare config sections and `CapabilityBound`; validate via the single `validate_section` |
| `RunActivation` | runtime contract | owned neutral run input |
| `RuntimeRunContext` | runtime host / runtime contract | per-attempt live wiring kept separate from durable activation data |
| `RunIngress` | dispatch/server | submit/control delivery semantics |
| `RunResolver` | runtime | produce a resolved execution plan from an installed catalog and executable snapshot data |
| `ResolvedSpec` | config-to-runtime data value | replayable resolved input plus descriptor fingerprint |
| `ResolvedExecutionEnv` | runtime | materialized plugins, hooks, tools, keys, and runtime facts for one run |
| `LlmExecutor` | provider adapter | model inference |
| `Tool` | runtime extension or environment adapter | one executable tool contract |
| `CommitCoordinator` | agent-domain store contract | atomic durable runtime write |
| `StreamSink` | runtime output port | live best-effort progress delivery |

API signatures belong in Rustdoc. This table records ownership and direction.

## Naming Boundary For Config, Admin, And Product Adapters

Neutral protocol and runtime code must not use product hosting words such as
`managed` in crate names, module names, type names, tool ids, or public protocol
fields. That word is reserved for external product adapters or historical
source-document discussion.

Use these names by authority:

| Name | Use for | Do not use for |
|---|---|---|
| `awaken-config-contract` | config records, config store ports, registry graph, model/agent definitions, published runtime input data | admin assistant tool execution, runtime loop internals, product policy |
| `awaken-runtime-contract` | internal runtime-facing ports for snapshot execution, snapshot lookup/listing, runtime capability, plugin config validation, activation, and execution entrypoints | config CRUD, admin workflow, public protocol DTOs, durable ingress internals |
| `awaken-admin-contract` | optional admin-only route DTOs or reusable admin API values, if they become stable | general config publication or runtime resolution data |
| `awaken-admin-assistant-tools` | private admin assistant tools and registry | ordinary builtin tools or agent-visible runtime catalogs |

If an admin assistant tool validates or drafts config, it may depend on config
contracts. The dependency must not invert: config contracts must not depend on
admin assistant tools.

## Elegance Rules

The elegant target design is:

1. Config CRUD and publish are outside runtime core.
2. Publication coordination and registry compilation are outside runtime core.
3. Published config crosses as complete install data and fingerprints, not live
   handles or partial mutable registries.
4. Runtime execution accepts inline snapshots or snapshot ids; `AgentId` alone is
   not treated as a complete run configuration.
5. Configuration surfaces use snapshot and capability ports, not runtime-owned
   config CRUD.
6. Protocol run parsing ends at `RunActivation`.
7. Per-attempt live handles travel through `RuntimeRunContext`, not activation or
   snapshot data.
8. Run delivery is `RunIngress`; direct and durable differ only by delivery
   guarantees.
9. Resolution produces a plan and execution environment; execution consumes them.
10. Runtime core owns tool abstractions, not concrete tool ids.
11. Builtin tools live in `awaken-ext-builtin-tools`.
12. Admin tools live in `awaken-admin-assistant-tools` and a private registry.
13. Sub-agent delegation uses one `agent_run` tool with an `agent_id` argument.
14. Commit is the only durable runtime write boundary.
15. Neutral code and protocol names do not use product hosting vocabulary.

When an implementation feels simpler by violating one of these rules, prefer a
smaller adapter at the boundary over a larger runtime interface.

## First Vertical Slice

A development-ready slice for this flow should prove:

1. config records for one provider, model, model pool, agent, and tool can be
   stored with revision checks;
2. `RegistryCompiler` validates the config graph and rejects a missing or
   conflicting ref;
3. model-pool selection produces an explicit `ModelBinding` before activation;
4. `RegistryCompiler` produces a fingerprinted `RegistryPublication`;
5. `RuntimeCatalogInstaller` installs the publication atomically;
6. an inline `ExecutableAgentSnapshot` and an `ExecutableAgentSnapshotId` converge into the
   same validated execution path;
7. configuration surface ports list snapshots, report runtime capabilities, and
   validate plugin config without config CRUD through runtime;
8. a protocol adapter builds a `RunActivation` without protocol DTO leakage and a
   separate `RuntimeRunContext` without durable-data leakage;
9. `RunIngress` submits through both direct and durable modes where supported;
10. the resolver produces a visible tool catalog and execution environment;
11. the loop executes one model step and one gated tool call;
12. state and events commit through `CommitCoordinator`;
13. replay/projection reads committed facts, not live stream output.

Each item needs a test, conformance case, or dependency check before the slice is
implementation-ready.

## Guardrails

G1, G2, G3, G4, G5, G6, G8, G9, G10, G13, G14, G28, and G29 in
[INVARIANTS](../INVARIANTS.md).
