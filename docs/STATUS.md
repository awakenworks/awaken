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
| `design/architecture-overview.md` | Ready | Maps the implemented Control/Coordinator/Resources/Worker boundaries, aggregate ownership, lifecycle processors, and runtime/server seams |
| `design/config-to-run-execution-flow.md` | Ready | Owns static Agent/Environment publication, role-scoped database/migration ownership, Deployment/Session creation, and request execution through per-kind materialization, commit, and HTTP/SSE response |
| `design/config-publication-lifecycle.md` | Ready | Owns Control publication and Coordinator executable-registration semantics, idempotency, recovery, and failure rules without a parallel whole-catalog install model |
| `design/protocol-adapter-boundaries.md` | Product-owned | Defines public protocol adapter mapping, conformance, and unsupported management boundaries |
| `design/permission-policy-axis.md` | Runtime-owned | Defines authorization flow, permission decisions, HITL tickets, and audit staging |
| `design/model-provider-backend-binding.md` | Runtime-owned | Defines model-provider/model/model-pool/agent spec graph, selected binding validation, fallback ownership, capability reconciliation, and narrow `ModelProviderSpec` / `AgentSpec` responsibilities |
| `design/commit-fact-projection-taxonomy.md` | Runtime-owned | Separates live stream output, committed truth, replay rows, and public projections |
| `design/builtin-tools-extension-contract.md` | Runtime-owned | Defines `awaken-ext-builtin-tools` package boundaries and toolset contracts |
| `design/key-design-decisions.md` | Ready | Defines runtime-owned implementation decisions, explicit config graph, concrete-tool packaging, admin-tool ownership, neutral naming, publication-role placement, and rejected leaks |
| `design/runtime-behavior.md` | Runtime-owned | Covers run lifecycle, activation/context split, live state apply versus durable commit, state/effects/events, extensions, cancellation, scheduling, eval |
| `design/auxiliary-context-windows.md` | Ready | Defines shared transcript windows, extension ownership, and asynchronous Memory, Compact, and Outcome behavior |
| `design/session-branch-prefixes.md` | Ready | Defines immutable cross-Session transcript prefixes without copied target history or parallel branch state |
| `design/runtime-scenario-validation.md` | Ready | Owns runtime GWT scenario ids, scenario text, executable-test mapping, and scenario test organization |
| `design/runtime-interface-boundaries.md` | Runtime-owned | Makes runtime role traits, activation/context/snapshot split, external publication-registration roles, executable snapshot contract, plugin contributions, tool decisions, and simple-design checks explicit |
| `design/neutral-waist.md` | Runtime-owned | Runtime execution ports and extension points |
| `design/tool-and-capability.md` | Runtime-owned | Capability segmentation, neutral ToolExecutor port, builtin-tools placement, unified delegation tool, and permission boundary |
| `design/run-ingress-message-delivery.md` | Boundary-only | Dispatch/server boundary for run ingress, durable delivery, pending input, and message handoff |
| `design/remote-worker-protocol.md` | Boundary-only | Accepted P0/P1/P2 contract for recoverable, database-independent, embeddable remote Workers; PostgreSQL active-active execution is verified without sticky routing, while optional transfer optimizations remain deferred |
| `application-authentication.md` | Product-owned | Integration guidance for service credentials, application tokens, and authenticated frontend transports |
| `design/anthropic-alignment-and-sessions.md` | Product-owned | Downstream product adapter guidance |
| `design/managed-dream.md` | Product-owned | Owns the Managed Dream API and Dream job, frozen JSONL/session evidence, read-only source snapshot, required write-through result MemoryStore, restricted ordinary Agent execution, recovery, and test design |
| `design/managed-deployments.md` | Product-owned | Owns durable Managed Deployment/DeploymentRun scheduling, Workspace scope, Agent version freezing, occurrence claims, lifecycle facts, and Dream scheduler coordination |
| `design/web-ui.md` | Product-owned | Web console blueprint: Oversight two-scope shell over the management plane, session surface, design tokens, contract-first frontend engineering plan |
| `design/credentials-and-vaults.md` | Product-owned | Credential/product concern; runtime sees opaque refs only |
| `design/resources-memory-files-skills.md` | Product-owned | Normative Resources application composition and File/Memory/Repository/Skill lifecycles, including command ownership, config resolution, activation, explicit terminal Repository publication, recovery, and reclamation |
| `design/runtime-persistence.md` | Runtime-owned | Expands ADR-0039: persistence bounded contexts (agent-truth / dispatch / config / protocol-projection), port surface, `awaken-store-<medium>` backend matrix, atomic staged commit (G13), and fact-authority reads (D4) |
| `design/tool-state-machine.md` | Runtime-owned | Defines the tool call state machine: typed state cells over the untyped command store, the four runtime seams (state materialization, tool gate chain, tool-outcome reaction hook, run-end guard state), reminder emission via the conversation aggregate, capability bounds, and persistence/atomicity/transactionality/restart guarantees |
| `design/plugin-configuration.md` | Runtime-owned | Defines per-plugin configuration: the raw config carrier on the resolved spec, config-aware resolve, validation as a dry run of resolve, schema derived from the config type via schemars, and delivery to the frontend on the capability catalog |
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
| `formal-verification-expansion.md` | Coverage report | Not required | n/a |
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
| `design/auxiliary-context-windows.md` | Design narrative | Not required | n/a |
| `design/session-branch-prefixes.md` | Product/downstream mapping | Not required | n/a |
| `design/runtime-scenario-validation.md` | Coverage map | Not required | n/a |
| `design/runtime-interface-boundaries.md` | Role owner | Required | self |
| `design/neutral-waist.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/tool-and-capability.md` | Role owner | Required | self |
| `design/run-ingress-message-delivery.md` | Role owner | Required | self |
| `design/remote-worker-protocol.md` | Role owner | Required | self |
| `application-authentication.md` | Role owner | Required | self |
| `design/anthropic-alignment-and-sessions.md` | Product/downstream mapping | Not required | n/a |
| `design/managed-dream.md` | Role owner | Required | self |
| `design/managed-deployments.md` | Role owner | Required | self |
| `design/web-ui.md` | Product/downstream mapping | Not required | n/a |
| `design/credentials-and-vaults.md` | Product/downstream mapping | Not required | n/a |
| `design/resources-memory-files-skills.md` | Role owner | Required | self |
| `design/observability-eval-dataset-boundary.md` | Product/downstream mapping | Not required | n/a |
| `design/prompt-skill-optimization-data-contracts.md` | Proposed implementation contract | Not required | n/a |
| `design/prompt-skill-optimization-state-machine.md` | Proposed lifecycle owner | Not required | n/a |
| `design/error-taxonomy.md` | Decision record | Not required | n/a |
| `design/packaging-enforcement-matrix.md` | Meta / introspection | Not required | n/a |
| `design/runtime-persistence.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/tool-state-machine.md` | Role owner | Required | self |
| `design/plugin-configuration.md` | Role owner | Required | self |
| `design/distributed-acp-execution.md` | Design narrative | Not required | n/a |
| `design/brain-hand-coverage.md` | Coverage report | Not required | n/a |
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
| `adr/0029-built-in-runtime-namespace.md` | Decision record | Not required | n/a |
| `adr/0030-permission-policy-axis.md` | Decision record | Not required | n/a |
| `adr/0031-config-store.md` | Decision record | Not required | n/a |
| `adr/0032-runnable-config.md` | Decision record | Not required | n/a |
| `adr/0033-in-process-run-driver.md` | Decision record | Not required | n/a |
| `adr/0034-runtime-axis-model-and-orthogonality.md` | Decision record | Not required | n/a |
| `adr/0035-environment-provisioning-tools-skills-resources.md` | Decision record | Not required | n/a |
| `adr/0036-skills-as-runtime-extension-single-tool.md` | Decision record | Not required | n/a |
| `adr/0037-managed-capability-advertisement-wire-alignment.md` | Decision record | Not required | n/a |
| `adr/0038-managed-resource-injection-and-store-organization.md` | Decision record | Not required | n/a |
| `adr/0039-runtime-persistence-port-convergence-and-store-naming.md` | Decision record | Not required | n/a |
| `adr/0040-server-durable-ingress-integration.md` | Decision record | Not required | n/a |
| `adr/0041-sandbox-execution-environment-provider.md` | Decision record | Required | self |
| `adr/0042-public-api-tenancy-authz-and-front-door-consistency.md` | Decision record | Not required | n/a |
| `adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md` | Decision record | Not required | n/a |
| `adr/0044-remote-hand-tool-executor-over-a-channel.md` | Decision record | Not required | n/a |
| `adr/0045-connection-plan-and-network-topology.md` | Decision record | Not required | n/a |
| `adr/0046-hand-placement-tool-executor-provider.md` | Decision record | Not required | n/a |
| `adr/0047-compaction-as-agent-run-and-the-context-plane-boundary.md` | Decision record | Not required | n/a |
| `adr/0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md` | Decision record | Not required | n/a |
| `adr/0049-a2a-cross-tenant-federation-carve-out.md` | Decision record | Not required | n/a |
| `adr/0050-telemetry-content-capture-consent-and-gdpr-erasure.md` | Decision record | Not required | n/a |
| `adr/0051-tenancy-edge-aspect-one-opaque-scope-id.md` | Decision record | Not required | n/a |
| `adr/0052-management-assistant-ordinary-agent-in-a-reserved-scope.md` | Decision record | Not required | n/a |
| `adr/0053-memory-store-fuse-mount.md` | Decision record | Not required | n/a |
| `adr/0054-safe-loop-boundary-shared-seam-and-pause-as-durable-await.md` | Decision record | Not required | n/a |
| `adr/0055-typed-state-kernel-loop-actions-as-state.md` | Decision record | Not required | n/a |
| `adr/0056-sandbox-reuse-two-orthogonal-volumes.md` | Decision record | Not required | n/a |
| `adr/0057-unified-agent-configuration.md` | Decision record | Not required | n/a |
| `adr/0058-one-neutral-event-vocabulary-message-sourced-classify-routed-two-tier-projection.md` | Decision record | Not required | n/a |
| `adr/0059-neutral-core-and-leaf-evolution.md` | Decision record | Not required | n/a |
| `adr/0060-durable-dispatch-completion-tombstone.md` | Decision record | Not required | n/a |
| `adr/0061-selectable-identity-and-platform-managed-resource-scopes.md` | Decision record | Not required | n/a |
| `adr/0062-published-inference-access-and-runtime-credential-injection.md` | Decision record | Not required | n/a |
| `adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md` | Decision record | Not required | n/a |
| `adr/0064-runtime-owned-outcome-orchestration.md` | Decision record | Not required | n/a |
| `adr/0065-recoverable-embeddable-remote-worker.md` | Decision record | Not required | n/a |
| `adr/0066-session-service-binding-and-realization.md` | Accepted target with implemented immutable mutation-policy and atomic create-receipt slices; whole guardrail remains target until deployment/E2E evidence is complete | Delegated | [remote Worker component catalog](design/remote-worker-protocol.md#10-remote-worker-component-catalog) |
| `adr/0067-credential-custody-model-exposure-and-secret-delivery.md` | Accepted target landing in verified slices; whole guardrail remains target until implementation/E2E evidence is complete | Delegated | [remote Worker component catalog](design/remote-worker-protocol.md#10-remote-worker-component-catalog) |
| `adr/0068-unified-prompt-and-skill-optimization.md` | Proposed decision record | Not required | n/a |
| `adr/0069-acp-capability-configuration-lifecycle.md` | Proposed decision record | Not required | n/a |
| `adr/0070-local-browser-session-bootstrap.md` | Decision record | Not required | n/a |
| `adr/0071-distributed-service-boundaries-and-executable-agent-registration.md` | Implemented decision record | Not required | n/a |
| `adr/0072-environment-definition-and-execution-boundary.md` | Accepted Environment definition/execution ownership target | Required | self |
| `adr/0073-session-environment-owned-hand-and-worker-capability-placement.md` | Implemented decision record | Not required | n/a |
| `adr/0074-session-environment-suspension-and-checkpointed-continuation.md` | Proposed Session Environment continuation decision | Not required | n/a |
| `adr/0075-unified-managed-session-worker-execution.md` | Implemented Session Worker, complete-profiled-creation, effect-fenced replay, and terminal Repository publication placement decision | Not required | n/a |
| `adr/0076-session-owned-background-tool-execution.md` | Implemented Native BackgroundTask decision | Not required | n/a |
| `adr/0077-iam-directory-product-space-placement.md` | Implemented product-space placement decision | Not required | n/a |

## Implementation Context

| Area | Owner | Current guidance |
|---|---|---|
| Runtime protocol/license | Current repository | Protocol/specification, SDK-facing schemas, examples, and conformance tests use Apache-2.0; code packages may carry their own package/file license metadata |
| Runtime core | Current repository | Keep runtime-owned crates/package open and neutral; consume immutable `ExecutableAgentSnapshot`/`RunActivation` data and commit state/effect facts without owning configuration publication or executable registration |
| Dispatch/server | `awaken-run-ingress` (host crate) + adjacent server/config package | Application admission selects a private direct/durable value. Direct uses `DirectAttemptDriver`; durable uses `RunDispatch` plus `DispatchPool`/`DispatchWorker` directly. Runtime `ActiveAttemptScope` alone owns current-attempt controls and a fresh optional LiveInbox. The host depends on runtime and store adapters, never the reverse |
| Config publication and executable availability | Control and Coordinator | Control persists `StoredPublication` and registers its immutable snapshot through `ExecutableAgentRegistrar`; `ExecutableAgentCatalog` is the single Coordinator execution projection. One Control registration supervisor recovers Agent and Environment facts with readiness/backoff/lag reporting. Active-active PostgreSQL projections advance by durable high-water and fall back to full replay. AllInOne uses local adapters, split roles use authenticated adapters, and Worker receives neither tokens nor authority databases |
| Environment definition and executable availability | Control and Coordinator | `awaken-environment-contract` and `EnvRegistry` own static definitions, revisions, and exact sandbox-policy references in Control. `EnvironmentApplication` is the sole authoring command/recovery path; `EnvironmentExecutionApplication` is the sole Coordinator snapshot/work/convergence path over `ExecutableEnvironmentCatalog` and `WorkQueue`. Archive retains Control history and withdraws current execution availability; Managed owns only HTTP mapping. |
| Resource application and lifecycle | Resources, currently co-deployed by Coordinator | One `ResourceAuthorities` value selects the catalog and File/Memory/Skill/lifecycle authorities; one `ResourcesApplication` derives `FileApplicationService` and purge scheduling; one router exposes File/Memory/Skill management. Runtime artifact harvesting and public File APIs call the same application service; Worker materialization remains per-kind and claim-fenced. No standalone role is exposed until independent scaling or credential isolation also supplies authenticated claim/reference transports |
| Hosted product adapters | Downstream product package or repository | Implement public DTOs/events behind anti-corruption adapters; product hosting vocabulary does not enter neutral runtime/protocol/config code |
| Hosted Management identity | `awaken-cli` + `awaken-control` over the pinned `awaken-iam-host` adapter | Server-mode Cloud identity requires a dedicated projected service-token file, reloads it for each PDP request, requires an explicit user bearer, and fails closed without reusing Flow identity or a cached local login (ADR-0061 amendment) |
| Hosted PostgreSQL schema lifecycle | `awaken database migrate` plus each bounded-context store's canonical scoped bundle | The migration command is the sole DDL writer. Server-mode `connect_existing` verifies exact ledgers and performs no DDL. Control owns Environment/Sandbox Policy bundles, Coordinator owns execution/work bundles and executable command logs, Resources owns catalog/content/lifecycle bundles, and Worker owns none. Migrations are unconditional, dense-versioned, checksummed, and fitness-checked |
| Unified Managed Session Worker execution | Session + Environment WorkQueue + Runtime Host | ADR-0075 is implemented: ordinary Managed creation retains its compatible wire shape and authoring policy, while each profiled command lowers its mode, direct Resource/Repository/MCP inputs, and mutation policy into one finalized revision-1 Session root. The repository atomically returns Applied/Replayed; only Applied continues into realization, activation, and eligible WorkQueue dispatch, while replay returns durable truth with no repeated effect and a failed root remains a typed 409. ADR-0066 owns the persisted Managed/Frozen/FileResources mutation authority and the full-stop profiled-receipt cutover. Existing root CAS, Resource/MCP generations, realization leases, claim fencing, and attempt decoration remain authoritative. Workspace compilation and decision-table tests are the promotion evidence |
| Observability/eval | Analytics/DX package or repository | Consume committed facts or normal runtime ports; never become runtime truth |
| E2E coverage | `scripts/ci/e2e-coverage.sh` + `e2e/` | Deterministic served-process API coverage. `stage_change_coverage_e2e.ts` owns uniquely identified functional obligations, including consolidated G42/G43 promotion evidence; the gate computes and prints the current total and executed numerator instead of duplicating a claimed count here. The post-rebase 2026-07-22 changed-line audit against `t` measured **1589/1662 = 95.61%**, above the enforced strict threshold of **>95%**; 264 non-API-reachable production lines are separately bounded and audited rather than hidden by broad exclusions. |
| Distributed role E2E | `deploy/k3d/README.md` + `e2e/k3d/distributed_control_e2e.sh` | ADR-0071 runs the production Control process with only a deterministic model-publication fact injected at its existing SPI, replicated shipped Coordinator roles, two shipped `awaken-worker` processes, signed Worker traffic, exact Credential/File/Memory/Skill realization, four owner databases, one public API endpoint, active-active projection refresh, Worker incarnation restart, Pod/node/Provider faults, PostgreSQL standby promotion, and concurrent load. Worker Pods receive no authority DB or seal configuration and are checked for zero PostgreSQL sockets; this is the P2-B process-level acceptance owner. |
| Session execution placement | Coordinator durable dispatch + registered Worker + `SessionEnvironment` | [ADR-0073](adr/0073-session-environment-owned-hand-and-worker-capability-placement.md): protocols remain resident ingress adapters; one capability-bearing Worker executes Native/ACP/outbound-A2A attempts; one SessionEnvironment owns Native Hand and ACP state. `SandboxRequirements` is shared by provider and claim admission. Agent/deployment Hand placement was removed. `awaken-tool-relay` and `ConnectionPlan` remain lower-level adapters used by container Hand transport, not a second placement authority. |

## Not Ready Without More Detail

The following should not be implemented as broad subsystems from these docs alone:

- a product-first managed crate family as the core architecture;
- a universal execution abstraction that merges application admission, direct
  attempts, durable Dispatch, or live input. The shipped surface is the concrete
  `DirectAttemptDriver` plus `RunDispatch`/`DispatchPool`/`DispatchWorker`
  ([ADR-0009](adr/0009-durable-run-ingress-slice.md)): a durable submit persists an
  accepted run, a worker claims and runs it under a single-owner lease, an expired
  lease is recovered, and an awaiting run resumes through delivered input — all over
  the durable `CommitCoordinator` backend ([ADR-0008](adr/0008-durable-postgres-commit-backend.md)).
  The originally minimal ADR-0009 slice has since gained scheduled wake, lease
  renewal, cross-thread outbox, query/maintenance, supersession, dead-letter, and
  durable completion tombstones through ADR-0011–0027 and ADR-0060. Extend those
  existing authorities and backend-conformance suites; do not create a parallel delivery
  subsystem. The commit contract any backend must satisfy remains fixed by
  [ADR-0006](adr/0006-fact-authority-run-record-is-cache.md);
- product-specific vaults, sessions, and outcome fields inside runtime crates.
- a manager/controller that owns config loading, registry compilation, catalog
  install, live control, and execution as one runtime object;
- a public protocol adapter without conformance fixtures, unsupported-management
  tests, and DTO leak checks;
- an executor/environment channel without correlation, idempotency, and
  indeterminate-result tests.
