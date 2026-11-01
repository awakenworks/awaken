# Construction-Time Injection and Typed Errors

Status: classification complete; (v) done, (iv) decided by item L, (iii) done except two test-fixture sites (rule below), (i) designed; Family A converged into one typed late binding (remedy (c)); its removal (remedy (a)) is designed in [remote-task-control.md](remote-task-control.md) and waits for the `LocalWorker` placement to retire. Family B classified: one redundant edge removed, containment edges exempt with a removal condition.
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

Current `Weak<` count: `python3 scripts/ci/principle_status.py` (33 after
Family B, from 41 before it).

## Decision classes

| Class | Rule | Sites | Done |
|---|---|---:|---:|
| (i) cycle | replace with build order, an injected role port, or a split component | 31 | Family A: 14 converged (c); Family B: 6 done, 4 exempt (containment), 7 moved to (ii) |
| (ii) legitimate weak | observer, lifetime that must not keep its target alive, or cache; exempt with a reason | 22 + 7 | exempt (Family B: 2 of the 7 done, a duplicate spelling) |
| (iii) optional role | required at construction, or a closed enum naming each case; a feature left off stays optional (exempt) | 60 | 21 done, 37 exempt, 2 planned |
| (iv) SDK public API | decided by item L: kept as reviewed SDK shapes ([SDK versioning](sdk-versioning.md#optional-roles-in-the-sdk-api)) | 18 | 15 kept (L); 3 found by the widened count await L |
| (v) error flattening | typed variant that keeps its source, classified once | 13 | 13 |

## Optional roles (iii): the rule applied

User decision 2026-10-31: `Option<Arc<dyn ..>>` is information, not a
violation, and only roles a deployment selects change. Each `Option<&dyn ..>`
site was decided by what `None` means:

1. `None` never occurs in a production composition: the role is required at
   construction; tests share one fixture constructor.
2. `None` selects a different mode (not only a refusal or an empty result): a
   closed enum names each mode, with a decision-table test.
3. `None` means the deployment leaves a feature off, and the effect is an
   existing refusal or an empty result: the dependency stays optional, and its
   borrowed form is exempt. The same holds for capability queries (a
   property of the backend, not a selected role) and for the
   `std::error::Error::source` signature.

Counts (`python3 scripts/ci/principle_status.py`, P5 optional roles): 76 at
`504872e23d`, 59 after this batch: 18 deferred to L, 39 exempt (37 feature-off or
capability-query sites and 2 `Error::source` signatures), 2 planned.

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

**Decision (user, 2026-10-31).** Remedy (a), the first one above, is the final
design and lands with package G (plan v5, V9). Until then, remedy (c) converges
the late binding into one typed value.

**Remedy (a), designed.** [remote-task-control.md](remote-task-control.md)
(Q1-Q3) makes it concrete: the `RegisteredWorker` placement already builds the
application over `WorkerPlacedSessions` (Dispatch queue, committed reads,
frozen projections) with no executor. Remedy (a) is therefore the retirement
of the `LocalWorker` placement: the co-located executor becomes an in-process
registered Worker. Package G is its prerequisite, because a claimed Worker
must run BackgroundTask before the co-located path can become one. The removal
condition of `SessionApplicationBinding` is that no `LocalWorker` placement
remains.

**Remedy (c), done.** `SessionApplicationBinding`
(`awaken-session-execution-contract`, `application_binding.rs`) is the one late
binding from the executor to the two application roles it calls back,
`SessionAgentCoordination` and `SessionRunBackgroundApplication`. It lives in
the execution contract because both the executor and `awaken-session-mcp`
consume it.

- The executor creates one binding in `SessionExecutorBuilder::build` and gives
  a clone to `RunCapabilities`. Every consumer receives that binding at
  construction: the coordination endpoint and its settlement observer, the
  BackgroundTask terminal observer, the BackgroundTask context plan and the MCP
  input handler.
- The composition root completes it in one place, `session_dispatch_lifecycle`
  (also behind `WorkerRoles::of`), right after it builds the application:
  `awaken-cli` binds `Local`; `awaken-worker` binds `Standalone` or
  `ClaimedWorker`; the scenario-host and executor tests use the same call.
  `awaken-process-startup` `registered_execution` and `awaken-coordinator` build
  no executor, so they hold no binding.
- States: unbound refuses every use with `Unbound`; `bind` succeeds once and
  returns the `SessionApplicationLifetime` that owns the roles; a second `bind`
  is `AlreadyBound` and changes nothing; a dropped role is
  `RequiredApplicationUnavailable` and never changes the bound mode; a
  `Standalone` or `ClaimedWorker` binding has `NoLocalApplication`. Each
  consumer maps a refusal exactly as it mapped a failed `Weak::upgrade` before.
- Decision table: `binding_refuses_unbound_use_and_a_second_bind_and_keeps_mode_after_drop`
  and the ownership tests next to it in `application_binding/tests.rs`.
- Removed: `SessionApplicationBindings` (executor), `SessionApplicationConnections`
  (run delivery) and the scattered `Weak` handles. The 14 Family A `Weak<` sites
  become the binding's 2, and the one `OnceLock` moves into it.

## Family B: installed-Session registry

`InstalledSessions` owns each Session's context. That context holds attempt
executors, terminal observers, tools and the deferred Sandbox executor, and
those objects reach the whole registry, or a process service that holds it
(`RealizationExecutor`, `ArtifactHarvester`, `RunAssembly`), through `Weak`.

**Classification (2026-10-31).** The 17 sites are 9 holders. Each holder has
one of two kinds of edge:

- **Containment (a true cycle).** The holder lives in an installed Session's
  run layer (`SessionRunBinding.context`, whose `SessionCtx` holds the attempt
  executor chain, the tools and the claimed Worker, or
  `SessionRunBinding.deferred_executor`) and reads that same Session, or calls
  a service that owns the registry, on every call:
  `SessionContextAttemptExecutor` (frozen tools, baseline and staged prompts),
  `ArtifactHarvestAttemptExecutor` (through `ArtifactHarvester`, which reads
  the Session's claim and Environment), `CoordinationEndpoint::Remote` (the
  Session's current dispatch claim) and `DeferredSandboxExecutor` (realizes
  the Environment on the first Sandbox tool). Each refuses once the owner is
  gone; the `lifetime_tests` of `run_exec` and `coordination` pin those rows.
- **Retained role (a legitimate weak edge).** The holder is a process role
  that the composition hands to something that can outlive it, so it must not
  keep Session state alive: `InstalledCommitSource` and `ResolvedAdvisorModels`
  (the Worker recovery source;
  `retained_worker_recovery_releases_original_installed_sessions_and_idle_context`),
  `MemoryTerminalObservers` (the Coordinator's Memory settlement observer;
  `retained_memory_settlement_observes_frozen_child_after_executor_release`)
  and `RunMcpEffects` (a configured MCP realizer;
  `retained_configured_mcp_effects_release_unrelated_session_owners`).

**Done.**

1. `DeferredSandboxExecutor` held two back-edges, to the registry and to the
   realization executor. The first was redundant: the realization executor's
   `sessions()` view is the same registry under the same lock. It now holds
   only the realization executor and reads its pinned Skill through that view
   (`read_pinned_skill` takes a `SessionRealizationAccess`). Refusal texts are
   unchanged. Decision table: `lazy_sandbox/tests.rs`
   (`deferred_executor_reads_its_session_through_its_one_owner`,
   `deferred_executor_refuses_once_its_owner_is_released`).
2. Every holder's constructor takes its owner as `&Arc` and keeps the one weak
   edge itself, so no caller can hand it an unrelated or already dead `Weak`;
   `CoordinationEndpoint::current_claim` takes the upgraded registry. This
   removes the duplicate `Weak<` parameter spellings, not an edge.

**Withdrawn remedy.** The per-Session `Arc` cell planned here does not remove a
containment edge. The cell would be owned by the registry entry and would hold
the run layer, whose `SessionCtx` holds the reader: a strong reference from the
reader to the cell is the same cycle one level down, and a weak one keeps the
`Weak<`. Moving only the inputs and the claim into a separate cell would split
one atomic mutation (the realization close and the run controls it selects are
taken under the registry's one `InvariantLock`, which the `@formal` entrypoints
of `installed_session.rs` rely on), and a reader would keep the cell of a
Session that was removed and installed again, where today it reads the current
entry by id. Remedies 2 and 3 do not apply either: the retained roles above
already live outside the registry, and `ArtifactHarvester` needs the Session's
Environment for both its claim and its publish. V2's lock layout is unchanged.

**Removal condition for the containment edges.** The Session context cache
(`SessionRunBinding.context` and `deferred_executor`) is no longer owned by the
installed-Session entry: a resident-context cache outside the registry, whose
attempts receive their Session reads (frozen inputs, staged prompts, claim)
with the activation and the dispatch. That moves the interruption selection
out of the registry mutation, so it needs its own formal review; it is not
scheduled.

`RunCapabilities::session_run_projection_callbacks` (`run_capabilities.rs`)
belongs here, not to Family A. The two admission callbacks it gives the
Session application read the registry through `Arc::downgrade` of
`InstalledSessions`, so retaining a callback cannot keep the registry alive.
This is a read edge from the application into the registry, not a call back
into the application, and the `Weak<` count does not see it because it never
spells the type. It is a retained role (class (ii)):
`retained_session_run_projection_callbacks_release_the_original_owners_and_idle_context`
pins it. Replacing it with committed reads would change what admission reads
(the process-local frozen projection, not committed state), so that remedy is
withdrawn.

The `Weak<` sites in `awaken-session-mcp` and `awaken-session-run-delivery`
are not Family B: they hold the MCP root authority, the MCP input host, an
admission interning cache and the completion registry, never the
installed-Session registry. Their exemptions below stand.

## Open decisions

- Resolved 2026-10-31 (user decision): `ManagedListPriceError::Unavailable` is
  an outage, so the one conversion `SessionStateViolation` →
  `SessionMutationError` classifies it as retryable `Unavailable`; creation and
  activity reuse that conversion. Every other violation stays permanent
  (`a_list_price_outage_is_the_one_retryable_state_violation`).
- Resolved 2026-10-31 (user decision): Family A takes remedy (a) with package G
  and, until then, remedy (c), one typed binding (done).
- Resolved 2026-10-31: Family B keeps the registry's lock layout. Its
  containment edges are exempt until the Session context cache leaves the
  installed-Session entry ([above](#family-b-installed-session-registry)).

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
| `crates/server/awaken-session-executor/src/run_exec.rs` | `Weak<` ×2 | ownership verifier: an attempt whose Session context is gone is no longer current (fail closed); and containment: the artifact-harvest decorator lives in the Session context and harvests through the `ArtifactHarvester` that reads the registry. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `crates/server/awaken-session-executor/src/application/attempt_context.rs` | `Weak<` ×1 | containment: the Session-context attempt executor lives in the Session context and projects that Session's frozen tools, baseline and staged prompts per attempt. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `crates/server/awaken-session-executor/src/coordination.rs` | `Weak<` ×1 | containment: the remote coordination endpoint is embedded in Session contexts and reads the Session's current dispatch claim per call. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `crates/server/awaken-session-executor/src/lazy_sandbox.rs` | `Weak<` ×1 | containment: the deferred Sandbox executor is stored in the Session's run layer and realizes the Environment through the realization executor that owns the registry. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `crates/server/awaken-session-executor/src/memory/terminal_observer.rs` | `Weak<` ×1 | retained role: the Coordinator's Memory settlement observer outlives the executor and must not keep resident Sessions alive |
| `crates/server/awaken-session-executor/src/session_effects.rs` | `Weak<` ×1 | retained role: a configured MCP realizer must not keep unrelated local Session state alive |
| `crates/server/awaken-session-executor/src/thread_reads.rs` | `Weak<` ×3 | retained reader: the Worker recovery source and its advisor labels outlive the composition and must not keep installed Sessions or the realization executor alive |
| `crates/server/awaken-session-mcp/src/mcp/attachment.rs` | `Weak<` ×1 | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `crates/server/awaken-session-mcp/src/mcp/input.rs` | `Weak<` ×2 | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `crates/server/awaken-session-mcp/src/mcp/sessions.rs` | `Weak<` ×3 | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `crates/server/awaken-session-mcp/src/mcp_relay.rs` | `Weak<` ×1 | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `crates/server/awaken-session-realization/src/execution_authority.rs` | `Weak<` ×2 | captured authorization: a later effect fails closed once the installed Sessions are gone |
| `crates/server/awaken-session-realization/src/session_realizations.rs` | `Weak<` ×1 | the one constructor of the captured authorization handle above |
| `crates/contract/awaken-session-execution-contract/src/application_binding.rs` | `SessionApplicationBinding`: `Weak<` ×2 and its one `OnceLock` | the one late binding of Family A: the executor calls back into the application that launches Runs through it, so the composition root completes it after building the application, and it must not keep that application alive. Remove when the `LocalWorker` placement is retired (remedy (a), [remote-task-control.md](remote-task-control.md) Q3): the application then starts Runs only through the dispatch queue and reads only committed state |
| `crates/server/awaken-session-run-delivery/src/background.rs` | `Weak<` ×1 | interning cache: one shared admission per (thread, task) while some Run holds it |
| `crates/server/awaken-session-run-delivery/src/completion.rs` | `Weak<` ×1 | drop guard: removing a waiter must not keep the completion registry alive |
| `crates/worker/awaken-sandbox-local/src/provider/execution.rs` | `Weak<` ×1 | observer: the execution scope signals live child processes it does not own |
| `crates/server/awaken-session-application/src/mutation.rs` | `Internal(violation.to_string())` | the serialized realization-control failure carries class plus text across the Worker boundary |
| (SDK) `crates/contract/awaken-runtime-contract`, `crates/contract/awaken-tool-contract`, `crates/runtime/awaken-runtime`, `crates/runtime/awaken-ext-mcp`, and `crates/server/awaken-session-executor/src/delegate.rs` (the SDK trait impl) | `Option<&dyn` | kept by item L: capability queries of open SDK traits and pure-function arguments whose `None` has one documented meaning ([SDK versioning](sdk-versioning.md#optional-roles-in-the-sdk-api)) |
| every row marked `exempt` in the `Option<&dyn …>` table below | `Option<&dyn` | the reason in that row: a feature left off, a capability query, or the `Error::source` signature |

## Sites

### `Weak<`

| Site (`crates/…`, line at `1b56f9aea0`) | Class | Status | Decision |
|---|---|---|---|
| `runtime/awaken-ext-mcp/src/transport.rs:172` | (ii) | exempt | revocation: a transport admitted under a drained generation fence must fail, not keep the fence alive |
| `runtime/awaken-ext-mcp/src/transport.rs:178` | (ii) | exempt | revocation: a cloned notification handler stops delivering once its generation fence is gone |
| `runtime/awaken-runtime/src/runtime.rs:160` | (ii) | exempt | cache: a per-Thread execution gate is collected when no guard holds it |
| `server/awaken-protocol-managed/src/lifecycle_delivery.rs:20` | (ii) | exempt | observer: the outbox supervisor must not own the HTTP adapter it wakes |
| `server/awaken-session-executor/src/application/attempt_context.rs:24` | (i) | exempt | containment: projects its own Session's frozen tools, baseline and staged prompts per attempt. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/application/attempt_context.rs:33` | (i) | done | the constructor takes the registry as `&Arc` and keeps the one weak edge |
| `server/awaken-session-executor/src/background_task.rs:118` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/background_task.rs:186` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/background_task.rs:200` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/coordination.rs:50` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/coordination.rs:53` | (i) | exempt | containment: reads its Session's current dispatch claim per call. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/coordination.rs:92` | (i) | done | `current_claim` takes the upgraded registry |
| `server/awaken-session-executor/src/coordination.rs:411` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/host/capability/resources.rs:123` | (ii) | done | `ResourcePorts` passes the registry as `&Arc`; `MemoryTerminalObservers` keeps the one weak edge |
| `server/awaken-session-executor/src/host/dependencies/session_application.rs:59` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/host/dependencies/session_application.rs:64` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/host/session/context/plan.rs:27` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-executor/src/lazy_sandbox.rs:13` | (i) | done | removed: the deferred Sandbox executor reads its Session through the realization executor's view of the same registry |
| `server/awaken-session-executor/src/lazy_sandbox.rs:14` | (i) | exempt | containment: stored in the Session's run layer, realizes the Environment through the realization executor that owns the registry. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/lazy_sandbox.rs:54` | (i) | done | removed with the registry handle above |
| `server/awaken-session-executor/src/lazy_sandbox.rs:55` | (i) | done | the constructor takes the realization executor as `&Arc` and keeps the one weak edge |
| `server/awaken-session-executor/src/mcp_host.rs:339` | (ii) | exempt | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `server/awaken-session-executor/src/mcp_host.rs:352` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-executor/src/memory/terminal_observer.rs:48` | (ii) | exempt | retained role: the Coordinator's Memory settlement observer outlives the executor and must not keep resident Sessions alive |
| `server/awaken-session-executor/src/memory/terminal_observer.rs:56` | (ii) | done | the constructor takes the registry as `&Arc` and keeps the one weak edge |
| `server/awaken-session-executor/src/run_assembly/mcp_attachment.rs:58` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-executor/src/run_assembly/mcp_attachment.rs:60` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-executor/src/run_exec.rs:30` | (ii) | exempt | ownership verifier: an attempt whose Session context is gone is no longer current (fail closed) |
| `server/awaken-session-executor/src/run_exec.rs:79` | (i) | exempt | containment: the artifact-harvest decorator lives in the Session context and harvests through the `ArtifactHarvester` that reads the registry. Remove when the Session context cache leaves the installed-Session entry ([Family B](#family-b-installed-session-registry)) |
| `server/awaken-session-executor/src/run_exec.rs:86` | (i) | done | the constructor takes the harvester as `&Arc` and keeps the one weak edge |
| `server/awaken-session-executor/src/session_effects.rs:25` | (ii) | exempt | retained role: a configured MCP realizer must not keep unrelated local Session state alive |
| `server/awaken-session-executor/src/thread_reads.rs:29` | (ii) | exempt | retained reader: the Worker recovery source outlives the composition and must not keep installed Sessions alive |
| `server/awaken-session-executor/src/thread_reads.rs:135` | (ii) | exempt | retained reader: advisor labels of the retained recovery source; must not keep installed Sessions alive |
| `server/awaken-session-executor/src/thread_reads.rs:136` | (ii) | exempt | retained reader: advisor labels of the retained recovery source; must not keep the realization executor alive |
| `server/awaken-session-mcp/src/mcp/attachment.rs:52` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp/input.rs:36` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-mcp/src/mcp/input.rs:37` | (ii) | exempt | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `server/awaken-session-mcp/src/mcp/input.rs:41` | (ii) | exempt | observer: an MCP notification handler rechecks the Host authority per delivery and must not keep it alive |
| `server/awaken-session-mcp/src/mcp/input.rs:42` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-mcp/src/mcp/sessions.rs:324` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp/sessions.rs:338` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp/sessions.rs:350` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-mcp/src/mcp_relay.rs:207` | (ii) | exempt | background server lifetime: the process MCP relay admits routes only while the root authority lives; it must not keep the Host alive |
| `server/awaken-session-realization/src/execution_authority.rs:32` | (ii) | exempt | captured authorization: a later effect fails closed once the installed Sessions are gone |
| `server/awaken-session-realization/src/execution_authority.rs:45` | (ii) | exempt | captured authorization: an execution target denies once its Environment is dropped |
| `server/awaken-session-realization/src/session_realizations.rs:359` | (ii) | exempt | the one constructor of the captured authorization handle above |
| `server/awaken-session-run-delivery/src/background.rs:29` | (ii) | exempt | interning cache: one shared admission per (thread, task) while some Run holds it |
| `server/awaken-session-run-delivery/src/completion.rs:200` | (ii) | exempt | drop guard: removing a waiter must not keep the completion registry alive |
| `server/awaken-session-run-delivery/src/session_applications.rs:57` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-run-delivery/src/session_applications.rs:58` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-run-delivery/src/session_applications.rs:99` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `server/awaken-session-run-delivery/src/session_applications.rs:103` | (i) | done (c) | Family A: replaced by the one `SessionApplicationBinding` ([above](#family-a-host-and-session-application)) |
| `worker/awaken-sandbox-local/src/provider/execution.rs:102` | (ii) | exempt | observer: the execution scope signals live child processes it does not own |

### `Option<&dyn …>`

Lines are at `1b56f9aea0`, except the rows this batch added (`durable_control.rs`, `pod_metadata.rs` and the two `Error::source` rows), which are at `504872e23d`. `principle_status.py` also counts the `&(dyn ..)` spelling.

| Site (`crates/…`) | Class | Status | Decision |
|---|---|---|---|
| `bin/awaken-process-startup/src/service.rs:218` | (iii) | done | closed enum `EmbeddedWorker { Colocated(..), NotLinked }`; rows W1-W4 (`disabled_or_split_worker_startup_has_no_local_effects`) |
| `bin/awaken-worker/src/credential_liveness.rs:129` | (iii) | exempt | feature off: a Worker without projected credentials publishes an empty credential observation family |
| `bin/awaken-worker/src/credential_liveness.rs:130` | (iii) | exempt | feature off: a Worker without ACP CLIs publishes an empty capability observation family |
| `bin/awaken-worker/src/credential_liveness.rs:215` | (iii) | exempt | same as `:129` |
| `bin/awaken-worker/src/credential_liveness.rs:224` | (iii) | exempt | same as `:130` |
| `bin/awaken-worker/src/manifest.rs:196` | (iii) | exempt | feature off: without brokered ACP model access no brokered credential profile is advertised; the builder already refuses it beside the local materializer |
| `contract/awaken-dispatch-contract/src/commit_epoch.rs:186` | (iii) | done | closed enum `PreparedRunRecovery { OriginalAuthority(&dyn RunRecoverySource), ReceiptOnly }`; rows A1-A4 (`prepared_admission_requires_exact_absence_and_observed_release`) |
| `contract/awaken-provisioning-contract/src/sandbox/effect.rs:32` | (iii) | done | `SandboxEffect::from_parts` deleted: a fence without its authority is unrepresentable (old row P3) |
| `contract/awaken-provisioning-contract/src/sandbox/runtime.rs:904` | (iii) | exempt | capability query: checkpointing is a property of the Sandbox backend, not a role a deployment selects; `None` is the absent capability |
| `contract/awaken-runtime-contract/src/auxiliary_agents.rs:67` | (iv) | kept (L) | pub fn freeze: absent publication source |
| `contract/awaken-runtime-contract/src/delegation.rs:351` | (iv) | kept (L) | trait default: cancellation capability query |
| `contract/awaken-runtime-contract/src/execution.rs:66` | (iv) | kept (L) | pub fn verify_attempt_ownership: direct callers have no verifier |
| `contract/awaken-runtime-contract/src/permission/run_capability.rs:41` | (iv) | kept (L) | pub fn effective: absent policy |
| `contract/awaken-runtime-contract/src/permission.rs:328` | (iv) | kept (L) | pub fn evaluate_run_policy: absent policy |
| `contract/awaken-runtime-contract/src/resolver.rs:74` | (iv) | kept (L) | pub fn resolve_delegate_snapshot: absent publication source |
| `contract/awaken-runtime-contract/src/resolver.rs:105` | (iv) | kept (L) | pub fn freeze_delegation_publications: absent publication source |
| `contract/awaken-session-configuration-contract/src/resource/binding_checks.rs:50` | (iii) | exempt | feature off: no Memory verifier means the deployment offers no Memory resource; row B2 refuses |
| `contract/awaken-session-configuration-contract/src/resource/binding_checks.rs:118` | (iii) | exempt | same as `:50` |
| `contract/awaken-session-configuration-contract/src/resource/binding_checks.rs:119` | (iii) | exempt | feature off: no Repository verifier means the deployment offers no Repository resource; row B4 refuses |
| `contract/awaken-session-configuration-contract/src/resource/inputs.rs:890` | (iii) | exempt | feature off: the optional `.resources()` builder feature; without it the tested refusal rows (N2/N7, `NotFound`) apply |
| `contract/awaken-session-configuration-contract/src/skill_delivery.rs:93` | (iii) | exempt | feature off: no Skill source means no custom Skills; row P3 refuses with `AuthorityUnavailable`, built-in Skills still resolve |
| `contract/awaken-session-configuration-contract/src/skill_delivery.rs:135` | (iii) | exempt | same as `:93` (absent catalog) |
| `contract/awaken-session-environment-contract/src/tool_export.rs:27` | (iii) | exempt | capability query: attached channels are a property of the exporter, not a role a deployment selects |
| `contract/awaken-session-execution-contract/src/session_realization/canonical_publication.rs:82` | (iii) | exempt | Worker holds no publication catalog: it runs only the publications frozen into each claimed dispatch; absence reads as no publication (rows S2/S4, `SnapshotNotFound`) |
| `contract/awaken-tool-contract/src/tool.rs:914` | (iv) | kept (L) | trait default: DurableTaskTool capability query |
| `contract/awaken-tool-contract/src/tool.rs:1219` | (iv) | kept (L) | trait default: DurableTaskExecutor capability query |
| `contract/awaken-tool-contract/src/tool.rs:1372` | (iv) | kept (L) | SDK impl of the query above |
| `control/awaken-credential-vault/src/repo/credential_operations.rs:1188` | (iii) | done | closed enum SourceMaterialMutation {MetadataOnly, Stored{store, materials}}: a material write without its store is unrepresentable |
| `control/awaken-environment-application/src/lib.rs:348` | (iii) | done | required at construction: the one production composition always supplied the policy store; tests share `InMemorySandboxExecutionPolicyStore::default()`; the unreachable `PolicyStoreUnavailable` variant is removed |
| `runtime/awaken-ext-goal/src/controller.rs:128` | - | exempt | `std::error::Error::source` signature |
| `runtime/awaken-ext-goal/src/controller.rs:148` | (iii) | done | private closed enum `RunCapability { Executing(&dyn OutcomeRuns), Preparation }` fixed by the constructor; rows X1-X2 (`run_capability_decides_whether_a_controller_may_touch_runs`) |
| `runtime/awaken-ext-mcp/src/tool.rs:170` | (iv) | kept (L) | SDK impl of the DurableTaskTool query |
| `runtime/awaken-runtime/src/engine/finalize.rs:89` | (iv) | deferred-L | awaken-runtime (SDK) internals: closed enum of the committed reader, with L |
| `runtime/awaken-runtime/src/engine/inference.rs:245` | (iv) | kept (L) | forwards RuntimeRunContext ownership to verify_attempt_ownership |
| `runtime/awaken-runtime/src/engine/inference.rs:399` | (iv) | kept (L) | forwards RuntimeRunContext ownership to verify_attempt_ownership |
| `runtime/awaken-runtime/src/engine/tool_execution.rs:945` | (iv) | kept (L) | SDK impl of the DurableTaskExecutor query |
| `runtime/awaken-runtime/src/engine/tool_execution.rs:1081` | (iv) | deferred-L | awaken-runtime (SDK) internals: carry the executor in `ToolExecutionTarget::Sandbox`, with L |
| `server/awaken-coordinator/src/worker_observation_boundary.rs:262` | (iii) | exempt | feature off: a Coordinator without a Worker provider cannot seal bootstrap ledgers and answers 503 (row R3) |
| `server/awaken-inference-endpoint/src/brokered_inference.rs:781` | (iii) | done | closed enum ResponseDelivery {Whole, Streamed(sink)} |
| `server/awaken-protocol-mcp/src/session_export.rs:98` | (iii) | exempt | implements the `AcpToolExporter::attached_channels` capability query above |
| `server/awaken-run-executor-acp/src/permission.rs:81` | (iv) | deferred-L | passes the narrowing to the SDK `evaluate_run_policy(Option<&dyn ..>)`; changes with that signature |
| `server/awaken-run-executor-acp/src/permission.rs:211` | (iii) | exempt | optional observer: absent when the Session exports no Awaken tools to the ACP CLI |
| `server/awaken-run-executor-acp/src/permission.rs:249` | (iii) | exempt | same as `:211` |
| `server/awaken-run-executor-acp/src/permission.rs:263` | (iii) | exempt | same as `:211` |
| `server/awaken-run-executor-acp/src/tool_continuation.rs:52` | (iii) | exempt | feature off: an ACP Run that owns no Awaken tools has no tool continuation |
| `server/awaken-session-application/src/credentials.rs:69` | (iii) | exempt | feature off: the optional `.credentials()` builder feature; without it vault-backed requests are refused and `missing_vault` is `Ok(None)` |
| `server/awaken-session-application/src/credentials.rs:74` | (iii) | exempt | feature off: without material ingress the deployment's credentials are read-only (`SessionCredentialAuthorities::read_only`) |
| `server/awaken-session-application/src/realization.rs:276` | (iii) | planned | production `SessionExecutionAdmission::new` already requires the role (compile_fail doctests); the `OnceLock` serves only the test-support partial fixture, whose rework spans about 12 test files in 6 crates |
| `server/awaken-session-application/src/resource_lowering.rs:79` | (iii) | exempt | feature off: the optional `.resources()` builder feature; without it the tested refusal rows (N2/N7, `NotFound`) apply |
| `server/awaken-session-application/src/resource_lowering.rs:100` | (iii) | exempt | feature off: the optional `.resources()` builder feature; without it the tested refusal rows (N2/N7, `NotFound`) apply |
| `server/awaken-session-application/src/resource_lowering.rs:110` | (iii) | exempt | feature off: the optional `.resources()` builder feature; without it the tested refusal rows (N2/N7, `NotFound`) apply |
| `server/awaken-session-application/src/session_input.rs:343` | (iii) | exempt | feature off: the optional `.resources()` builder feature; without it the tested refusal rows (N2/N7, `NotFound`) apply |
| `server/awaken-session-application/src/session_input.rs:366` | (iii) | exempt | feature off: the optional `.resources()` builder feature; without it the tested refusal rows (N2/N7, `NotFound`) apply |
| `server/awaken-session-application/src/terminal_cleanup/claims.rs:82` | (iii) | planned | same as `realization.rs:276`: required in production, optional only for the partial test fixture |
| `server/awaken-session-executor/src/application/realization_renewal.rs:195` | (iii) | exempt | feature off: a process without a Session Control client (local `awaken-cli`) renews nothing (`Ok(0)`) or refuses a due lease with the existing error |
| `server/awaken-session-executor/src/application/realization_renewal.rs:237` | (iii) | exempt | same as `:195` |
| `server/awaken-session-executor/src/delegate.rs:242` | (iii) | done | required: `HostRunDelegationService` holds its publication source; `Some(..)` only at the SDK `resolve_delegate_snapshot` call; tests share `no_publications()` |
| `server/awaken-session-executor/src/delegate.rs:599` | (iv) | kept (L) | platform impl of the SDK cancellation query; follows its signature |
| `server/awaken-session-executor/src/outcome_controller.rs:114` | (iii) | done | executor moved into OutcomeCommand::Resume; Prepare never carries one |
| `server/awaken-session-mcp/src/mcp/attachment.rs:100` | (iii) | exempt | feature off: the Coordinator credential-refresh adapter may be left unset; staging refuses with `mcp_credential_refresh_unavailable` or attaches no reloader |
| `server/awaken-session-mcp/src/mcp/attachment.rs:101` | (iii) | exempt | feature off: without the platform egress mediator staging refuses with `mcp_gateway_provisioner_required` |
| `server/awaken-session-realization/src/environment_holding/realization.rs:106` | (iii) | done | owned enum `AuthorizedProviderEffect { Unattached, Fenced { fence, authority } }` replaces the fence and authority options; rows A1-A4 (`authorized_provider_effect_carries_its_fence_with_its_authority`) |
| `server/awaken-session-realization/src/test_support.rs:57` | (iii) | done | the fixture takes `SandboxEffect` |
| `server/awaken-session-realization-driver/src/terminal_cleanup_recovery.rs:149` | - | exempt | `std::error::Error::source` signature |
| `server/awaken-session-run-delivery/src/auxiliary_snapshot.rs:41` | (iii) | done | required `&dyn PublishedAgentSnapshotSource`: every caller passed `Some`; rows R1/R5 use an empty source |
| `server/awaken-session-run-delivery/src/child_dispatch.rs:23` | (iii) | exempt | Worker holds no publication catalog: it runs only the publications frozen into each claimed dispatch; absence reads as no publication (rows S2/S4, `SnapshotNotFound`) |
| `server/awaken-session-run-delivery/src/delivery/outcomes.rs:116` | (iii) | done | executor moved into OutcomeCommand::Resume; Prepare never carries one |
| `server/awaken-session-run-delivery/src/session_projections.rs:424` | (iii) | exempt | Worker holds no publication catalog: it runs only the publications frozen into each claimed dispatch; absence reads as no publication (rows S2/S4, `SnapshotNotFound`) |
| `server/awaken-session-run-delivery/src/session_run_reservation.rs:49` | (iii) | exempt | per-Session data, not a role: a Session with no Skills has no registry and its input passes through unexpanded |
| `server/awaken-session-run-delivery/src/thread_coordination/terminal_quiescence.rs:60` | (iii) | done | closed enum `RootAttempt { Resident(&dyn ResidentAttempt), Elsewhere }`; rows Q1-Q3 (`root_attempt_reaches_only_its_own_threads_dispatches`) |
| `server/awaken-worker-transport-security/src/lib.rs:146` | (iii) | done | required `&dyn WorkerDirectory`: every router already held one; the unreachable local branch and `with_worker_directory` are removed |
| `server/awaken-session-executor/src/host/capability/execution.rs:17` | (iii) | done | closed enum `AcpBackend { Absent, Installed(factory), Projection(&'static policy) }`; rows P1-P3 (`coordinator_catalog_policy_never_installs_an_acp_executor`) |
| `shared/awaken-dispatch-execution/src/durable_control.rs:30` | (iii) | done | closed enum `RunResidence { Resident(&live control), Elsewhere }`; rows C1-C3 (`the_live_accelerator_follows_an_accepted_intent_only_for_a_resident_attempt`) |
| `shared/awaken-dispatch-execution/src/durable_control.rs:51` | (iii) | done | same as `:30` |
| `shared/awaken-dispatch-execution/src/durable_control.rs:62` | (iii) | done | same as `:30` |
| `shared/awaken-dispatch-execution/src/session_dispatch.rs:73` | (iii) | exempt | Worker holds no publication catalog: it runs only the publications frozen into each claimed dispatch; absence reads as no publication (rows S2/S4, `SnapshotNotFound`) |
| `shared/awaken-dispatch-execution/src/session_dispatch.rs:110` | (iii) | exempt | Worker holds no publication catalog: it runs only the publications frozen into each claimed dispatch; absence reads as no publication (rows S2/S4, `SnapshotNotFound`) |
| `shared/awaken-worker-fleet-kubernetes/src/pod_metadata.rs:91` | (iii) | done | required checkpoint callback; the runtime-owner transfer passes `unfenced_checkpoint()` |
| `worker/awaken-sandbox-local/src/namespace/memory_mount.rs:195` | (iii) | done | adoption takes one `SandboxEffect`, not a fence and an authority option |
| `worker/awaken-sandbox-local/src/provider.rs:1619` | (iii) | exempt | implements the `Sandbox::checkpointing` capability query above |

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
