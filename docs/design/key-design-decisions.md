# Key Design Decisions

These decisions are the load-bearing rules for new work. Each decision should be
read as: use the existing domain language first, add a new abstraction only when a
current port cannot express the behavior, and make the enforcement mechanical.

Each decision here owns its rationale and boundary; the mechanical enforcer and
the test that proves it live as a guardrail in [INVARIANTS.md](../INVARIANTS.md)
(G1–G29). Decisions are not restated as guardrails, and guardrails do not restate
the rationale.

---

## D1 - Runtime Core Is Not The Product

**Problem.** A product-first design makes the runtime depend on public protocol
names, hosted policy, vaults, resource data, and deployment choices.

**Decision.** The runtime core owns only what is needed to execute and persist one
run. Server, protocol, and product behavior live above it.

**Consequence.** Runtime code may name `AgentRuntime`, `RunActivation`,
`StreamSink`, `CommitCoordinator`, typed state/tools, and extension hooks. It runs
tools in-process (ADR-0007). It may not name Managed Agents DTOs, public event
names, tenant policy, registry publication workflow, vault schemas, or remote
execution placement.

---

## D2 - The Runtime/Server Seam Is A Gated Port

**Problem.** Letting server code import any runtime internals silently widens the
contract and makes future repository splits risky.

**Decision.** The server consumes the runtime through a fixed port list:
`AgentRuntime`, `RunActivation`, `RuntimeRunContext`,
`ResolvedSpec`/resolver output, `RunExecutor`, `LiveRunControl`, `RunResolver`,
`CommitCoordinatorSource`,
`RunWithSnapshotExecutor`, `AgentSnapshotResolver`, `AgentSnapshotCatalog`,
`RuntimeCapabilitySource`, `PluginManifest`,
`StreamSink`, `Plugin`, `Contributions`, `CommitCoordinator`, and
runtime store traits.

**Consequence.** Adding a new cross-boundary type is an architecture change, not a
local convenience. Public API and dependency checks must fail until the new port
is reviewed.

---

## D3 - Config Sends Data, The Kernel Builds Live Objects

**Problem.** Passing live registries, factories, pins, or scope objects into the
kernel couples runtime execution to config provenance and blocks out-of-process
or remote execution.

**Decision.** The config domain sends a serializable `ResolvedSpec` plus a
catalog fingerprint. The runtime validates the fingerprint and instantiates live
objects from its own registered catalog.

**Consequence.** `ResolvedRun` stays runtime-internal. Publication ids, tenant
scopes, drafts, and authoring workflow are config-domain concepts. Executable
snapshot ids are runtime-facing handles resolved through `AgentSnapshotResolver`;
the runtime sees immutable snapshot data it can validate, not provenance it must
understand.

---

## D4 - Dispatch Is Additive Over Runtime Control

**Problem.** Durable ingress semantics can become entangled with basic runtime
execution, making simple runtime use pay for buffering and dispatch machinery.

**Decision.** Use one `RunIngress` port with two implementations:
`DirectRunIngress` for direct queue-less runtime control, and
`DurableRunIngress` for durable buffering, recovery, pending input, and replay.

**Consequence.** Runtime control works without durable ingress internals.
Durable-only operations fail closed on the weak ingress. Durable ingress behavior
is additive, not a parallel runtime.

---

## D5 - Commits Are The Only Durable Runtime Write Boundary

**Problem.** Side writes for waits, protocol events, or projections race the
canonical runtime commit and break replay.

**Decision.** Runtime facts, terminal state, and coordinator-staged waits go
through `CommitCoordinator`. Public streams and webhooks are projections after
commit.

**Consequence.** Consumers reconstruct truth from committed facts. Public event
names and protocol-specific status are applied by adapters after the commit.

---

## D6 - Capabilities Are Segmented, Not Centralized Into A God Object

**Problem.** A single capability object for descriptors, execution bytes, mutable
operator policy, secrets, and session data becomes too broad to reason about.

**Decision.** Split capability configuration into segments:
decision-surface descriptors in pinned config, execution behavior owned by the
runtime/extension and invoked in-process by id, operator overlay in
config/admin/product policy, secrets in the data plane, and session data in
runtime facts.

**Consequence.** The first slice pins descriptors and content hashes. Tools run
in-process (ADR-0007); remote/out-of-process execution is deferred to a future
ADR. Permission policy stays mutable and separate from replayable descriptors.

---

## D7 - Goal Evaluation Is An Extension

