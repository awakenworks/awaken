# Construction-Time Injection and Typed Errors

Status: classification complete; (v) done, (iii) started, (i) designed; Family A blocked by a verified runtime cycle.
Principles: P5 (interface segregation, construction-time injection) and P6
(errors are values, classified once) of the architecture plan.

## Counting

Production Rust only (`_crate_boundary_workspace.production_rust`), at
`1b56f9aea0`:

| Pattern | Sites | Note |
|---|---:|---|
| `Weak<` | 53 | |
| `Option<&dyn` / `Option<&'a dyn` | 74 | the plan's 59 omitted 14 lifetime-named sites and 1 type split across lines |
| `Internal(<expr>.to_string())` | 13 | |
| `Option<Arc<dyn` (not in the plan's metric) | 620 | the construction-time form of an optional role; a guard that counts only `Option<&dyn` misses it |

A guard should match across line breaks: the line-based count misses
`host/capability/execution.rs`.

## Decision classes

| Class | Rule | Sites | Done |
|---|---|---:|---:|
| (i) cycle | replace with build order, an injected role port, or a split component | 31 | 0 |
| (ii) legitimate weak | observer, lifetime that must not keep its target alive, or cache; exempt with a reason | 22 | exempt |
| (iii) optional role | required at construction, or a closed enum naming each case | 59 | 4 |
| (iv) SDK public API | defer to item L (SDK SemVer) | 15 | deferred |
| (v) error flattening | typed variant that keeps its source, classified once | 13 | 13 |

## Typed errors (v)

- `awaken-sandbox-memoryd`: `FuseError::Io(std::io::Error)` and
  `FuseError::NonUtf8(FromUtf8Error)` replace stringified sources.
  `Internal(String)` keeps only fixed invariant messages. Display text, errno
  (`EIO`) and the `SandboxError::Backend { operation: "memory FUSE" }` mapping are
  unchanged.
- `awaken-session-application`: `SessionStateViolation { Aggregate(SessionMutationValidationError), Budget(ManagedListPriceError) }`
  is the typed source of `SessionMutationError::Invalid`,
  `SessionCreationError::Invalid` and `SessionActivityError::Invalid`. Each
  `Invalid` is classified exactly like the `Internal` it replaces (permanent,
  `session_application_invalid`, same text). Every adapter arm that handled
  `Internal(_)` handles `Invalid(_)` with it.
- The serialized `SessionRealizationControlFailure::Internal(String)` crosses the
  Worker boundary as class plus text, so `mutation.rs` renders the typed cause
  once there. This is the one remaining match of the pattern; it is listed in
  the exemptions below.

## Family A: Host and Session application

The executor is built first; the Session application is built over it; then
`session_dispatch_lifecycle` publishes `Weak` handles to the application's
`SessionAgentCoordination` and `SessionRunBackgroundApplication` roles into a
`OnceLock` (`SessionApplicationBindings`). Consumers upgrade per call. This is
late binding through a service locator, which P5 forbids.

Remedy, in build order: construct the two roles first, over the durable
Session repository and the dispatch queue. Both roles admit work into the queue
and need no executor. Pass them as `Arc` to `SessionExecutorBuilder`. The
`SessionApplications::{Local, Standalone, ClaimedWorker}` selection becomes a
constructor argument. Then delete `SessionApplicationConnections`,
`SessionApplicationLifetime` and `SessionApplicationBindings`. First verify that
neither role's implementation reaches the executor; if one does, split off the
part that does.

This touches the composition roots (`awaken-cli`, `awaken-worker`,
`awaken-coordinator`, `awaken-scenario-host`) and is not started in this batch.

**Verification (at `65252536c3`): blocked by a true runtime cycle.** Both roles
reach the executor, so build order alone cannot break the edge:

- Coordination: `SessionCoordination` (the `SessionAgentCoordination` impl)
  holds the executor's `ChildRunCoordination`, `SessionRuns.threads` and
  `CommittedReads` from `SessionExecutionRoles`, and `SessionRealizations`
  over `RunAssembly`. The executor `ChildRunCoordination` holds `SessionRuns`,
  `RunAssembly` and `RunCapabilities`, and `RunCapabilities` carries the
  binding. At runtime, `send_session_agent_message` launches a child Run
  through `ChildRunCoordination`. That Run's model gate
  (`ChildRunLauncher` → `coordination_endpoint`) calls
  `admit_session_model_request`, and its dispatch settlement
  (`HostDispatchSettlementObserver`) calls the boundary methods.
- Background Runs: `SessionRunApplication` (the
  `SessionRunBackgroundApplication` impl) holds `SessionRuns` as its
  `RunApplication`. Its `SessionRunAdmissions` holds the executor's
  `ChildRunCoordination` and `StreamingRuns`.
  `submit_session_run_background` → `activate_admitted_session_run` →
  `ChildRunCoordination::activate_session_run`. The BackgroundTask tool
  (`context/plan.rs`) and MCP background input (`mcp_host.rs`) call it from
  inside a Run.
- `session_execution_roles()` passes `RunCapabilities` itself, as
  `capability`, to the application. Injecting the roles into the claimed-Run
  resolver also closes a strong cycle: `ProcessDispatch` keeps the pool and its
  resolver in a `OnceCell`, and `SessionRuns` holds `ProcessDispatch`.

Two remedies break the cycle, and both need a decision:

- The application activates and reads Runs only through the dispatch queue and
  committed reads, so it no longer holds `SessionRuns`,
  `ChildRunCoordination`, `RunAssembly` or `RunCapabilities`. This changes the
  `SessionExecutionRoles` contract.
- The executor splits into a Session realization part, which the application
  consumes, and a Run execution part, which is built after the application and
  receives the roles as `Arc`.

Either remedy touches V0's run paths and the Family B registry.

## Family B: installed-Session registry

`InstalledSessions` owns each Session's context. That context holds attempt
executors, terminal observers, tools and the deferred Sandbox executor, and
those objects reach the whole registry, or a process service that holds it
(`RealizationExecutor`, `ArtifactHarvester`, `RunAssembly`), through `Weak`.

Remedy, by splitting the component:

1. Move the per-Session state these objects read into one `Arc` cell owned by
   the registry entry: inputs and tools, baseline, dispatch claim, deferred
   executor and foreground record. Inject that cell at construction instead of
   the registry.
2. Process services that read the registry are owned only by the composition
   root (`ExecutorComponents`): committed reads, advisor models and Memory
   terminal observers. Registry entries never hold them.
3. Where a per-Session object needs a process service, split that service into
   a registry-free part, injected as `Arc`, and a registry-bound part that only
   the composition root calls. For example, the `ArtifactHarvester` publish
   logic is separate from its realization lookup.

This changes the registry's lock layout, so it is sequenced after the lock
lane (V2) lands. It is not started in this batch.

## Open decisions

- Resolved 2026-10-31 (user decision): `ManagedListPriceError::Unavailable` is
  an outage, so the one conversion `SessionStateViolation` →
  `SessionMutationError` classifies it as retryable `Unavailable`; creation and
  activity reuse that conversion. Every other violation stays permanent
  (`a_list_price_outage_is_the_one_retryable_state_violation`).
- Family A: choose between the two cycle remedies recorded under Family A.
- Families A and B change composition roots and lock layout. Each needs its own
  slice, sequenced after V0 and V2.

## Exemptions (for the V1 guard)

A guard may allow exactly these production sites. Match on file and the
quoted item; line numbers drift.

| File | Item | Reason |
|---|---|---|
| `crates/runtime/awaken-ext-mcp/src/transport.rs` | `Weak<` ×2 | revocation: a transport admitted under a drained generation fence must fail, not keep the fence alive |
| `crates/runtime/awaken-runtime/src/runtime.rs` | `Weak<` ×1 | cache: a per-Thread execution gate is collected when no guard holds it |
| `crates/server/awaken-protocol-managed/src/lifecycle_delivery.rs` | `Weak<` ×1 | observer: the outbox supervisor must not own the HTTP adapter it wakes |
| `crates/server/awaken-session-executor/src/mcp_host.rs` | `Weak<` ×2 | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `crates/server/awaken-session-executor/src/run_assembly/mcp_attachment.rs` | `Weak<` ×2 | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `crates/server/awaken-session-executor/src/run_exec.rs` | `Weak<` ×1 | ownership verifier: an attempt whose Session context is gone is no longer current (fail closed) |
| `crates/server/awaken-session-mcp/src/mcp/attachment.rs` | `Weak<` ×1 | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `crates/server/awaken-session-mcp/src/mcp/input.rs` | `Weak<` ×2 | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `crates/server/awaken-session-mcp/src/mcp/sessions.rs` | `Weak<` ×3 | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `crates/server/awaken-session-mcp/src/mcp_relay.rs` | `Weak<` ×1 | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `crates/server/awaken-session-realization/src/execution_authority.rs` | `Weak<` ×2 | captured authorization: a later effect fails closed once the installed Sessions are gone |
| `crates/server/awaken-session-realization/src/session_realizations.rs` | `Weak<` ×1 | the one constructor of the captured authorization handle above |
| `crates/server/awaken-session-run-delivery/src/background.rs` | `Weak<` ×1 | interning cache: one shared admission per (thread, task) while some Run holds it |
| `crates/server/awaken-session-run-delivery/src/completion.rs` | `Weak<` ×1 | drop guard: removing a waiter must not keep the completion registry alive |
| `crates/worker/awaken-sandbox-local/src/provider/execution.rs` | `Weak<` ×1 | observer: the execution scope signals live child processes it does not own |
| `crates/server/awaken-session-application/src/mutation.rs` | `Internal(violation.to_string())` | the serialized realization-control failure carries class plus text across the Worker boundary |
| (SDK) `crates/contract/awaken-runtime-contract`, `crates/contract/awaken-tool-contract`, `crates/runtime/awaken-runtime`, `crates/runtime/awaken-ext-mcp`, and `crates/server/awaken-session-executor/src/delegate.rs` (the SDK trait impl) | `Option<&dyn` | deferred to item L; remove from the allow-list when L lands |

## Sites

### `Weak<`

| Site (`crates/…`, line at `1b56f9aea0`) | Class | Status | Decision |
|---|---|---|---|
| `runtime/awaken-ext-mcp/src/transport.rs:172` | (ii) | exempt | revocation: a transport admitted under a drained generation fence must fail, not keep the fence alive |
| `runtime/awaken-ext-mcp/src/transport.rs:178` | (ii) | exempt | revocation: a cloned notification handler stops delivering once its generation fence is gone |
| `runtime/awaken-runtime/src/runtime.rs:160` | (ii) | exempt | cache: a per-Thread execution gate is collected when no guard holds it |
| `server/awaken-protocol-managed/src/lifecycle_delivery.rs:20` | (ii) | exempt | observer: the outbox supervisor must not own the HTTP adapter it wakes |
| `server/awaken-session-executor/src/application/attempt_context.rs:24` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/application/attempt_context.rs:33` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/background_task.rs:118` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/background_task.rs:186` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/background_task.rs:200` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/coordination.rs:50` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/coordination.rs:53` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/coordination.rs:92` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/coordination.rs:411` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/host/capability/resources.rs:123` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/host/dependencies/session_application.rs:59` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/host/dependencies/session_application.rs:64` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/host/session/context/plan.rs:27` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/lazy_sandbox.rs:13` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/lazy_sandbox.rs:14` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/lazy_sandbox.rs:54` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/lazy_sandbox.rs:55` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/mcp_host.rs:339` | (ii) | exempt | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `server/awaken-session-executor/src/mcp_host.rs:352` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-executor/src/memory/terminal_observer.rs:48` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/memory/terminal_observer.rs:56` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/run_assembly/mcp_attachment.rs:58` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-executor/src/run_assembly/mcp_attachment.rs:60` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-executor/src/run_exec.rs:30` | (ii) | exempt | ownership verifier: an attempt whose Session context is gone is no longer current (fail closed) |
| `server/awaken-session-executor/src/run_exec.rs:79` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/run_exec.rs:86` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/session_effects.rs:25` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/thread_reads.rs:29` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/thread_reads.rs:135` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/thread_reads.rs:136` | (i) | planned | Family B: inject the per-Session cell, not the registry ([below](#family-b-installed-session-registry)) |
| `server/awaken-session-mcp/src/mcp/attachment.rs:52` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp/input.rs:36` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-mcp/src/mcp/input.rs:37` | (ii) | exempt | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `server/awaken-session-mcp/src/mcp/input.rs:41` | (ii) | exempt | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `server/awaken-session-mcp/src/mcp/input.rs:42` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-mcp/src/mcp/sessions.rs:324` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp/sessions.rs:338` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp/sessions.rs:350` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp_relay.rs:207` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-realization/src/execution_authority.rs:32` | (ii) | exempt | captured authorization: a later effect fails closed once the installed Sessions are gone |
| `server/awaken-session-realization/src/execution_authority.rs:45` | (ii) | exempt | captured authorization: an execution target denies once its Environment is dropped |
| `server/awaken-session-realization/src/session_realizations.rs:359` | (ii) | exempt | the one constructor of the captured authorization handle above |
| `server/awaken-session-run-delivery/src/background.rs:29` | (ii) | exempt | interning cache: one shared admission per (thread, task) while some Run holds it |
| `server/awaken-session-run-delivery/src/completion.rs:200` | (ii) | exempt | drop guard: removing a waiter must not keep the completion registry alive |
| `server/awaken-session-run-delivery/src/session_applications.rs:57` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-run-delivery/src/session_applications.rs:58` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-run-delivery/src/session_applications.rs:99` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `server/awaken-session-run-delivery/src/session_applications.rs:103` | (i) | planned | Family A: inject the application roles at construction ([below](#family-a-host-and-session-application)) |
| `worker/awaken-sandbox-local/src/provider/execution.rs:102` | (ii) | exempt | observer: the execution scope signals live child processes it does not own |

### `Option<&dyn …>`

| Site (`crates/…`, line at `1b56f9aea0`) | Class | Status | Decision |
|---|---|---|---|
| `bin/awaken-process-startup/src/service.rs:218` | (iii) | planned | closed enum of the embedded-Worker topology (Colocated(prep) or Disabled) |
| `bin/awaken-worker/src/credential_liveness.rs:137` | (iii) | planned | required at construction: the probe owns its observation sources; an absent source is a typed empty source |
| `bin/awaken-worker/src/credential_liveness.rs:138` | (iii) | planned | same as above |
| `bin/awaken-worker/src/credential_liveness.rs:223` | (iii) | planned | same as above |
| `bin/awaken-worker/src/credential_liveness.rs:232` | (iii) | planned | same as above |
| `bin/awaken-worker/src/manifest.rs:196` | (iii) | planned | closed enum: brokered ACP model access Supported(&dyn ..) or Unsupported |
| `contract/awaken-dispatch-contract/src/commit_epoch.rs:186` | (iii) | planned | closed enum: recovery Required(&dyn RunRecoverySource) or NotApplicable |
| `contract/awaken-provisioning-contract/src/sandbox/effect.rs:32` | (iii) | planned | closed: carry SandboxEffect (Unattached or Fenced) from AuthorizedSessionEnvironmentEffect instead of a fence and an authority option |
| `contract/awaken-provisioning-contract/src/sandbox/runtime.rs:904` | (iii) | planned | closed: the provider/exporter declares its capability set at construction (enum of role sets), not a per-call query |
| `contract/awaken-runtime-contract/src/auxiliary_agents.rs:67` | (iv) | deferred-L | pub fn freeze: absent publication source |
| `contract/awaken-runtime-contract/src/delegation.rs:351` | (iv) | deferred-L | trait default: cancellation capability query |
| `contract/awaken-runtime-contract/src/execution.rs:66` | (iv) | deferred-L | pub fn verify_attempt_ownership: direct callers have no verifier |
| `contract/awaken-runtime-contract/src/permission/run_capability.rs:41` | (iv) | deferred-L | pub fn effective: absent policy |
| `contract/awaken-runtime-contract/src/permission.rs:328` | (iv) | deferred-L | pub fn evaluate_run_policy: absent policy |
| `contract/awaken-runtime-contract/src/resolver.rs:74` | (iv) | deferred-L | pub fn resolve_delegate_snapshot: absent publication source |
| `contract/awaken-runtime-contract/src/resolver.rs:105` | (iv) | deferred-L | pub fn freeze_delegation_publications: absent publication source |
| `contract/awaken-session-configuration-contract/src/resource/binding_checks.rs:50` | (iii) | planned | closed enum per verifier: Configured(&dyn ..) or Unconfigured, whose refusal rows (B1-B6, P1-P7) already exist |
| `contract/awaken-session-configuration-contract/src/resource/binding_checks.rs:118` | (iii) | planned | closed enum per verifier: Configured(&dyn ..) or Unconfigured, whose refusal rows (B1-B6, P1-P7) already exist |
| `contract/awaken-session-configuration-contract/src/resource/binding_checks.rs:119` | (iii) | planned | closed enum per verifier: Configured(&dyn ..) or Unconfigured, whose refusal rows (B1-B6, P1-P7) already exist |
| `contract/awaken-session-configuration-contract/src/resource/inputs.rs:890` | (iii) | planned | required at construction: the Resource registry/inventory is a deployment role; a deployment without Resources selects a refusing registry type, not None |
| `contract/awaken-session-configuration-contract/src/skill_delivery.rs:93` | (iii) | planned | closed enum per verifier: Configured(&dyn ..) or Unconfigured, whose refusal rows (B1-B6, P1-P7) already exist |
| `contract/awaken-session-configuration-contract/src/skill_delivery.rs:135` | (iii) | planned | closed enum per verifier: Configured(&dyn ..) or Unconfigured, whose refusal rows (B1-B6, P1-P7) already exist |
| `contract/awaken-session-environment-contract/src/tool_export.rs:27` | (iii) | planned | closed: the provider/exporter declares its capability set at construction (enum of role sets), not a per-call query |
| `contract/awaken-session-realization-contract/src/session_realization/canonical_publication.rs:82` | (iii) | planned | required at construction: every deployment selects a publication source; Some(..) only at the SDK call |
| `contract/awaken-tool-contract/src/tool.rs:914` | (iv) | deferred-L | trait default: DurableTaskTool capability query |
| `contract/awaken-tool-contract/src/tool.rs:1219` | (iv) | deferred-L | trait default: DurableTaskExecutor capability query |
| `contract/awaken-tool-contract/src/tool.rs:1372` | (iv) | deferred-L | SDK impl of the query above |
| `control/awaken-credential-vault/src/repo/credential_operations.rs:1188` | (iii) | done | closed enum SourceMaterialMutation {MetadataOnly, Stored{store, materials}}: a material write without its store is unrepresentable |
| `control/awaken-environment-application/src/lib.rs:348` | (iii) | planned | required at construction: the Environment application always holds its policy store |
| `runtime/awaken-ext-goal/src/controller.rs:148` | (iii) | planned | split Controller into a preparation controller and an executing controller (private field of an SDK crate) |
| `runtime/awaken-ext-mcp/src/tool.rs:170` | (iv) | deferred-L | SDK impl of the DurableTaskTool query |
| `runtime/awaken-runtime/src/engine/finalize.rs:89` | (iii) | planned | closed enum: finish with a committed reader or the context's own view (private; V0 owns the engine this batch) |
| `runtime/awaken-runtime/src/engine/inference.rs:245` | (iv) | deferred-L | forwards RuntimeRunContext ownership to verify_attempt_ownership |
| `runtime/awaken-runtime/src/engine/inference.rs:399` | (iv) | deferred-L | forwards RuntimeRunContext ownership to verify_attempt_ownership |
| `runtime/awaken-runtime/src/engine/tool_execution.rs:945` | (iv) | deferred-L | SDK impl of the DurableTaskExecutor query |
| `runtime/awaken-runtime/src/engine/tool_execution.rs:1081` | (iii) | planned | closed: ToolExecutionTarget already names Sandbox; carry the executor in that variant (private; V0 owns the engine this batch) |
| `server/awaken-coordinator/src/worker_observation_boundary.rs:265` | (iii) | planned | required at construction: the bootstrap closer holds the binding source |
| `server/awaken-inference-endpoint/src/brokered_inference.rs:781` | (iii) | done | closed enum ResponseDelivery {Whole, Streamed(sink)} |
| `server/awaken-protocol-mcp/src/session_export.rs:98` | (iii) | planned | closed: the provider/exporter declares its capability set at construction (enum of role sets), not a per-call query |
| `server/awaken-run-executor-acp/src/permission.rs:84` | (iii) | planned | closed enum: narrowing Narrowed(&dyn policy) or Unrestricted |
| `server/awaken-run-executor-acp/src/permission.rs:214` | (iii) | planned | required at construction: the ACP executor selects its grant observer once (builder role) |
| `server/awaken-run-executor-acp/src/permission.rs:252` | (iii) | planned | same as above |
| `server/awaken-run-executor-acp/src/permission.rs:266` | (iii) | planned | same as above |
| `server/awaken-run-executor-acp/src/tool_continuation.rs:55` | (iii) | planned | required at construction: the executor's owned tool continuation role |
| `server/awaken-session-application/src/credentials.rs:69` | (iii) | planned | required at construction: SessionCredentials holds its credential source |
| `server/awaken-session-application/src/credentials.rs:74` | (iii) | planned | required at construction: SessionCredentials holds its material ingress |
| `server/awaken-session-application/src/realization.rs:276` | (iii) | planned | build order: placement is a OnceLock set after construction; inject it at construction |
| `server/awaken-session-application/src/resource_lowering.rs:79` | (iii) | planned | required at construction: the Resource registry/inventory is a deployment role; a deployment without Resources selects a refusing registry type, not None |
| `server/awaken-session-application/src/resource_lowering.rs:100` | (iii) | planned | required at construction: the Resource registry/inventory is a deployment role; a deployment without Resources selects a refusing registry type, not None |
| `server/awaken-session-application/src/resource_lowering.rs:110` | (iii) | planned | required at construction: the Resource registry/inventory is a deployment role; a deployment without Resources selects a refusing registry type, not None |
| `server/awaken-session-application/src/session_input.rs:343` | (iii) | planned | required at construction: the Resource registry/inventory is a deployment role; a deployment without Resources selects a refusing registry type, not None |
| `server/awaken-session-application/src/session_input.rs:366` | (iii) | planned | required at construction: the Resource registry/inventory is a deployment role; a deployment without Resources selects a refusing registry type, not None |
| `server/awaken-session-application/src/terminal_cleanup/claims.rs:82` | (iii) | planned | required at construction: terminal cleanup holds the Worker incarnation source |
| `server/awaken-session-executor/src/application/realization_renewal.rs:195` | (iii) | planned | closed enum: renewal Local or Controlled(&dyn SessionRealizationControl) |
| `server/awaken-session-executor/src/application/realization_renewal.rs:237` | (iii) | planned | same as above |
| `server/awaken-session-executor/src/delegate.rs:242` | (iii) | planned | required at construction: every deployment selects a publication source; Some(..) only at the SDK call |
| `server/awaken-session-executor/src/delegate.rs:599` | (iv) | deferred-L | platform impl of the SDK cancellation query; follows its signature |
| `server/awaken-session-executor/src/outcome_controller.rs:114` | (iii) | done | executor moved into OutcomeCommand::Resume; Prepare never carries one |
| `server/awaken-session-mcp/src/mcp/attachment.rs:100` | (iii) | planned | required at construction: the MCP attachment adapter owns its refresh factory |
| `server/awaken-session-mcp/src/mcp/attachment.rs:101` | (iii) | planned | closed enum: platform egress Provisioned(&dyn ..) or Direct |
| `server/awaken-session-realization/src/environment_holding/realization.rs:106` | (iii) | planned | closed: carry SandboxEffect (Unattached or Fenced) from AuthorizedSessionEnvironmentEffect instead of a fence and an authority option |
| `server/awaken-session-realization/src/test_support.rs:57` | (iii) | planned | closed: carry SandboxEffect (Unattached or Fenced) from AuthorizedSessionEnvironmentEffect instead of a fence and an authority option |
| `server/awaken-session-run-delivery/src/auxiliary_snapshot.rs:41` | (iii) | planned | required at construction: every deployment selects a publication source; Some(..) only at the SDK call |
| `server/awaken-session-run-delivery/src/child_dispatch.rs:23` | (iii) | planned | required at construction: every deployment selects a publication source; Some(..) only at the SDK call |
| `server/awaken-session-run-delivery/src/delivery/outcomes.rs:116` | (iii) | done | executor moved into OutcomeCommand::Resume; Prepare never carries one |
| `server/awaken-session-run-delivery/src/session_projections.rs:424` | (iii) | planned | required at construction: every deployment selects a publication source; Some(..) only at the SDK call |
| `server/awaken-session-run-delivery/src/session_run_reservation.rs:49` | (iii) | planned | closed enum: Skills Offered(&dyn SkillRegistry) or NotOffered |
| `server/awaken-session-run-delivery/src/thread_coordination/terminal_quiescence.rs:60` | (iii) | planned | closed enum: root attempt Resident(&dyn ..) or Elsewhere |
| `server/awaken-worker-transport-security/src/lib.rs:146` | (iii) | planned | required at construction: registered routers always hold the Worker directory; the local compatibility router is a separate type |
| `server/awaken-session-executor/src/host/capability/execution.rs:17` | (iii) | planned | closed enum: ACP backend Installed(factory) or Projection(&'static policy) for a Coordinator, instead of two independent options |
| `shared/awaken-dispatch-execution/src/session_dispatch.rs:73` | (iii) | planned | required at construction: every deployment selects a publication source; Some(..) only at the SDK call |
| `shared/awaken-dispatch-execution/src/session_dispatch.rs:110` | (iii) | planned | required at construction: every deployment selects a publication source; Some(..) only at the SDK call |
| `worker/awaken-sandbox-local/src/namespace/memory_mount.rs:195` | (iii) | planned | closed: carry SandboxEffect (Unattached or Fenced) from AuthorizedSessionEnvironmentEffect instead of a fence and an authority option |
| `worker/awaken-sandbox-local/src/provider.rs:1619` | (iii) | planned | closed: the provider/exporter declares its capability set at construction (enum of role sets), not a per-call query |

### `Internal(x.to_string())`

| Site (`crates/…`, line at `1b56f9aea0`) | Class | Status | Decision |
|---|---|---|---|
| `server/awaken-session-application/src/creation.rs:128` | (v) | done | SessionCreationError::Invalid(SessionStateViolation::Aggregate) |
| `server/awaken-session-application/src/creation.rs:139` | (v) | done | SessionCreationError::Invalid(SessionStateViolation::Aggregate) |
| `server/awaken-session-application/src/lifecycle.rs:308` | (v) | done | SessionMutationError::Invalid(SessionStateViolation::Budget) |
| `server/awaken-session-application/src/lifecycle.rs:319` | (v) | done | SessionMutationError::Invalid(SessionStateViolation::Budget) |
| `server/awaken-session-application/src/lifecycle.rs:326` | (v) | done | SessionMutationError::Invalid(SessionStateViolation::Budget) |
| `server/awaken-session-application/src/mutation.rs:162` | (v) | done | unit variant: fixed text; no source exists |
| `server/awaken-session-application/src/mutation.rs:497` | (v) | done | SessionMutationError::Invalid(SessionStateViolation::Aggregate) |
| `worker/awaken-sandbox-memoryd/src/copy.rs:208` | (v) | done | FuseError::Io(std::io::Error) |
| `worker/awaken-sandbox-memoryd/src/fuse.rs:305` | (v) | done | FuseError::Io(std::io::Error) |
| `worker/awaken-sandbox-memoryd/src/fuse.rs:477` | (v) | done | FuseError::Io(std::io::Error) |
| `worker/awaken-sandbox-memoryd/src/fuse.rs:876` | (v) | done | FuseError::NonUtf8(FromUtf8Error) |
| `worker/awaken-sandbox-memoryd/src/fuse.rs:894` | (v) | done | FuseError::NonUtf8(FromUtf8Error) |
| `worker/awaken-sandbox-memoryd/src/lib.rs:85` | (v) | done | FuseError::NonUtf8(FromUtf8Error) |
