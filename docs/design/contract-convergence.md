# Contract convergence by bounded context

Status: design (plan v5 item V4), measured on `1b56f9aea0` (descends from main
`72518f3094`). Implementation follows V0; no code moves with this document.

## Rule

A contract crate is one boundary: it names between whom, who implements and
who calls ([ADR-0091](../adr/0091-architecture-rules.md) D2). Crates on the same
boundary merge; crates on different boundaries stay apart even when used
together. A merge must change the dependency graph, not only the crate count
([ADR-0088](../adr/0088-bounded-context-map-and-sdk-platform-contract.md),
2026-10-07). Plan v5 S4 adds that contracts follow context and callers, not
mechanism. The crate count is an output of these rules, not a target.

## Evidence

Manifests and sources were parsed without a build: `dependency_declarations`
and `production_rust` from `scripts/ci/_crate_boundary_workspace.py`, then every
`awaken_<contract>::` path in each reader was resolved to the items it names.
"Prod readers" have a normal dependency; "dev" readers only a dev-dependency.
"Re-exp" counts first-party items a contract publishes with `pub use`; the total
(156) equals the `contract-reexport` baseline of `_contract_map_fitness.py`.
Layer `contract` has 30 crates: the 29 under `crates/contract` plus `awaken-time`.

| Crate (`awaken-`) | Context | Lines | Prod readers | Dev | Re-exp | Decision |
|---|---|---:|---:|---:|---:|---|
| `session-types` | session | 400 | 35 | 2 | 0 | keep |
| `session-contract` | session | 18852 | 22 | 4 | 0 | keep, absorbs Core content (step 1) |
| `session-configuration-contract` | session | 4843 | 28 | 2 | 3 | keep |
| `session-dream-contract` | session | 365 | 8 | 0 | 0 | keep |
| `session-application-contract` | session | 1092 | 27 | 5 | 3 | keep, narrowed (steps 1–3) |
| `session-execution-contract` | session | 3329 | 18 | 2 | 51 | keep, absorbs realization (step 4) |
| `session-realization-contract` | session | 2451 | 17 | 1 | 3 | **merge** into `session-execution-contract` |
| `session-environment-contract` | environment | 1235 | 13 | 0 | 0 | keep |
| `acp-contract` | environment | 609 | 14 | 0 | 0 | keep |
| `agent-channel` | environment | 51 | 13 | 4 | 0 | keep |
| `container-contract` | environment | 805 | 13 | 0 | 0 | keep |
| `environment-contract` | environment | 655 | 20 | 13 | 0 | keep |
| `executable-environment-contract` | environment | 976 | 12 | 3 | 0 | keep |
| `provisioning-contract` | environment | 5897 | 43 | 6 | 1 | keep |
| `sandbox-control` | environment | 146 | 7 | 2 | 0 | keep |
| `dispatch-contract` | dispatch | 3468 | 14 | 0 | 58 | keep |
| `worker-contract` | dispatch | 1919 | 16 | 4 | 2 | keep |
| `credential-contract` | credentials | 2871 | 38 | 4 | 29 | keep |
| `resource-contract` | resources | 3518 | 36 | 3 | 0 | keep |
| `tenancy` | access | 220 | 18 | 3 | 2 | keep |
| `service-auth-contract` | access | 181 | 4 | 0 | 0 | keep |
| `data-governance-contract` | governance | 317 | 9 | 2 | 0 | keep |
| `deployment-contract` | deployment | 507 | 7 | 0 | 0 | keep |
| `executable-agent-contract` | config | 500 | 10 | 0 | 1 | keep |
| `agent-contract`, `lease-contract`, `runtime-contract`, `tool-contract`, `time`, `web-policy-contract` | agent-runtime (SDK) | 6381 / 437 / 8530 / 2510 / 370 / 725 | 79 / 25 / 60 / 39 / 70 / 2 | – | 0 / 0 / 3 / 0 / 0 / 0 | keep (published SDK, ADR-0086) |

No crate serves unrelated callers, so nothing is split.

## Session context