**Problem.** Putting outcome semantics into the runtime core would encode one
product's model of "done" into every run.

**Decision.** The runtime provides neutral continuation mechanisms: async
`ContinuationGuard`, structured opaque verdicts, thread-scoped state effects, and
`TerminationReason::Concluded`. `awaken-ext-goal` owns `GoalSpec`, grading, and
goal classifications. Product protocols map onto that extension.

**Consequence.** The kernel records and replays verdicts but does not interpret
goal semantics. Anthropic Outcome mapping belongs in a product adapter.

---

## D8 - Anti-Corruption Layers Own Public Protocol Names

**Problem.** Public DTO names leaking into runtime code make the core hard to
reuse and hard to test independently.

**Decision.** Protocol adapters translate public input into neutral runtime
commands and translate committed runtime facts back into public events.

**Consequence.** Managed Agents, A2A, ACP, AI SDK, AG-UI, and future protocols can
share the runtime without contaminating it with protocol vocabulary.

---

## D9 - Selection, Capability, And Health Are Never Authorization

**Problem.** Treating pool membership, capability compatibility, or a successful
health probe as access control creates hidden privilege paths.

**Decision.** Selection results, capability profiles, and availability checks
carry no grant. Authorization is decided only by the permission policy path.

**Consequence.** Credential selection, model/provider
compatibility, and detection checks remain operational decisions, not security
decisions.

---

## D10 - A Design Must Name The First Vertical Slice

**Problem.** Broad architecture can be correct but still fail to guide
implementation.

**Decision.** Every design change must name the bounded context, model element,
port/repository, owning crate/project, guardrail, test/enforcer, and first
vertical slice.

**Consequence.** Speculative crate lists, future deployment topology, and
multi-feature abstractions are not implementation guidance until reduced to an
executable slice.

---

## D11 - Runtime Behavior Uses State, Effects, And Extension Seams

**Problem.** Features such as state machines, reminders, context compaction,
observability, and eval can easily grow side stores or special run modes.

**Decision.** Runtime behavior changes enter through typed state/effects,
registered plugins/hooks, explicit runtime commands, or extension ports. Durable
results still commit through the runtime write boundary; analytics and UI views
remain projections.

**Consequence.** A new runtime feature must show its state key or effect type,
hook/port, commit path, replay behavior, and first executable slice before it
guides implementation.

---

## D12 - Contract Names Follow Authority

**Problem.** Broad names such as "runtime contract" or "dispatch contract" can
hide different authorities: agent-domain truth, durable run ingress, and
protocol projection. Once those are mixed, readers cannot tell where `RunRecord`,
`CommitCoordinator`, event replay, dispatch claims, or protocol rows belong.

**Decision.** Use three naming layers:

| Layer | Rule | Example |
|---|---|---|
| Crate | express packaging layer and dependency direction | `awaken-agent-contract`, `awaken-runtime`, `awaken-run-ingress-contract`, `awaken-protocol-replay-contract` |
| Module | express domain context | `agent::stream`, `agent::event`, `agent::fact` |
| Type | keep the industry short name inside its module | `Event`, `Record`, `Message`, `ToolCall` |

Do not solve ambiguity by growing long prefixed type names. Use paths for
precision and export clear aliases at public boundaries:

```text
agent::stream::Event       -> StreamEvent
agent::event::Record       -> EventRecord
agent::fact::run           -> run-fact builders/readers
```

Crate boundaries:

