# Awaken Agents documentation

Choose the path for what you want to build. You can run Agents and connect an
application without first learning the Runtime's internal architecture.

| Your task | Start here | First result |
|---|---|---|
| Build an assistant or Agent-backed application | [Run Awaken locally](../README.md#try-awaken), then [connect your application](../README.md#build-with-awaken) | One published Agent, its Session and a result you can inspect |
| Embed execution inside a Rust application | [Runtime SDK](../crates/sdk/runtime/awaken-runtime/README.md) and [independent consumer](../examples/sdk-consumer/README.md) | A runnable application using public SDK imports |
| Operate a deployment | [Deployment guide](../deploy/README.md) | An explicit configuration, data location and execution boundary |
| Change Awaken itself | [Contributing](../CONTRIBUTING.md), then the design reading path below | The owning component, invariant and relevant checks |

## Architecture maps

[Architecture overview](design/architecture-overview.md#1-context-map) explains
process composition and application ownership. [ADR-0088](adr/0088-bounded-context-map-and-sdk-platform-contract.md#decision)
and crate metadata own domain context membership. These are different views:
Control, Coordinator and Worker are deployment roles, not an alternative domain
context catalog. Runtime is the execution core embedded by Worker or an SDK
consumer, not another service authority.

The remaining pages are the engineering design and guardrail corpus. Public
protocols, resource content, credentials and deployment stay outside the neutral
Runtime core; follow their owning theme documents rather than putting those
concerns into an embedded application.

## License Boundary

The `awaken-runtime` protocol/specification, SDK-facing schemas, examples, and
conformance tests are Apache-2.0 unless a file says otherwise. Code crates may
use their own package or file license metadata, but adjacent distributed
config, admin, or hosted-product packages must not redefine the runtime protocol
or require non-Apache terms for implementing it.

## How To Use These Docs

This reading path is for contributors changing implementation or architecture.
Application developers should use the task paths above first.

Documentation ownership and code/website synchronization follow the
[developer-entry amendment](adr/0001-documentation-model-and-vocabulary-alignment.md#amendment-2026-11-09-developer-entry-and-downstream-publication).
Run `scripts/ci/check-docs.sh` for the existing prose/link checks; that result is
separate from executable-example, product-runtime and customer acceptance.

1. Read [design/architecture-overview.md](design/architecture-overview.md) for the
   context map and DDD vocabulary.
2. Read [design/config-to-run-execution-flow.md](design/config-to-run-execution-flow.md)
   for the canonical configuration-to-application and request-to-committed-response
   paths.
3. Read [design/key-design-decisions.md](design/key-design-decisions.md) before
   adding a new subsystem or public seam.
4. Check [INVARIANTS.md](INVARIANTS.md) for the mechanical rule that must be
   enforced by tests or dependency checks.
5. Check [requirements-coverage.md](requirements-coverage.md) to ensure the
   change fits the runtime-owned coverage target or is explicitly marked as a
   boundary concern outside the runtime package.
6. Run the executable Kani proofs and the bounded TLA+ model documented in
   [../formal/README.md](../formal/README.md) when changing runtime/run-ingress
   lifecycle rules.
7. Use the theme docs only for the area you are changing.
   Use [design/test-validation.md](design/test-validation.md) for strict test
   design, execution tiers, native OS validation and coverage interpretation.
8. Update [STATUS.md](STATUS.md) when a design slice becomes implemented or moves
   out of scope.

## Decision Records (ADRs)

Load-bearing decisions are recorded as numbered ADRs under
[adr/](adr/), following a decision-first model: one decision per ADR,
append-mostly, with explicit supersede/amend chains. Theme docs
below are either owners or navigation. Owner docs may maintain the role catalog
or pre-implementation state machine for the roles they own; navigation, status,
coverage, and wiki documents must link to the canonical ADR, owner doc, or code
type rather than duplicate schemas, state machines, or role catalogs.

The complete ADR list, with each record's current standing, is the `adr/` part
of the machine-checked [Role Catalog Coverage](STATUS.md#role-catalog-coverage)
table; numbers are not reused, so the absent 0096 was never assigned. The
foundational records are:

- [adr/0001-documentation-model-and-vocabulary-alignment.md](adr/0001-documentation-model-and-vocabulary-alignment.md)
  — documentation model, guardrail-enforcer rule, one internally consistent
  vocabulary, and the link-only/meta-process treatment of the wiki/coverage layers.
- [adr/0002-resolver-role-demarcation.md](adr/0002-resolver-role-demarcation.md)
  — the two canonical lookup/materialization roles (`AgentSnapshotResolver` and
  `RunResolver`) and their boundaries.
- [adr/0003-deferred-work-mechanism-selection.md](adr/0003-deferred-work-mechanism-selection.md)
  — which deferred-work mechanism to use; why there is no generic
  BackgroundTask umbrella, with the narrow detached-tool amendment in ADR-0076.
- [adr/0004-plugin-factory-contributions-and-capability-bound.md](adr/0004-plugin-factory-contributions-and-capability-bound.md)
  — the `Plugin` factory, config-aware `resolve` to `Contributions`, the
  `ResolvedExecutionEnv` aggregate, and `CapabilityBound` as a fail-closed
  contribution ceiling that is never authorization.
- [adr/0005-run-terminal-state-single-authority.md](adr/0005-run-terminal-state-single-authority.md)
  — the committed run `RunState` (`Awaiting | Ended(EndCause)`) as the one stored
  terminal authority, with status/outcome/error derived, never stored.
- [adr/0006-fact-authority-run-record-is-cache.md](adr/0006-fact-authority-run-record-is-cache.md)
  — the committed fact log as the run's read authority and the `RunRecord` as a
  derived cache that equals the latest fact; the fence is the committed count.

## Development-Ready Design Rule

A design is ready to guide implementation only when it names:

| Required item | Why it matters |
|---|---|
| Bounded context | Prevents product/server/runtime vocabulary leaks |
| Aggregate/entity/value object | Keeps domain behavior close to the model |
| Port or repository trait | Makes dependencies explicit and testable |
| Owning crate/project | Prevents speculative package sprawl |
| Guardrail and enforcer | Turns architecture into a reviewable check |
| First vertical slice | Keeps implementation simple and shippable |

If any row is missing, the document is still exploratory and should not drive a
large implementation.

## Role Catalog And State Machine Rule

A design document must include a role or component catalog when it owns stable
architecture roles: cross-crate ports, durable components, permission gates,
plugin seams, recovery services, or any type that carries execution, commit,
resolution, authorization, or persistence authority.

Do not add a catalog to every document. Overview, status, coverage, and product
mapping docs may link to the owning catalog instead. Ordinary helpers, parsers,
builders, caches, and private data structures stay in Rustdoc or code comments.

Use this shape for catalog rows:

| Column | Meaning |
|---|---|
| Name | Stable role, trait, struct, or target implementation name |
| Kind | Boundary port, value object, internal component, policy, hook, service |
| Owns | The authority or fact this role owns |
| Uses | Roles or data it depends on |
| Must Not Own | Responsibilities that would blur the boundary |
| Failure Mode | The main failure it prevents or surfaces |
| Guardrail/Test | The invariant, public API check, dependency check, or behavior test |

Maintain the catalog when a stable role is added, renamed, split, or given new
authority. API-level details still belong in Rustdoc; the catalog records why the
role exists and where its boundary stops.

State machines are maintained by the owning code type/Rustdoc once implemented.
Before implementation, the owning design doc may carry the minimal lifecycle:
states, transitions, visibility or commit point, failure/recovery behavior, and
tests. Navigation, status, coverage, and wiki documents may only link to that
owner.

## Document Classes

Documents without stable architecture roles are meta, navigational, reflective,
or downstream-mapping documents. They can be implementation-ready without owning a
catalog because they do not introduce execution, commit, recovery, permission, or
persistence authority.

| Class | Owns | Catalog policy |
|---|---|---|
| Meta / introspection | corpus rules, readiness status, guardrail registry, ownership map, change log | no catalog; checked by status/ownership/wiki hooks |
| Navigation / context map | where concepts live and how bounded contexts relate | no catalog or delegated catalog link |
| Decision record | why a choice was made and which alternatives were rejected | no catalog unless it introduces a stable role |
| Coverage map | requirement-to-context coverage across packaging targets | no catalog |
| Role owner | stable roles, ports, policies, durable components, plugin seams | catalog required |
| Delegated boundary narrative | explains a boundary whose roles are owned by another doc | link to owning catalog |
| Product or downstream mapping | adapter/product guidance outside runtime core | no catalog unless it owns stable product roles |

The source of truth for this classification is
[STATUS.md](STATUS.md#role-catalog-coverage). The CI hook
`check-role-catalogs` fails if a required catalog is missing, a delegated catalog
link is broken, or a non-catalog document starts carrying a catalog without first
updating its classification. The hook discovers source documents from top-level
`docs/*.md`, `docs/design/*.md`, and future `docs/adr/0*.md` files, so adding a
new source document automatically requires a classification row. Wiki documents
are checked separately by the OKF/wiki hooks.

## Theme Docs

| Theme doc | Use for |
|---|---|
| `config-to-run-execution-flow.md` | Control Agent/Environment publication, database ownership, Coordinator Deployment/Session creation, dispatch, materialization, execution, commit, and response |
| `config-publication-lifecycle.md` | Control publication and Coordinator executable-registration identity, idempotency, recovery, and failures |
| `protocol-adapter-boundaries.md` | Public protocol adapters, conformance, replay, error mapping, and unsupported management APIs |
| `permission-policy-axis.md` | Permission decisions, HITL tickets, authorization, and audit staging |
| `model-provider-backend-binding.md` | Model/provider/backend binding validation and capability reconciliation |
| `commit-fact-projection-taxonomy.md` | Live streams, commits, facts, protocol replay, public projection, and dataset sinks |
| `builtin-tools-extension-contract.md` | `awaken-ext-builtin-tools`, Hand tools, native delegation, and Managed coordination contracts |
| `runtime-behavior.md` | Run lifecycle, state/effects, plugins, cancellation, scheduled work, observability/eval |
| `runtime-scenario-validation.md` | GWT scenario ids, scenario text, executable-test mapping, and scenario test organization |
| `runtime-interface-boundaries.md` | Runtime role traits, executable snapshot contract, plugin contribution matrix, tool decision ladder, simple-design evaluation |
| `neutral-waist.md` | Runtime ports, backend execution, event streaming, goal continuation hooks |
| `tool-and-capability.md` | Tools, capability checks, permissions, pinned descriptors |
| `run-ingress-message-delivery.md` | Boundary guidance for run ingress, durable dispatch, pending input, and message delivery |
| `remote-worker-protocol.md` | Recoverable remote Worker claim/recovery/commit protocol, public Worker assembly, phased production identity/feed/active-active work, and storage alternatives |
| `anthropic-alignment-and-sessions.md` | Boundary guidance for downstream protocol/product adapters and anti-corruption mapping |
| `managed-dream.md` | Managed Dream API and explicit cross-Session Dream: frozen JSONL evidence, read-only source snapshot, write-through result MemoryStore, ordinary auxiliary Agent, recovery, and compatibility tests |
| `managed-deployments.md` | Durable Managed Deployment and DeploymentRun scheduling, exact occurrence claims, Agent version freezing, lifecycle webhooks, and the shared periodic driver |
| `credentials-and-vaults.md` | Boundary guidance for product-owned credential lifecycle and authorization boundaries |
| `resources-memory-files-skills.md` | Resources application composition; File/Memory/Repository/Skill identities, binding, activation, explicit terminal Repository publication, recovery, and reclamation |
| `file-lifecycle.md` | Logical File creation, exact byte resolution, deletion replay, ownership references, and physical reclamation boundaries |
| `observability-eval-dataset-boundary.md` | Trace, dataset, eval, and analytics boundaries |
| `error-taxonomy.md` | Neutral error ownership and public adapter error mapping |
| `packaging-enforcement-matrix.md` | Package/import/license/vocabulary enforcement matrix |
| `requirements-coverage.md` | Whole runtime coverage and out-of-runtime boundary coverage |