| Contract | Between | Implemented by | Called by |
|---|---|---|---|
| `session-types` | every Session boundary (shared vocabulary, no trait) | – | 35 readers, including protocols that need only `RunError*` |
| `session-contract` | Session application ↔ its store | `session-store` (repository, reads, reconciliation, outbox) | `session-application`, `session-store`, `store-conformance`; read values for executor, realization, Managed adapter |
| `session-configuration-contract` | Control plane ↔ Session (immutable configuration, resolution ports) | `config-service`, `control`, `resource-application`, `environment-application` | `session-application`, Control plane; 6 readers use it without Core |
| `session-dream-contract` | Dream application ↔ store (separate aggregate) | `dream-application`, `session-store` | `coordinator`, `protocol-awaken`, `protocol-managed` |
| `session-application-contract` | callers ↔ Session use cases | `session-application` (all 12 traits) | protocols, `cli`, `control`, `deployment-application`, `environment-application`, `dispatch-*`, `coordinator`, `webhook` |
| `session-execution-contract` (+ realization) | Session ↔ its Worker-side executors | `session-application`, `session-executor`, `session-run-delivery`, `session-realization(-driver)`, `coordinator-client` | `dispatch-execution`, `dispatch-http`, `coordinator`, `protocol-managed`, `resource-worker-http` |

**Realization merges into execution.** Both cross the same Coordinator ↔ Worker
boundary between the same parties. The realization roles are already split
between them: `RealizationRuntime`, `RealizationEffects`, `TerminalCleanup` and
`McpAttachmentRealizer` live in execution, `SessionRealizationControl` and the
terminal controls in realization; execution imports 10 realization items and
re-exports 6. Of the 16 crates that read realization, 14 also read execution.
Execution already depends on realization, so the merge changes no reader's
closure except `resource-worker-http` and `scenario-host` (+3,329 lines each),
and removes 14 duplicate edges plus one edge between contracts. The target keeps
the name `awaken-session-execution-contract`: realization executes environment
effects (ADR-0091 D7.1). The upward edge `realization-contract → worker-contract`
moves with its code under its existing `context_order_baseline.json` entry and
removal condition; the count stays one.

**The application contract becomes the narrow port for protocol adapters.**
`protocol-a2a`, `-ag-ui` and `-ai-sdk` need only `RunApplication`,
`OwnerScopedRunApplication`, `RunResume`, `RunMode`, `RunApplicationError`,
`Pending`, `StepOutcome`, `EventForwardingSink`, `blocks_text`,
`paginate_history` and `session-types::RunError*`. Today the application
contract depends on Core for three Core-owned modules, which drags Core and its
configuration, credential, provisioning and resource contracts into every
adapter. Moving them (steps 1–3) gives:

| Reader | Closure today (crates / lines) | After |
|---|---|---|
| `session-application-contract` | 14 / 55,461 | 4 / 7,588 |
| `protocol-ag-ui`, `protocol-ai-sdk` | 16 / 56,773 | 6 / 8,900 |
| `protocol-a2a` | 17 / 57,351 | 10 / 23,389 |
| `protocol-awaken` | 23 / 68,417 | 18 / 41,257 |
| `deployment-application` | 17 / 57,560 | 10 / 24,245 |
| `dream-application` | 16 / 56,918 | 8 / 20,085 |
| `dispatch-contract` | 18 / 59,301 | 17 / 40,449 |

**Kept apart, with the reason.** Each would add up to the given closure (lines /
crates) to readers that do not need it:

| Candidate merge | Readers that would pay | Cost |
|---|---|---|
| application → Core | 11 (`protocol-a2a`, `-ag-ui`, `-ai-sdk`, `-awaken`, `cli`, `control`, `deployment-application`, `dream-application`, `environment-application`, `dispatch-contract`, `dispatch-erasure`) | the reduction above, reversed |
| configuration → Core | 6 (`admin-assistant`, `config-service`, `eval`, `resource-application`, `resource-persistence`, `session-environment`) | 55,461 / 14 |
| types → application | 15, including Core itself, which would then depend on its callers' port | 56,553 / 15 and a reversed dependency |
| dream → Core | – | separate aggregate and recovery protocol (ADR-0091 2026-10-19) |

## Other contexts

All other crates keep their boundary. Rejected candidates, with upper-bound
closure costs per reader (lines / crates):

- `worker-contract` → `dispatch-contract`: 9 readers (`worker-registry`,
  `credential-materializer`, `session-mcp`, …) would gain 62,769 / 19.
- `session-environment-contract` → `provisioning-contract`: 52 readers of
  provisioning, including Core, would gain the ACP, container, runtime and tool
  contracts (34,015 / 14). Its metadata is already `environment`.
- `environment-contract` + `executable-environment-contract`: 63 readers would
  gain 18,831 / 10, and provisioning ↔ environment would form a cycle.
- `agent-channel` or `sandbox-control` → `provisioning-contract`: their readers
  (`acp-contract`, `connection-plan`; `sandbox-control-transport` on the Sandbox
  process side) would gain 17,455 / 8. Small leaves stay leaves.
- `service-auth-contract` → `tenancy`: graph-neutral (two edges fewer, two new
  `tenancy` edges for the catalogs), changes 11 awaken-cloud paths; not made.