| Crate | Responsibility | Must not contain |
|---|---|---|
| `awaken-agent-contract` | Agent-domain contract: message, thread, run, tool call, approval, stream, event, fact, state, and commit ports | server routes, HTTP, store implementations, protocol DTOs |
| `awaken-runtime-contract` | Internal runtime-facing contract: snapshot execution, snapshot lookup/listing, runtime capability, plugin config validation, activation, and execution entrypoint ports | config CRUD, admin workflow, public protocol DTOs, durable ingress internals, runtime loop implementation details |
| `awaken-runtime` | agent kernel: execution loop, tool pipeline, plugin hooks, resolution, provider routing, and production of stream events, event drafts, and fact commit plans | durable dispatch, HTTP, protocol projection |
| `awaken-run-ingress-contract` | server run-delivery contract: dispatch records, claim/lease state, pending input, live-command delivery, and runtime-store facade | runtime loop implementation types, protocol replay rows, store backend implementations |
| `awaken-protocol-replay-contract` | protocol replay contract: public stream replay rows, cursors, redaction state, and replay-log ports | runtime truth, run-dispatch lifecycle, adapter-specific DTO logic |
| `awaken-config-contract` | config-domain contract: config store ports, registry graph, model/agent definitions, published config, registry/audit records, and materialized runtime input data | live runtime objects, runtime commit authority, protocol projection, admin assistant tool execution |
| `awaken-admin-contract` | optional admin-only route DTOs or reusable admin API values, if they become stable enough to share | general config publication, registry graph, materialized runtime input data |
| `awaken-run-ingress` | durable run host: input buffer, dispatch coordinator, recovery, event staging, and live control | protocol encoders, store backends, runtime-domain event definitions |
| `awaken-stores` | event/fact log, run-dispatch store, protocol replay store, config/audit backend implementations | runtime normalization, protocol DTO mapping |
| `awaken-protocol-*` | protocol DTOs/encoders such as A2A, AG-UI, AI SDK; split only when stable and reused | agent kernel logic |
| `awaken-ext-*` | plugins/extensions such as permission, MCP, goal, and skills | core runtime ownership |

The contract crate internal modules follow the agent-domain layout:

```text
agent/
  message.rs
  thread.rs
  run.rs
  turn.rs
  step.rs
  tool_call.rs
  approval.rs
  handoff.rs
  artifact.rs
  state.rs

stream/
  event.rs      # Event: live stream event
  sink.rs       # Sink: live stream sink

event/
  draft.rs      # Draft: pre-commit candidate
  record.rs     # Record: committed neutral event record
  kind.rs
  envelope.rs

fact/
  run.rs        # run fact builders/readers; payload is a run projection value
  thread.rs     # thread commit visibility facts
  message.rs
  state.rs

commit/
  coordinator.rs
  staged.rs
  boundary.rs

store/
  event_log.rs
  fact_log.rs
  run_store.rs
```

Durable-ingress modules use their own context:

```text
run_ingress::{direct, durable, lifecycle}
durable_host::{
  input_buffer,
  dispatch_coordinator,
  recovery_replay,
  event_capture,
  staging_coordinator,
  live_control,
  commit_boundary,
}
```

The intended dependency direction is:

```text
awaken-agent-contract
  <- awaken-runtime-contract
  <- awaken-runtime
  <- awaken-run-ingress-contract
  <- awaken-stores

awaken-runtime-contract + awaken-runtime + awaken-run-ingress-contract
  <- run-ingress implementation

awaken-runtime + awaken-agent-contract
  <- awaken-ext-*

awaken-agent-contract + awaken-protocol-replay-contract
  <- protocol adapters and replay projections
```

The hard rules are:

- runtime must not depend on server or protocol crates;
- protocol names must not enter the runtime crate;
- stores must not perform event normalization;
- run ingress coordinates durable delivery but does not define agent-domain
  events;
- protocol replay projects only from committed events or facts, never from live
  stream output.

**Consequence.** Crates stay coarse-grained and follow dependency direction.
Modules carry fine-grained domain context. Type names stay short and familiar.
`fact::*` remains the recovery source of truth; `stream::*` is live delivery;
`event::*` is durable neutral event vocabulary. Protocol projection stays outside
runtime and follows the same short-name-plus-path rule when it becomes part of an
implementation slice. Tests can enforce each layer instead of relying on name
length or prose convention.

**Rejected.**

- Keep one terminal "dispatch contract": rejected because dispatch can mean
  ingress, store authority, protocol replay, or server projection.
- Create `awaken-events` or similar noun crates: rejected because they become
  context-free dumping grounds.
- Move agent event definitions into run ingress: rejected because event/fact
  truth is agent-domain truth.
- Merge tool execution and run dispatch: rejected because tool execution is
  per-tool-call runtime work, while run dispatch is per-run durable delivery and
  lease ownership.
- Add standalone capability or resilience contracts now: rejected until multiple
  implemented slices need shared externally testable behavior.

---

## D13 - Runtime Protocol/Specification Uses Apache-2.0

**Problem.** If the runtime protocol/specification, SDK-facing schemas, examples,
or conformance tests inherit a restricted product/admin/config license, third
parties cannot confidently implement or embed the runtime boundary.

**Decision.** The `awaken-runtime` protocol/specification and conformance
surface use Apache-2.0. Code packages may carry their own package or file license
metadata. Adjacent config, admin, hosted-product, or hosted service
code may use separate licensing, but those layers consume the runtime protocol
and cannot redefine it under restricted terms.

