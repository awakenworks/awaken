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
| [adr/0015-crash-retry-budget-and-dead-letter.md](../adr/0015-crash-retry-budget-and-dead-letter.md) | a crash-retry budget (attempt_count increments on recovery, resets on park) and a reap step that dead-letters a poison run, with dead_letters/requeue ops | [index.md](index.md) |
| [adr/0016-durable-cancel.md](../adr/0016-durable-cancel.md) | durable cancel of a queued/parked run: dispatch removes, Runtime::cancel_run commits a terminal Cancelled through the one finish boundary; supersession-by-epoch deferred | [index.md](index.md) |
| [adr/0017-send-message-over-outbox.md](../adr/0017-send-message-over-outbox.md) | send_message backed by the outbox via a host adapter, addressed by thread (not ephemeral run); resolves the thread's parked run/ticket and stages a delivery | [index.md](index.md) |
| [adr/0018-priority-dedupe-gc.md](../adr/0018-priority-dedupe-gc.md) | dispatch priority (fresh band), a live-key dedupe on enqueue_with, and operator dead-letter GC (purge_dead_letters); enqueue stays a default-preserving provided method | [index.md](index.md) |
| [adr/0019-distributed-dispatch-and-wake-signal.md](../adr/0019-distributed-dispatch-and-wake-signal.md) | Postgres already claims across nodes (SKIP LOCKED); renew_lease keeps a long run owned; a WakeSignal port (local + feature-gated NATS) replaces the daemon notify | [index.md](index.md) |
| [adr/0020-scheduled-action.md](../adr/0020-scheduled-action.md) | ScheduledAction (ADR-0003 mechanism #1) as a WaitingReason: a gate Schedule commits the deferred action, perform_scheduled_action allow-resumes it, the worker performs it in-process; recovered from committed state for consistency | [index.md](index.md) |
| [adr/0021-idle-thread-delivery.md](../adr/0021-idle-thread-delivery.md) | send_message to a thread with no parked run stages unbound pending input (empty run/correlation); the thread's next run drains it as new input, consumed on settle; auto-activation deferred | [index.md](index.md) |
| [adr/0022-epoch-supersession.md](../adr/0022-epoch-supersession.md) | opt-in SubmitOptions.supersede marks the thread's prior pending/parked dispatches superseded by a monotonic epoch (newest wins), excluded from claim; superseded() lists them; running-run and committed-Cancelled deferred | [index.md](index.md) |
| [adr/0023-dead-letter-ttl-gc.md](../adr/0023-dead-letter-ttl-gc.md) | reap stamps dead_lettered_at (epoch ms); purge_dead_letters_before(cutoff) ages out old dead-letters; the daemon GCs on its cadence when DispatchServiceConfig.dead_letter_ttl is set | [index.md](index.md) |
| [adr/0024-daemon-lease-renewal.md](../adr/0024-daemon-lease-renewal.md) | renew_owned_leases bulk-renews a daemon owner's running leases; a heartbeat task runs it on lease_renewal_interval concurrent with the drain so a long run is not stolen | [index.md](index.md) |
| [adr/0025-dispatch-query-surface.md](../adr/0025-dispatch-query-surface.md) | list_dispatches returns a DispatchSummary (run/thread/status/attempts) per row; public DispatchStatus mapped from each backend; read-only, carries no run-outcome truth | [index.md](index.md) |
| [adr/0026-stop-policy.md](../adr/0026-stop-policy.md) | EndCause::Stopped(reason) and Runtime::stop_run commit a terminal host-policy stop through the finish boundary (clearing the ticket); a late resume/scheduled result fails closed; policy thresholds stay host-owned | [index.md](index.md) |
| [adr/0027-scheduled-action-kind-axis.md](../adr/0027-scheduled-action-kind-axis.md) | CapabilityBound/Contributions/ResolvedExecutionEnv gain an action_kinds axis (G30) enforced like tools; GateOutcome::Schedule.action_kind validated via permits_action_kind, unselected kind fails closed | [index.md](index.md) |
| [adr/0028-nats-store-deferral.md](../adr/0028-nats-store-deferral.md) | NATS wake signal is live-tested (feature nats, skip-without-server); a NATS-backed KV store stays deferred (untestable here, Postgres already gives distributed claim); the DispatchStore seam + shared specs make it a drop-in | [index.md](index.md) |
| [adr/0029-built-in-runtime-namespace.md](../adr/0029-built-in-runtime-namespace.md) | the dispatch and commit stores hard-code a runtime table namespace (no prefix constructor arg); test isolation is per-test Postgres schema (search_path), never the store API; re-adding a prefix needs a tenant requirement | [index.md](index.md) |
| [adr/0030-permission-policy-axis.md](../adr/0030-permission-policy-axis.md) | PermissionGate maps a PermissionPolicy decision to a GateOutcome (ask→Suspend on a WaitingTicket DecisionTicket); audit is a committed PermissionDecided event; PermissionDecision stays Allow/Deny/Ask (set_result→gate SetResult, require_scope→ask); rule policy lives in awaken-ext-permission | [index.md](index.md) |
| [adr/0031-config-store.md](../adr/0031-config-store.md) | awaken-config-store compiles an AgentConfig into a content-addressed (sha256) Publication (snapshot+install the runtime consumes); ConfigStore persists configs/publications under the config namespace (SQLite/Postgres, ADR-0029); lifecycle spine draft→compiled→published, richer states deferred | [index.md](index.md) |
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
