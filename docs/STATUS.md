# Design status and ownership

Source code and its owning ADR define behavior. This page locates those
authorities; it does not certify a build, deployment or product outcome.

## Documentation model

- [Documentation decisions](adr/0001-documentation-model-and-vocabulary-alignment.md)
  define the source-document and downstream-wiki boundary.
- [Guardrails](INVARIANTS.md) identify invariants and their enforcers.
- [Requirements coverage](requirements-coverage.md) connects requirements to
  their existing implementation and evidence owners.
- [Documentation index](README.md) locates the bounded-context design owners.
- [Wiki index](wiki/index.md) provides short retrieval links to those owners.

## Implementation and evidence

Use the current implementation and named tests to assess a behavior. A source
anchor is a design reference, an executed test qualifies its recorded scope,
and provider or deployment acceptance requires its own actual observations.

Test design and execution owns validation methods.
Session acceptance owns its
scenario and source-binding contract. Recorded outcomes remain attached to
their original source, toolchain, platform and artifact hashes; changing a
commit identifier does not create a new successful result.

Machine execution packets, host scheduling and per-integration progress are
kept outside the source repository. Missing historical bytes remain unavailable
evidence, not a new pass or a mandatory platform for unrelated work.

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
| `design/acp-harness-capability-and-tool-bridge.md` | Delegated boundary narrative | Delegated | [tool-and-capability.md](design/tool-and-capability.md#tool-and-capability-role-catalog) |
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
| `design/file-lifecycle.md` | Delegated boundary narrative | Delegated | [resources-memory-files-skills.md](design/resources-memory-files-skills.md#resource-input-component-catalog) |
| `design/resources-memory-files-skills-roadmap.md` | Delegated boundary narrative | Delegated | [resources-memory-files-skills.md](design/resources-memory-files-skills.md#resource-input-component-catalog) |
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
| `adr/0078-terminal-recovery-retains-the-existing-session-authority.md` | Implemented terminal Resource projection, terminal-only release, and bounded same-process Kubernetes physical join | Not required | n/a |
| `adr/0079-one-tool-authorization-vocabulary-and-opaque-backend-configuration.md` | Implemented single authorization vocabulary, retired mode axis, fail-closed baseline, and opaque backend session configuration | Not required | n/a |
| `adr/0080-full-gate-freshness-and-declared-tool-environment.md` | Implemented full-gate record and freshness policy, one declared gate-tool manifest, stage tool/disk prechecks, and the build-space reclamation rules | Not required | n/a |
| `adr/0081-contract-layering-and-ratchet-guardrails.md` | Accepted contract layering; ratchet guardrails, deployment fact ownership, executable Environment contract merge, Hosted Runtime vocabulary placement and async Resource Registry ports implemented; guard-passing fences and typed sandbox errors land separately | Not required | n/a |
| `adr/0082-open-specification-closed-substrate.md` | Proposed; awaits the product decision it records (open specification and single-node backends, closed substrate); no implementation until accepted | Not required | n/a |