- `container-contract` stays the transport/broker seam (ADR-0091 2026-10-20).

## Re-exports reach zero

Every Platform contract stops publishing other crates' items; readers name the
owner. 153 of 156 go in V4:

| Contract | Items | Owner readers then import |
|---|---:|---|
| `dispatch-contract` | 58 | `worker-contract` 43, `credential-contract` 14, `tenancy` 1 |
| `session-execution-contract` | 51 | Core 25, application 7, configuration 7, realization 6 (gone with step 4), `session-types` 2, `agent-contract` 2, `provisioning-contract` 2 |
| `credential-contract` | 29 | `agent-contract::credential` (ADR-0086 owner) |
| `session-application-contract` | 3 | `awaken-time::utc` |
| `session-configuration-contract` | 3 | `provisioning-contract` (drop the `EnvironmentIdleRetention*` aliases) |
| `session-realization-contract` | 3 | configuration `resource::realization_lease_*` |
| `tenancy`, `worker-contract` | 2, 2 | `agent-contract`; `credential-contract` (drop the `WorkerCredential*` aliases) |
| `executable-agent-contract`, `provisioning-contract` | 1, 1 | `runtime-contract`; `sandbox-control` |

The `neutral_role!` macro stops using `__macro_support`: it names the owners
by absolute path (`::awaken_session_contract`, `::awaken_session_types`, …), and
each invoking crate declares the owners its arm uses. The three
`runtime-contract` paths to `ThreadUsage`, `ThreadUsageKey` and
`THREAD_USAGE_STATE_KEY` are reviewed SDK facade paths (ADR-0086, public-API
snapshot); they are removed only with the SDK SemVer release (plan item L).
When `contract-reexport` and `renamed-reexport` reach zero outside that
facade, both ratchets become a plain deny rule and lose their baseline (S2).
Re-exports of contract items by implementation crates (331 items, mostly
`awaken-dispatch` 95, `awaken-runtime` 34, `awaken-worker-registry` 28) are
outside V4; the Session ones disappear with steps 1–4.

## `RunIngress` name

`awaken-deployment-config::RunIngress { Direct, Durable }` decides whether a
foreground Run is delivered through the durable dispatch queue; "ingress" names
the retired run-ingress crates (ADR-0009), not that responsibility. Rename it
`ForegroundRunDelivery`, keep the variants and `is_durable()`, and rename the
field `ExecutionTopology::ingress` to `foreground_delivery`. The executor's
private `ForegroundRunDelivery { Direct(Arc<DirectAttemptDriver>), Durable }`
says who drives the attempt, so it becomes `ForegroundRunDriver`. TLA+
`RunIngress`/`RunIngressKernel` model the dispatch row protocol and keep their
names. awaken-cloud does not name either type.

Use sites (production): `crates/shared/awaken-deployment-config/src/execution_topology.rs`
21, 30, 159 and `lib.rs` 42, 578–581; `crates/shared/awaken-dispatch-execution/src/process_dispatch.rs`
278; `crates/server/awaken-session-executor/src/lib.rs` 139 (a cross-crate
re-export, removed), `host/dependencies.rs` 192, 532, `host/run.rs` 438,
`host/run/committed_terminal_observation.rs` 70, `host/session/dispatch_worker.rs`
20, 39, 102–109, `host/session_ctx.rs` 233–258. Tests: deployment-config
`lib.rs` 1064, 1082 and `execution_topology.rs` 189; dispatch-execution
`service.rs` 503, 509 and `process_dispatch/tests.rs` 229; session-run-delivery
`delivery.rs` 544 and `dispatch_lifecycle/tests.rs` 5, 115; session-executor
`coordination.rs` 799, `host/dispatch_reconciliation.rs` (7 sites) and
`host/tests/{close_edge_selection, environment_binding, interruption_selection,
managed_interrupt, run_boundary, terminal_disposal, terminal_recovery_selection}.rs`.
Documents: ADR-0088 (2026-10-24 amendment) and `docs/STATUS.md` P7.

## Order

Each step compiles on its own and lands as one commit after V0.

1. **Core content leaves the application contract.** `SessionEventAdmission`
   and its functions, `LifecycleFactDelivery` and `CompositeLifecycleFactDelivery`
   move to `awaken-session-contract`; `session_usage_from_thread_usage` becomes
   `SessionUsage::from_thread_usage` there. The application contract drops its
   Core dependency; `LifecycleFactNotifier` stays.
2. **Application contract keeps only ports.** Callers use `awaken_time::utc`
   directly; `SessionApplicationConfiguration`, `SessionApplicationBuildError`,
   `SessionExecutionPlacement`, `WorkDispatchFailure`,
   `WorkDispatchReconciliation`, `runtime_placement_for_topology` and
   `SessionParticipantProvenance` move to `awaken-session-application`, whose
   readers already import them from there.
