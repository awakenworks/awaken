# Design Status

This repository is configured as the `awaken-runtime` design corpus and
guardrail home. Code may be added here, but the status below is about whether a
document can guide runtime implementation and boundary checks.

Status values:

| Value | Meaning |
|---|---|
| Ready | Names context, model, port, owner, guardrail, enforcer, and first slice |
| Runtime-owned | Belongs to the runtime package or its public runtime contract |
| Boundary-only | Documents an adjacent system only to keep it out of runtime core |
| Product-owned | Belongs in a downstream product or server/protocol project, not runtime core |
| Retrieval-ready | Correct as a downstream index, but not a source of truth |
| Exploratory | Not ready to drive implementation |

## Documentation Model (ADR-0001)

Per [ADR-0001](adr/0001-documentation-model-and-vocabulary-alignment.md), this
repository is migrating to a decision-first model: ADRs plus a guardrail index
with concrete enforcers ([INVARIANTS.md](INVARIANTS.md)), with one internally
consistent vocabulary. This affects the documentation layers differently:

- **Retained — `docs/wiki/*` (LLM retrieval index).** The wiki is a distinct,
  downstream layer for locating the authoritative owner quickly — not a competing
  source of truth. It is kept, with tightened discipline: every fact is
  **link-only** (one sentence plus an owner link) and never copies schemas,
  guardrail ranges, state machines, or tables. The drift this caused (e.g. the
  stale `G1 through G16`) came from breaking that rule, not from the layer; the
  `check-wiki-no-invariant-copy` hook enforces it.
- **Trimmed — process/meta tables.** The readiness, document-class, and
  role-catalog-coverage tables below, and `requirements-coverage.md` as a
  standalone matrix, are process/status surface. They locate the owning document
  for a role catalog; they do not own role semantics or state machines. They are
  slated for simplification once their load-bearing facts fold into ADRs or the
  guardrail index, and are retained for navigation during migration only.

## Document Readiness

| Document | Status | Notes |
|---|---|---|
| `README.md` | Ready | Defines bounded contexts and development-ready design rule |
| `INVARIANTS.md` | Ready | Guardrails are mechanical and reviewable |
| `requirements-coverage.md` | Ready | Maps all required areas to bounded contexts and packaging rules |
| `design/architecture-overview.md` | Ready | Uses DDD context map and approved runtime/server seams |
| `design/config-to-run-execution-flow.md` | Ready | Defines the end-to-end handoff from explicit model-provider/model/model-pool/agent config graph through config-side publication coordination, registry compilation, runtime catalog install, executable snapshot selection, activation, resolution, execution, commit, and projection; keeps future integration evidence outside existing specs until a tested boundary exists |
| `design/config-publication-lifecycle.md` | Ready | Makes publication states, config-side compilation, atomic runtime install, rollback, and publication failure rules explicit |
| `design/protocol-adapter-boundaries.md` | Product-owned | Defines public protocol adapter mapping, conformance, and unsupported management boundaries |
| `design/permission-policy-axis.md` | Runtime-owned | Defines authorization flow, permission decisions, HITL tickets, and audit staging |
| `design/model-provider-backend-binding.md` | Runtime-owned | Defines model-provider/model/model-pool/agent spec graph, selected binding validation, fallback ownership, capability reconciliation, and narrow `ModelProviderSpec` / `AgentSpec` responsibilities |
| `design/commit-fact-projection-taxonomy.md` | Runtime-owned | Separates live stream output, committed truth, replay rows, and public projections |
| `design/builtin-tools-extension-contract.md` | Runtime-owned | Defines `awaken-ext-builtin-tools` package boundaries and toolset contracts |
| `design/key-design-decisions.md` | Ready | Defines runtime-owned implementation decisions, explicit config graph, concrete-tool packaging, admin-tool ownership, neutral naming, publication-role placement, and rejected leaks |
| `design/runtime-behavior.md` | Runtime-owned | Covers run lifecycle, activation/context split, live state apply versus durable commit, state/effects/events, extensions, cancellation, scheduling, eval |
| `design/runtime-scenario-validation.md` | Ready | Owns runtime GWT scenario ids, scenario text, executable-test mapping, and scenario test organization |
| `design/runtime-interface-boundaries.md` | Runtime-owned | Makes runtime role traits, activation/context/snapshot split, catalog install boundary, external publication roles, executable snapshot contract, plugin contributions, tool decisions, and simple-design checks explicit |
| `design/neutral-waist.md` | Runtime-owned | Runtime execution ports and extension points |
| `design/tool-and-capability.md` | Runtime-owned | Capability segmentation, neutral ToolExecutor port, builtin-tools placement, unified delegation tool, and permission boundary |
| `design/run-ingress-message-delivery.md` | Boundary-only | Dispatch/server boundary for run ingress, durable delivery, pending input, and message handoff |
| `design/anthropic-alignment-and-sessions.md` | Product-owned | Downstream product adapter guidance |
| `design/credentials-and-vaults.md` | Product-owned | Credential/product concern; runtime sees opaque refs only |
| `design/resources-memory-files-skills.md` | Product-owned | Resource data plane and out-of-process execution stay outside runtime core |
| `design/observability-eval-dataset-boundary.md` | Boundary-only | Separates traces, datasets, eval, and analytics from runtime truth |
| `design/error-taxonomy.md` | Ready | Classifies neutral errors and public adapter error mapping |
| `design/packaging-enforcement-matrix.md` | Ready | Defines package/import/license/vocabulary enforcement expectations |
| `wiki/*` | Retrieval-ready | LLM/wiki facts are synced to source docs and current guardrails |