**Consequence.** The runtime remains a neutral integration point. Commercial or
source-available product layers can exist above it without blocking independent
runtime implementations, SDKs, examples, or conformance tests.

---

## D14 - Concrete Tool IDs Live Outside Runtime Core

**Problem.** Once the runtime core ships concrete model-callable tool ids such as
shell, filesystem, task, or delegation tools, the core silently starts owning
product policy, environment assumptions, and tool authorization defaults.

**Decision.** The runtime core provides the typed `Tool` trait, the low-level
`RawTool` adapter trait, descriptors, registries, resolver pipeline, permission
seams, execution ports, and commit behavior. It does not provide any concrete
model-callable tool implementation or tool id outside tests. First-party tools
that ship with the distribution live in `awaken-ext-builtin-tools`, an official
extension package with independently enabled toolsets:

| Toolset | Example ids | Ownership rule |
|---|---|---|
| `builtin-hand-tools` | `bash`, `read`, `write`, `edit`, `glob`, `grep`, `web_fetch`, `web_search` | registers descriptors and concrete tools that execute in-process within the extension |
| `builtin-task-tools` | `send_message`, `cancel_task`, `recover_failed_messages` | registers task orchestration tools over runtime state/effect seams; recovery tools are ops-scoped unless explicitly enabled |
| `builtin-delegation-tools` | `agent_run` | registers one delegation tool; target agent is an argument, not a generated tool id |

The delegation tool is deliberately unified. The runtime must not generate
`agent_run_<agent_id>` descriptors. A resolved run may specialize the single
`agent_run` descriptor with an allowed target-agent list or metadata, and the
tool must fail closed when the `agent_id` argument is not in the resolved
delegate/multiagent roster.

**Consequence.** Catalog policy, permission rules, and audit logs target stable
tool ids such as `agent_run`, not a growing set of generated names. Runtime core
stays tool-agnostic, while official distributions can still provide an
out-of-the-box tool bundle by installing `awaken-ext-builtin-tools`.

---

## D15 - Admin Assistant Tools Are Server-Owned

**Problem.** Admin-assistant tools look like ordinary `Tool` implementations,
but they depend on admin auth, config publication, audit logs, schema validation,
capability snapshots, and operator workflow. If they enter `builtin-tools`, they
become assignable runtime capabilities instead of admin-only control actions.

**Decision.** Admin-assistant tools live in the server/admin-owned package
`awaken-admin-assistant-tools`. That package may implement the runtime `Tool`
trait and build a private `ToolRegistry`, but it is not a runtime extension
package and is not part of normal agent catalog publication.

Examples include:

| Tool id | Owner | Visibility rule |
|---|---|---|
| `admin_get_platform_capabilities` | Admin / Server | bound only to the admin assistant route |
| `admin_create_agent_draft` | Admin / Server | creates an unpublished draft value only |
| `admin_validate_agent` | Admin / Server | runs server-side validation without granting publish authority |

Admin tools are route- or service-bound by admin auth. They are hidden from
ordinary `capabilities.tools`, excluded from normal `AgentSpec.allowed_tools`,
and audited as admin/control operations. A publish or destructive admin action is a
separate tool only after an explicit product/security decision.

**Consequence.** Runtime agents can use official builtin tools without gaining
admin authority. Admin assistant behavior remains replaceable and testable as a
server feature, while the runtime core and `awaken-ext-builtin-tools` stay
product-neutral.

`awaken-config-contract` remains the broader config-domain contract for
config, registry graph, and published runtime input data. `awaken-admin-contract`
is only appropriate for narrow admin-only API values if those values become
stable and reusable. Admin assistant tools belong to
`awaken-admin-assistant-tools`, not runtime core or ordinary builtin tools.

---

## D16 - Neutral Code Avoids Product Hosting Vocabulary

**Problem.** Words such as `managed` describe a hosted product or compatibility
adapter, not the neutral runtime protocol. If they appear in runtime, protocol,
config, or ordinary extension code names, the core starts looking like one
product's API instead of a reusable substrate.

**Decision.** Neutral runtime/protocol/config code must not use product hosting
vocabulary such as `managed` in crate names, module names, type names, tool ids,
or public protocol fields. Such names are allowed only in product adapter code or
boundary documentation that explicitly maps an external product protocol into
neutral runtime values.

Use neutral names instead:

| Product-shaped term | Neutral replacement |
|---|---|
| managed config contract | `awaken-config-contract` |
| managed/admin tool bundle | `awaken-admin-assistant-tools` when admin-only, `awaken-ext-builtin-tools` when ordinary runtime builtin |
| managed run/session field | neutral run/thread/activation/projection field |
| managed/product protocol event | adapter-owned public event projected from committed runtime facts |

**Consequence.** Protocol neutrality is reviewable mechanically. Grep/deny-list
checks can fail runtime, protocol, config, and ordinary extension crates when a
product-hosting token leaks in. Product adapters may still expose compatibility
names at the edge.

---

## D17 - Custom Tool Use Is Runtime Wait/Resume, Not Management

**Problem.** Managed Agents custom tool use can be mistaken for a management
surface because clients can name tools and send results. If that channel is
allowed to mutate agent definitions or global catalogs, a runtime compatibility
feature becomes a hidden config/admin API.

**Decision.** Custom/client-executed tool use is modeled as a resolved runtime
tool call that suspends on an external result. Product adapters may project the
pending call to their public custom-tool event and may map the later public result
back to a neutral resume/result command. Tool descriptors may be admitted only as
per-run runtime input when the adapter explicitly allows client-executed tools;
they are not config records and are not published into the global catalog.

The runtime owns the pending call id, authorization state, deadline, descriptor
fingerprint, and resume validation. Product adapters own public event names,
public correlation fields, and public result encoding. Config/admin APIs remain
separate and are not implied by custom tool support.

**Consequence.** The public compatibility surface can be named
`managed-agents-runtime-protocol` without supporting Managed Agents management
endpoints. Unknown, duplicate, expired, thread-mismatched, or
descriptor-mismatched results fail closed. Runtime core does not learn product
event names, and config stays authoritative for published agents and global tool
catalogs.

AG-UI remains a separate public protocol adapter. It may reuse neutral runtime
ports and wait/resume mechanics, but it does not share Managed
Agents DTOs, public event names, beta headers, or conformance fixtures.

---

## D18 - Runtime Axes Stay Separate

**Problem.** A broad runtime controller that edits config, steers active runs,
executes loops, writes durable truth, and projects public protocol events makes
authority hard to test and turns every adapter into a privileged integration
point.

**Decision.** Treat configuration publication, live control, and execution as
separate primary runtime-facing axes:

| Axis | Authority | Boundary |
|---|---|---|
| Configuration publication | publish and version behavior data | Config Domain -> `RegistryPublication` -> `RuntimeCatalogInstaller` -> `RunResolver` |
| Live control | steer one active run at safe boundaries | `RunIngress` / `LiveRunControl` / `RuntimeInputHandle` |
| Execution | run the resolved loop and stage runtime truth | `RunExecutor`, backend/model/tool ports, `CommitCoordinator` |

Activation, resolution, state, event, wait/resume, commit, and
extension are supporting axes. They may connect the primary axes, but they must
not merge their authority. In particular, live control cannot write config,
configuration publication cannot steer an active run, and execution cannot emit
public protocol truth before commit.

**Consequence.** New runtime-facing APIs must identify which axis they belong to
and which port carries them. If an API needs authority from multiple axes, it
must be split or justified as an explicit boundary change with tests.

---

## D19 - Executable Snapshot Is The Run Configuration Identity

**Problem.** Treating `AgentId` as the full identity of a runnable configuration
assumes an agent has one stable definition. In practice the same agent id may be
resolved differently for different runs or threads because instructions, tools,
plugins, model bindings, capability requirements, and publication versions can
change.

**Decision.** Runtime-facing execution uses an explicit executable snapshot. A
caller may provide `ExecutableAgentSnapshot` inline or provide an
`ExecutableAgentSnapshotId` that is resolved through `AgentSnapshotResolver`.
Configuration surfaces may use `AgentSnapshotCatalog`, `RuntimeCapabilitySource`, and
`PluginManifest` (with the single `validate_section`) to list executable
snapshots, inspect runtime capabilities, and validate plugin config sections.

These ports are internal runtime-facing contracts. They are not config CRUD,
publication workflow, admin management APIs, or public protocol DTOs.

**Consequence.** Runtime and config/admin surfaces stay decoupled. The runtime
can execute directly from immutable snapshot data, or ask a resolver for that
data by id, without knowing where the snapshot was stored or which config/admin
workflow created it. `AgentId` remains a domain identifier, but
`ExecutableAgentSnapshot` is the complete run/thread configuration identity.