3. **Live inbox driving port.** `LiveInboxApplication` and its values move to the
   application contract; `LiveInboxAccess` stays in execution.
   `protocol-awaken` drops its execution dependency.
4. **Realization merges into execution.** Private modules, one explicit public
   vocabulary (as Core did); `@formal` markers, the Kani proof
   `provisioning_defers_only_before_the_providers_deadline` and the owned tests
   move with the code; `TARGET_MAP` loses the realization row,
   `contract-crates` tightens to 29, the context-order entry is re-keyed.
5. **Execution contract re-exports to zero**, including `__macro_support` and
   `ToolPermissionDecision`.
6. **Remaining Platform re-exports**, one owner per commit: configuration,
   credential, dispatch, tenancy, worker, executable-agent, provisioning.
7. **Close.** The re-export ratchets become deny rules; each contract's crate
   documentation lists its callers and implementers from this table.

`RunIngress` is renamed in a separate commit at any point; it touches no
contract.

Target: **29 contract crates** (28 under `crates/contract`), down from 30. The
plan's estimate of about 26 assumed merges that would grow the closures above.

## Retired names

Each absorbing or narrowed crate gains `compile_fail` doctests, as
`awaken-session-contract` pins its retired crates:

| Crate | `compile_fail` |
|---|---|
| `session-execution-contract` | `use awaken_session_realization_contract::SessionRealizationControl;` `use awaken_session_execution_contract::LiveInboxApplication;` `use awaken_session_execution_contract::__macro_support::RunError;` `use awaken_session_execution_contract::ToolPermissionDecision;` `use awaken_session_execution_contract::SessionThreadTarget;` |
| `session-application-contract` | `use awaken_session_application_contract::SessionEventAdmission;` `…::LifecycleFactDelivery;` `…::session_usage_from_thread_usage;` `…::epoch_millis_to_rfc3339;` `…::SessionApplicationConfiguration;` |
| `session-configuration-contract` | `use awaken_session_configuration_contract::EnvironmentIdleRetentionMode;` |
| `credential-contract` | `use awaken_credential_contract::CredentialRef;` |
| `dispatch-contract` | `use awaken_dispatch_contract::RunClaim;` |
| `tenancy` | `use awaken_tenancy::ScopeId;` |
| `worker-contract` | `use awaken_worker_contract::WorkerCredentialRevision;` |
| `executable-agent-contract` | `use awaken_executable_agent_contract::ExecutableAgentSnapshot;` |
| `provisioning-contract` | `use awaken_provisioning_contract::SandboxControlServiceKind;` |
| `deployment-config` | `use awaken_deployment_config::RunIngress;` |

## awaken-cloud impact

Read from its tracked sources at `b735ddb3`. Its pin predates the Core merge;
the paths below are only those this design changes. They are recorded in a dated
ADR-0088 amendment with the step that changes them (ADR-0091 D9):

| Step | Path today | Path after | Files |
|---|---|---|---|
| 1 | `awaken_session_application_contract::LifecycleFactDelivery` | `awaken_session_contract::LifecycleFactDelivery` | `awaken-cloud-billing/src/session_runtime_metering.rs`, `awaken-cloud-management/src/main.rs` |
| 4 | `awaken_session_realization_contract::{SessionResourceMaterializationIntent, SessionResourceMaterializationTarget}` | `awaken_session_execution_contract::…` | `awaken-hosting-durable/src/worker_model_access.rs` |
| 6 | `awaken_credential_contract::{CredentialAccess, CredentialEnvelope, CredentialExecutionPolicy, CredentialMaterialSource, CredentialPurpose, CredentialRef, CredentialTarget, CredentialUsage, HttpEffectPlacement, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder, SealedCredentialEnvelopeRef}` | `awaken_agent_contract::credential::…` | 19 files in `awaken-cloud`, `awaken-cloud-console`, `awaken-cloud-credential-envelope`, `awaken-egress-gateway`, `awaken-provider-control`, `awaken-pro` |
| 6 | `awaken_tenancy::ScopeId` | `awaken_agent_contract::ScopeId` | 13 files in `awaken-cloud`, `awaken-cloud-console`, `awaken-cloud-management`, `awaken-hosted-run`, `awaken-hosting-durable`, `awaken-model-access`, `awaken-model-publication` |

Cloud names no `dispatch-contract`, `session-execution-contract`, `worker-contract`
alias or `RunIngress` path. Cloud crates that do not yet depend on
`awaken-agent-contract` add it in the same upgrade (V7).
