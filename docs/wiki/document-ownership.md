---
type: Ownership Index
title: Document Ownership
description: Map of source documents to the wiki pages that index them.
tags: [wiki, ownership, documentation]
timestamp: 2026-06-27T00:00:00+08:00
---

# Document Ownership

Source documents own behavior. Wiki pages own retrieval facts only.

## Source Documents

| Source document | Owns | Wiki index |
|---|---|---|
| [requirements-coverage.md](../requirements-coverage.md) | requirement coverage across supported packaging targets | [index.md](index.md) |
| [adr/0001-documentation-model-and-vocabulary-alignment.md](../adr/0001-documentation-model-and-vocabulary-alignment.md) | documentation model, guardrail-enforcer rule, and vocabulary alignment to workspace types | [index.md](index.md) |
| [adr/0002-resolver-role-demarcation.md](../adr/0002-resolver-role-demarcation.md) | the three canonical resolver roles and their boundaries | [index.md](index.md) |
| [adr/0003-deferred-work-mechanism-selection.md](../adr/0003-deferred-work-mechanism-selection.md) | selecting among ScheduledAction / waiting ticket / RunDispatch | [index.md](index.md) |
| [adr/0004-plugin-factory-contributions-and-capability-bound.md](../adr/0004-plugin-factory-contributions-and-capability-bound.md) | the Plugin factory, resolved Contributions, ResolvedExecutionEnv aggregate, and CapabilityBound as a fail-closed contribution ceiling | [index.md](index.md) |
| [adr/0005-run-terminal-state-single-authority.md](../adr/0005-run-terminal-state-single-authority.md) | the committed run Phase (Waiting / Ended(EndCause)) as the one stored terminal authority, with status/outcome/error derived | [index.md](index.md) |
| [adr/0006-fact-authority-run-record-is-cache.md](../adr/0006-fact-authority-run-record-is-cache.md) | the committed fact log as the run read authority and RunRecord as a derived cache equal to the latest fact | [index.md](index.md) |
| [adr/0007-runtime-owns-tool-execution.md](../adr/0007-runtime-owns-tool-execution.md) | the runtime executes tools in-process; concrete tools live in the extension; ExecutionBackend/G7 retired | [index.md](index.md) |
| [adr/0008-durable-postgres-commit-backend.md](../adr/0008-durable-postgres-commit-backend.md) | a durable Postgres CommitCoordinator/read-port backend via sqlx + scoped migrations, with an in-memory projection for the sync reads | [index.md](index.md) |
| [adr/0009-durable-run-ingress-slice.md](../adr/0009-durable-run-ingress-slice.md) | a minimal DurableRunIngress slice in awaken-run-ingress: RunDispatch claim/lease/recovery, PendingInbox, and a worker that reads committed truth, with deferred items named | [index.md](index.md) |
| [adr/0010-idempotent-pending-consumption.md](../adr/0010-idempotent-pending-consumption.md) | pending input keyed to the ticket correlation it answers, so a crashed resume is never re-applied without an atomic append+freeze; drops the frozen column | [index.md](index.md) |
| [adr/0011-autonomous-dispatch-service.md](../adr/0011-autonomous-dispatch-service.md) | DispatchService daemon draining the queue on a nudge or poll with automatic crashed-lease recovery, and a Clock port keeping the worker deterministic | [index.md](index.md) |
| [adr/0012-sqlite-and-postgres-store-backends.md](../adr/0012-sqlite-and-postgres-store-backends.md) | SQLite and Postgres adapters for the commit and dispatch layers over one portable schema; rusqlite runs sync writes on a blocking thread, BEGIN IMMEDIATE guards claims | [index.md](index.md) |
| [adr/0013-pending-lifecycle-and-cross-thread-outbox.md](../adr/0013-pending-lifecycle-and-cross-thread-outbox.md) | revision-guarded pending edit/retract and a transactional cross-thread outbox (idempotent append-then-delete, no 2PC); reorder/delivery_mode dropped under correlation-keyed delivery | [index.md](index.md) |
| [adr/0014-scheduled-delivery.md](../adr/0014-scheduled-delivery.md) | scheduled delivery via a nullable available_at epoch-millis on pending input, compared against the injected clock; flips scheduled_wake true | [index.md](index.md) |
| [architecture-overview.md](../design/architecture-overview.md) | bounded contexts, domain vocabulary, and cross-context boundaries | [index.md](index.md) |
| [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md) | end-to-end handoff from explicit model-provider/model/model-pool/agent config graph through config-side publication coordination, registry compilation, runtime catalog install, executable snapshot selection, activation/context split, resolution, execution, commit, and projection; future integration evidence stays outside existing specs until a tested boundary exists | [config-to-run-execution-flow-facts.md](config-to-run-execution-flow-facts.md) |
| [config-publication-lifecycle.md](../design/config-publication-lifecycle.md) | config publication states, atomic runtime install, rollback, and publication failures | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [protocol-adapter-boundaries.md](../design/protocol-adapter-boundaries.md) | public protocol adapter mapping, conformance, replay, and unsupported management APIs | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [permission-policy-axis.md](../design/permission-policy-axis.md) | authorization flow, permission decisions, HITL tickets, and audit staging | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [model-provider-backend-binding.md](../design/model-provider-backend-binding.md) | model-provider/model/model-pool/agent spec graph, selected binding validation, fallback ownership, capability reconciliation, and narrow `ModelProviderSpec` / `AgentSpec` responsibilities | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [commit-fact-projection-taxonomy.md](../design/commit-fact-projection-taxonomy.md) | live stream output, committed truth, protocol replay, public projection, and dataset sink taxonomy | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [builtin-tools-extension-contract.md](../design/builtin-tools-extension-contract.md) | first-party builtin toolsets and extension package contract | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [key-design-decisions.md](../design/key-design-decisions.md) | load-bearing decisions and rejected alternatives | [engineering-lessons.md](engineering-lessons.md) |
| [runtime-behavior.md](../design/runtime-behavior.md) | run phases, activation/context split, live state apply versus durable commit, state/effects, plugins, cancellation, scheduled work, and eval | [runtime-behavior-facts.md](runtime-behavior-facts.md) |
| [runtime-scenario-validation.md](../design/runtime-scenario-validation.md) | runtime GWT scenario ids, scenario text, executable-test mapping, and scenario test organization | [index.md](index.md) |
| [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md) | runtime role split, activation/context/snapshot split, catalog install boundary, external publication roles, executable snapshot contract, plugin contribution seams, tool decision ladder, independent axes, and simple-design evaluation | [runtime-interface-boundaries-facts.md](runtime-interface-boundaries-facts.md) |
| [neutral-waist.md](../design/neutral-waist.md) | runtime execution ports and data-only config edge | [neutral-waist-facts.md](neutral-waist-facts.md) |
| [tool-and-capability.md](../design/tool-and-capability.md) | descriptors, neutral ToolExecutor port, capability checks, and permission boundaries | [tool-and-capability-facts.md](tool-and-capability-facts.md) |
| [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) | direct/durable run ingress, durable dispatch, pending input, message delivery, and recovery | [run-ingress-message-delivery-facts.md](run-ingress-message-delivery-facts.md) |
| [anthropic-alignment-and-sessions.md](../design/anthropic-alignment-and-sessions.md) | product protocol adapters, sessions, events, and outcome mapping | [anthropic-alignment-and-sessions-facts.md](anthropic-alignment-and-sessions-facts.md) |
| [credentials-and-vaults.md](../design/credentials-and-vaults.md) | credential refs, vault lifecycle, selection, and availability | [credentials-and-vaults-facts.md](credentials-and-vaults-facts.md) |
| [resources-memory-files-skills.md](../design/resources-memory-files-skills.md) | logical resources, memory/files, and skills | [resources-memory-files-skills-facts.md](resources-memory-files-skills-facts.md) |
| [observability-eval-dataset-boundary.md](../design/observability-eval-dataset-boundary.md) | trace, dataset, eval, experiment, and analytics boundaries | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [error-taxonomy.md](../design/error-taxonomy.md) | neutral error ownership and public adapter error mapping | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [packaging-enforcement-matrix.md](../design/packaging-enforcement-matrix.md) | package, import, license, vocabulary, ownership, and catalog enforcement | [runtime-explicit-boundaries-facts.md](runtime-explicit-boundaries-facts.md) |
| [INVARIANTS.md](../INVARIANTS.md) | guardrail registry and DDD review checklist | all fact pages |