## Role Catalog Coverage

This table is the machine-checked classification for source documents. It decides
which documents must carry a role/component catalog and which documents are
meta, navigational, delegated, or downstream mapping documents.

The table is not a second source of design truth. A `Role owner` document owns
only the catalog for its stable roles; lifecycle state machines live in the
owning code/Rustdoc when implemented, or in that same owner document before
implementation. Meta, coverage, status, and wiki documents link to those owners.

| Document | Class | Catalog policy | Catalog owner |
|---|---|---|---|
| `README.md` | Meta / introspection | Not required | n/a |
| `STATUS.md` | Meta / introspection | Not required | n/a |
| `INVARIANTS.md` | Guardrail registry | Not required | n/a |
| `requirements-coverage.md` | Coverage map | Not required | n/a |
| `design/architecture-overview.md` | Navigation / context map | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/config-to-run-execution-flow.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/config-publication-lifecycle.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/protocol-adapter-boundaries.md` | Product/downstream mapping | Not required | n/a |
| `design/permission-policy-axis.md` | Role owner | Required | self |
| `design/model-provider-backend-binding.md` | Role owner | Required | self |
| `design/commit-fact-projection-taxonomy.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/builtin-tools-extension-contract.md` | Delegated boundary narrative | Delegated | [tool-and-capability.md](design/tool-and-capability.md#tool-and-capability-role-catalog) |
| `design/key-design-decisions.md` | Decision record | Not required | n/a |
| `design/runtime-behavior.md` | Role owner | Required | self |
| `design/runtime-scenario-validation.md` | Coverage map | Not required | n/a |
| `design/runtime-interface-boundaries.md` | Role owner | Required | self |
| `design/neutral-waist.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/tool-and-capability.md` | Role owner | Required | self |
| `design/run-ingress-message-delivery.md` | Role owner | Required | self |
| `design/anthropic-alignment-and-sessions.md` | Product/downstream mapping | Not required | n/a |
| `design/credentials-and-vaults.md` | Product/downstream mapping | Not required | n/a |
| `design/resources-memory-files-skills.md` | Product/downstream mapping | Not required | n/a |
| `design/observability-eval-dataset-boundary.md` | Product/downstream mapping | Not required | n/a |
| `design/error-taxonomy.md` | Decision record | Not required | n/a |
| `design/packaging-enforcement-matrix.md` | Meta / introspection | Not required | n/a |
| `adr/0001-documentation-model-and-vocabulary-alignment.md` | Decision record | Not required | n/a |
| `adr/0002-resolver-role-demarcation.md` | Decision record | Not required | n/a |
| `adr/0003-deferred-work-mechanism-selection.md` | Decision record | Not required | n/a |
| `adr/0004-plugin-factory-contributions-and-capability-bound.md` | Decision record | Not required | n/a |
| `adr/0005-run-terminal-state-single-authority.md` | Decision record | Not required | n/a |
| `adr/0006-fact-authority-run-record-is-cache.md` | Decision record | Not required | n/a |
| `adr/0007-runtime-owns-tool-execution.md` | Decision record | Not required | n/a |
| `adr/0008-durable-postgres-commit-backend.md` | Decision record | Not required | n/a |
| `adr/0009-durable-run-ingress-slice.md` | Decision record | Not required | n/a |
| `adr/0010-idempotent-pending-consumption.md` | Decision record | Not required | n/a |
| `adr/0011-autonomous-dispatch-service.md` | Decision record | Not required | n/a |
| `adr/0012-sqlite-and-postgres-store-backends.md` | Decision record | Not required | n/a |
| `adr/0013-pending-lifecycle-and-cross-thread-outbox.md` | Decision record | Not required | n/a |
| `adr/0014-scheduled-delivery.md` | Decision record | Not required | n/a |
| `adr/0015-crash-retry-budget-and-dead-letter.md` | Decision record | Not required | n/a |
| `adr/0016-durable-cancel.md` | Decision record | Not required | n/a |
| `adr/0017-send-message-over-outbox.md` | Decision record | Not required | n/a |
| `adr/0018-priority-dedupe-gc.md` | Decision record | Not required | n/a |
| `adr/0019-distributed-dispatch-and-wake-signal.md` | Decision record | Not required | n/a |
| `adr/0020-scheduled-action.md` | Decision record | Not required | n/a |
| `adr/0021-idle-thread-delivery.md` | Decision record | Not required | n/a |
| `adr/0022-epoch-supersession.md` | Decision record | Not required | n/a |
| `adr/0023-dead-letter-ttl-gc.md` | Decision record | Not required | n/a |
| `adr/0024-daemon-lease-renewal.md` | Decision record | Not required | n/a |
| `adr/0025-dispatch-query-surface.md` | Decision record | Not required | n/a |
| `adr/0026-stop-policy.md` | Decision record | Not required | n/a |
| `adr/0027-scheduled-action-kind-axis.md` | Decision record | Not required | n/a |
| `adr/0028-nats-store-deferral.md` | Decision record | Not required | n/a |

## Implementation Context

| Area | Owner | Current guidance |
|---|---|---|
| Runtime protocol/license | Current repository | Protocol/specification, SDK-facing schemas, examples, and conformance tests use Apache-2.0; code packages may carry their own package/file license metadata |
| Runtime core | Current repository | Keep runtime-owned crates/package open and neutral; use `RuntimeCatalogInstaller`, snapshot execution ports, serializable `ResolvedSpec`, and committed state/effect facts without owning config publication compilation |
| Dispatch/server | `awaken-run-ingress` (host crate) + adjacent server/config package | Use `RunIngress` with direct `DirectRunIngress` (runtime) and durable `DurableRunIngress` (`awaken-run-ingress`, [ADR-0009](adr/0009-durable-run-ingress-slice.md)); the host depends on runtime and the store adapter, never the reverse |
| Config publication | Config domain / adjacent server-config package | Keep `ConfigPublicationCoordinator` and `RegistryCompiler` outside runtime core; hand runtime a complete catalog install request through `RuntimeCatalogInstaller` |
| Hosted product adapters | Downstream product package or repository | Implement public DTOs/events behind anti-corruption adapters; product hosting vocabulary does not enter neutral runtime/protocol/config code |
| Credentials/vaults | Credential domain / Product | Opaque refs into runtime; no grant from selection/probe |
| Observability/eval | Analytics/DX package or repository | Consume committed facts or normal runtime ports; never become runtime truth |

## Not Ready Without More Detail

The following should not be implemented as broad subsystems from these docs alone:

- a product-first managed crate family as the core architecture;
- a universal execution abstraction that replaces `RunIngress` or durable ingress
  internals before a concrete server slice requires it. The shipped surface is
  `DirectRunIngress` plus durable `DurableRunIngress` (`awaken-run-ingress`,
  [ADR-0009](adr/0009-durable-run-ingress-slice.md)): a durable submit persists an
  accepted run, a worker claims and runs it under a single-owner lease, an expired
  lease is recovered, and a parked run resumes through delivered input — all over
  the durable `CommitCoordinator` backend ([ADR-0008](adr/0008-durable-postgres-commit-backend.md)).
  That slice is intentionally minimal: scheduled wake, lease renewal, cross-thread
  outbox, dispatch query/maintenance, supersession, and dead-letter are named as
  deferred in ADR-0009, not built. The commit contract any backend must satisfy is
  fixed by [ADR-0006](adr/0006-fact-authority-run-record-is-cache.md);
- product-specific vaults, sessions, and outcome fields inside runtime crates.
- a manager/controller that owns config loading, registry compilation, catalog
  install, live control, and execution as one runtime object;
- a public protocol adapter without conformance fixtures, unsupported-management
  tests, and DTO leak checks;
- an executor/environment channel without correlation, idempotency, and
  indeterminate-result tests.
