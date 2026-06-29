# Awaken Runtime Design Corpus

This corpus configures the current repository as the design and guardrail home
for `awaken-runtime`. It names the runtime-owned domain model, the ports around
that model, every runtime-related requirement inherited from the reference
corpus, and the checks a change must satisfy before code is written.

The runtime target is deliberately narrow:

- **Open runtime core**: agent execution, run lifecycle, tool abstractions,
  typed state/effects, events, commit coordination, extension hooks, and
  local/direct runtime use. Runtime protocol/specification and conformance
  material use Apache-2.0.
- **Runtime boundary contracts**: the neutral ports consumed by server,
  distributed ingress, config, resource, credential, and product
  adapters, including the internal snapshot execution and runtime capability
  surface ports.
- **Out-of-runtime concerns**: distributed run ingress, config publication,
  hosted product
  semantics, public protocols, vaults, resource data planes, and execution
  placement are documented only as boundaries so they do not leak into the core.

The design must cover all runtime behavior directly and enough adjacent
server/product/environment behavior to make the runtime boundary enforceable.

## Bounded Contexts

The corpus is organized into five bounded contexts:

- **Runtime Core** — the domain center: agent execution, run lifecycle, commit
  boundary, typed state/effects, tool abstractions, and extension hooks.
- **Dispatch / Server** — run ingress, durable delivery, protocol replay, and
  config-publication coordination above the runtime.
- **Neutral Platform** — reusable connection and control mechanisms, free of
  product DTOs or hosted policy.
- **Config / Admin / Product** — config authoring, registry compilation, admin
  APIs, tenant policy, vaults, and product mappings.

The Runtime Core names neither product nor server internals; every other context
adapts into it through explicit ports and never the reverse. The full context map
— what each context owns and must not own — is owned by
[design/architecture-overview.md](design/architecture-overview.md#1-context-map),
and the one-way package dependency direction it implies is enforced by
[design/packaging-enforcement-matrix.md](design/packaging-enforcement-matrix.md#import-rules).

## License Boundary

The `awaken-runtime` protocol/specification, SDK-facing schemas, examples, and
conformance tests are Apache-2.0 unless a file says otherwise. Code crates may
use their own package or file license metadata, but adjacent distributed
config, admin, or hosted-product packages must not redefine the runtime protocol
or require non-Apache terms for implementing it.

## How To Use These Docs

1. Read [design/architecture-overview.md](design/architecture-overview.md) for the
   context map and DDD vocabulary.
2. Read [design/config-to-run-execution-flow.md](design/config-to-run-execution-flow.md)
   for the end-to-end path from configuration publication and catalog install to
   activation, resolution, execution, and commit.
3. Read [design/key-design-decisions.md](design/key-design-decisions.md) before
   adding a new subsystem or public seam.
4. Check [INVARIANTS.md](INVARIANTS.md) for the mechanical rule that must be
   enforced by tests or dependency checks.
5. Check [requirements-coverage.md](requirements-coverage.md) to ensure the
   change fits the runtime-owned coverage target or is explicitly marked as a
   boundary concern outside the runtime package.
6. Use the theme docs only for the area you are changing.
7. Update [STATUS.md](STATUS.md) when a design slice becomes implemented or moves
   out of scope.

## Decision Records (ADRs)

Load-bearing decisions are recorded as numbered ADRs under
[adr/](adr/), following a decision-first model: one decision per ADR,
append-mostly, with explicit supersede/amend chains. Theme docs
below are either owners or navigation. Owner docs may maintain the role catalog
or pre-implementation state machine for the roles they own; navigation, status,
coverage, and wiki documents must link to the canonical ADR, owner doc, or code
type rather than duplicate schemas, state machines, or role catalogs.

- [adr/0001-documentation-model-and-vocabulary-alignment.md](adr/0001-documentation-model-and-vocabulary-alignment.md)
  — documentation model, guardrail-enforcer rule, one internally consistent
  vocabulary, and the link-only/meta-process treatment of the wiki/coverage layers.
- [adr/0002-resolver-role-demarcation.md](adr/0002-resolver-role-demarcation.md)
  — the three canonical resolver roles (`AgentResolver` / `Resolver` /
  `RunResolver`), their boundaries, and an open renaming question.
- [adr/0003-deferred-work-mechanism-selection.md](adr/0003-deferred-work-mechanism-selection.md)
  — which deferred-work mechanism to use (`ScheduledAction`, waiting ticket, or
  `RunDispatch`); why no `BackgroundTask` umbrella.
- [adr/0004-plugin-factory-contributions-and-capability-bound.md](adr/0004-plugin-factory-contributions-and-capability-bound.md)
  — the `Plugin` factory, config-aware `resolve` to `Contributions`, the
  `ResolvedExecutionEnv` aggregate, and `CapabilityBound` as a fail-closed
  contribution ceiling that is never authorization.
- [adr/0005-run-terminal-state-single-authority.md](adr/0005-run-terminal-state-single-authority.md)
  — the committed run `Phase` (`Waiting | Ended(EndCause)`) as the one stored
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
| `config-to-run-execution-flow.md` | End-to-end configuration, publication coordination, registry compilation, runtime catalog install, executable snapshot selection, activation, resolution, execution, and commit flow |
| `config-publication-lifecycle.md` | Config snapshot, publication, atomic install, rollback, and runtime catalog swap lifecycle |
| `protocol-adapter-boundaries.md` | Public protocol adapters, conformance, replay, error mapping, and unsupported management APIs |
| `permission-policy-axis.md` | Permission decisions, HITL tickets, authorization, and audit staging |
| `model-provider-backend-binding.md` | Model/provider/backend binding validation and capability reconciliation |
| `commit-fact-projection-taxonomy.md` | Live streams, commits, facts, protocol replay, public projection, and dataset sinks |
| `builtin-tools-extension-contract.md` | `awaken-ext-builtin-tools`, hand tools, task tools, and unified delegation tool contracts |
| `runtime-behavior.md` | Run lifecycle, state/effects, plugins, cancellation, scheduled work, observability/eval |
| `runtime-scenario-validation.md` | GWT scenario ids, scenario text, executable-test mapping, and scenario test organization |
| `runtime-interface-boundaries.md` | Runtime role traits, executable snapshot contract, plugin contribution matrix, tool decision ladder, simple-design evaluation |
| `neutral-waist.md` | Runtime ports, backend execution, event streaming, goal continuation hooks |
| `tool-and-capability.md` | Tools, capability checks, permissions, pinned descriptors |
| `run-ingress-message-delivery.md` | Boundary guidance for run ingress, durable dispatch, pending input, and message delivery |
| `anthropic-alignment-and-sessions.md` | Boundary guidance for downstream protocol/product adapters and anti-corruption mapping |
| `credentials-and-vaults.md` | Boundary guidance for product-owned credential lifecycle and authorization boundaries |
| `resources-memory-files-skills.md` | Boundary guidance for resource data plane, skills, and out-of-process execution |
| `observability-eval-dataset-boundary.md` | Trace, dataset, eval, and analytics boundaries |
| `error-taxonomy.md` | Neutral error ownership and public adapter error mapping |
| `packaging-enforcement-matrix.md` | Package/import/license/vocabulary enforcement matrix |
| `requirements-coverage.md` | Whole runtime coverage and out-of-runtime boundary coverage |

The docs favor DDD and simple design: model current domain language, reuse existing
ports, add the smallest coherent vertical slice, and keep product semantics out of
the core.