> **Amendment (2026-06-30, ADR-0032).** The snapshot and its catalog install are
> bundled as one run input, `RunnableConfig` (built directly or by `compile()`);
> `Runtime::run` installs and executes it in one call. The snapshot stays the
> configuration identity — `RunnableConfig` carries it with the catalog it was
> built against. See
> [config-to-run-execution-flow.md](config-to-run-execution-flow.md#implemented-run-input-runnableconfig-adr-0032).

---

## D20 - Publication Coordination Is Outside Runtime

**Problem.** A single service that loads config, compiles registries, installs
runtime catalogs, steers active runs, and executes loops becomes a hidden
runtime controller. It also makes config/admin code appear to own runtime
behavior.

**Decision.** `ConfigPublicationCoordinator` and `RegistryCompiler` are
config-side roles. They may live in a server/config package, but not in runtime
core. The coordinator orders a publish transaction. The compiler validates a
`ConfigSnapshot` and produces a complete `RegistryPublication` / install
candidate. Runtime exposes only `RuntimeCatalogInstaller` for the install
handoff, plus the snapshot execution and inspection ports used after install.

**Consequence.** Runtime remains a consumer of complete publications. It can
reject an install, resolve executable snapshots, execute runs, and commit facts,
but it cannot load config records, publish versions, or own admin workflow. No
single runtime-facing role may combine config loading, registry compilation,
catalog install, live control, and execution authority.

---

## D21 - Activation Data And Runtime Context Are Separate

**Problem.** A convenient run request can easily mix immutable input, executable
configuration, process-local channels, commit wiring, stream sinks, and resolver
pins. Once mixed, durable dispatch, replay, and tests cannot tell which values are
serializable facts and which values are live host wiring.

**Decision.** Use three names for three authorities:

| Name | Authority |
|---|---|
| `ExecutableAgentSnapshot` | immutable executable configuration for one run/thread scope |
| `RunActivation` | immutable neutral run input: snapshot input, intent, messages or input reference, options, overrides, trace identity |
| `RuntimeRunContext` | per-attempt live wiring: cancellation, input receiver, stream sink, commit source, thread context cache, pinned resolver scope, persistence mode, optional pre-resolved plan |

`RunActivation` and `ExecutableAgentSnapshot` may be serialized or reconstructed.
`RuntimeRunContext` is process-local and recreated by direct ingress, durable
ingress, tests, or a runtime host.

**Consequence.** Durable requests stay data-only. Live handles do not leak into
snapshot identity or activation data. A host may implement one internal struct for
ergonomics, but public/runtime-facing contracts must expose the split so tests can
enforce replayability, boundary direction, and same-source commit wiring.

---

## D22 - Config Graph Is Explicit Before Runtime

**Problem.** Model-provider, model, model-pool, and agent configuration can become
ambiguous when an agent points at "a model" and runtime later discovers provider,
fallback, credential, backend, and capability choices during execution. That
hides selection policy in the execution path and makes replay hard to explain.

**Decision.** Treat config as an explicit graph before runtime execution:

```text
ModelProviderSpec -> ModelSpec -> ModelBinding
AgentSpec -> ModelSpec or ModelPoolSpec -> selected ModelBinding
AgentSpec -> ToolSpec / SkillSpec / plugin refs
```

`ModelProviderSpec` describes a configured model-access provider instance.
`ModelSpec` describes a configured model and its capability metadata.
`ModelPoolSpec` describes explicit selection, routing, fallback, weighting, and
downgrade policy. `AgentSpec` assembles behavior by reference.
`RegistryCompiler` validates and freezes this graph into a publication;
`ExecutableAgentSnapshot` carries the resolved graph identity for one run/thread
scope.

**Consequence.** Runtime validates selected model-provider/model/backend bindings
and capability requirements, but it does not search for fallback model providers
or mutate the config graph during execution. If fallback is allowed, its
candidates and downgrade rules are visible before activation and recorded in
resolved data or publication metadata sufficient for replay and debugging.

Do not broaden these specs to make room for a future integration. A runtime
driver, installed command, cloud platform, gateway endpoint, discovery record,
probe result is not a `ModelProviderSpec` unless it is
literally the configured target for model calls. It is not an `AgentSpec` field
unless it is agent behavior or a requirement expressed by reference. Future
integrations feed publication inputs, capability evidence, backend profiles, tool
descriptors, or snapshot data through their own tested boundary when that
boundary exists.