## Cross-Theme Anchors

| Anchor | Canonical owner |
|---|---|
| Runtime/server boundary | [architecture-overview.md](../design/architecture-overview.md) and [neutral-waist.md](../design/neutral-waist.md) |
| Config to run execution flow | [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md) |
| Config graph model | [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md#config-graph-model) |
| Publication roles outside runtime | [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#publication-roles-outside-runtime) and [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md#publication-objects-are-outside-runtime) |
| Contract authority boundaries | [key-design-decisions.md D12](../design/key-design-decisions.md#d12---contract-names-follow-authority) and [architecture-overview.md](../design/architecture-overview.md#21-contract-authority-map) |
| Runtime role split | [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#role-split) |
| Activation/context/snapshot split | [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#activation-versus-runtime-context) |
| Executable snapshot contract | [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#snapshot-execution-and-inspection-contract) |
| Plugin contribution seams | [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#plugin-contribution-matrix) |
| Tool decision ladder | [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md#tool-decision-ladder) |
| Permission policy axis | [permission-policy-axis.md](../design/permission-policy-axis.md) |
| Model/provider/backend binding | [model-provider-backend-binding.md](../design/model-provider-backend-binding.md) |
| Run lifecycle and state/effects | [runtime-behavior.md](../design/runtime-behavior.md#run-lifecycle) |
| Runtime GWT scenarios | [runtime-scenario-validation.md](../design/runtime-scenario-validation.md) |
| Runtime event model | [runtime-behavior.md](../design/runtime-behavior.md#runtime-event-model) |
| Commit and projection taxonomy | [commit-fact-projection-taxonomy.md](../design/commit-fact-projection-taxonomy.md) |
| Data-only config edge | [neutral-waist.md](../design/neutral-waist.md#data-only-config-edge) |
| Config publication lifecycle | [config-publication-lifecycle.md](../design/config-publication-lifecycle.md) |
| Capability segmentation | [tool-and-capability.md](../design/tool-and-capability.md#capability-segments) |
| Selection is not authorization | [credentials-and-vaults.md](../design/credentials-and-vaults.md#selection-is-not-authorization) |
| Public protocol names | [anthropic-alignment-and-sessions.md](../design/anthropic-alignment-and-sessions.md#anti-corruption-layer) |
| Public protocol adapters | [protocol-adapter-boundaries.md](../design/protocol-adapter-boundaries.md) |
| Resource realization | [resources-memory-files-skills.md](../design/resources-memory-files-skills.md#resource-boundary) |
| Error taxonomy | [error-taxonomy.md](../design/error-taxonomy.md) |
| Packaging enforcement | [packaging-enforcement-matrix.md](../design/packaging-enforcement-matrix.md) |
