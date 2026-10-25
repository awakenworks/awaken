# ADR-0088: Bounded Context Map and SDK–Platform Contract

- Status: Accepted
- Date: 2026-10-07
- Builds on: ADR-0081 contract layering and ratchet guardrails; ADR-0086 Runtime SDK
  public surface; ADR-0087 SQLite through sqlx.
- Affects: crate `bounded_context` metadata, the crate-boundary guards, and
  awaken-cloud only through later renames (none in this ADR).
- Amended by: ADR-0089 (Accepted: decision 2 separates Execution's realization
  effects from the independent Session driver shared by both topologies)
- Amended by: ADR-0091 (Accepted: the 2026-10-07 amendment sentence on not merging
  the small Session contracts)
- Amended: 2026-10-09 (reconciled with ADR-0081: public-API snapshots are an
  `api_snapshot` opt-in)
- Amended: 2026-10-09 (reconciled with ADR-0086: credential vocabulary lives in
  `awaken-agent-contract`)
- Amended: 2026-10-10 (the dispatch contract stops depending on Session contracts;
  public paths that moved and their awaken-cloud impact)
- Amended: 2026-10-11 (provider checkpoint capture input has its own name;
  Session authorization and checkpoint object bytes are unchanged)
- Amended: 2026-10-24 (crate names follow the context vocabulary: `run-ingress-*`
  becomes `dispatch-*` and non-SDK `runtime` crate names are renamed; awaken-cloud impact)

## Context

Crates already carry `product` and `bounded_context` metadata, and guards ratchet
dependency direction between contexts. The tactical model is sound: the Session
aggregate and its pure transition laws live apart from persistence and application
services; `SessionRepository`, the lifecycle outbox and the Managed timeline
projection are standard.

The strategic layer is not. Contexts were drawn by where code runs, not by which
domain problem it solves:

- `session` holds 26 crates that mix four concerns: the Session lifecycle, the Run
  execution host (`awaken-runtime-host`, `awaken-session-mcp`), the runtime's thread
  commit storage (`awaken-store-*`), and the executable Agent catalog that Control
  publishes.
- `worker` is a deployment shape, not a domain. Run execution happens both in a
  Coordinator process and on a Worker.
- Languages that cross contexts have no stated owner. `FrozenSessionProjection` and
  the realization actions describe the Session, but live in a contract tagged
  `environment`. Session-specific remote ports (`ClaimedSessionRealizationControl`,
  `ClaimedSessionControl`) live in the dispatch contract.
- The relationship between the Runtime SDK and the Platform was never written down.
  The guard `sdk-standalone` forbids SDK → Platform edges, but nothing says what the
  Platform may use from the SDK. Measured on main: Platform crates depend on
  `awaken-agent-contract` 59 times, `awaken-time` 55 times and
  `awaken-runtime-contract` 48 times. Once Run execution is counted as its own
  context, engine-SPI modules of `runtime-contract` are used outside it at about 57
  sites. Most other Platform uses are values: the Agent definition (`resolved`,
  `snapshot`, `agent_bindings`, `delegation`, `permission`) and the Run, message and
  thread vocabulary of `agent-contract`.

## Decision

1. **Subdomains.** Session (lifecycle and authority of managed sessions) and
   Execution (running Runs in isolated environments) are core. Environment,
   Dispatch, Committed Truth, Resources, Config, Credentials and Governance are
   supporting. Storage, Fencing, Protocols, Access and Deployment are generic. The
   Runtime SDK is a separate product with one context, `agent-runtime`.

2. **Contexts follow domain problems.** Membership changes through metadata only:
   - **Execution** (new): `awaken-runtime-host`, `awaken-session-mcp`, the future
     `awaken-session-realization`, `awaken-run-executor-acp`, `awaken-run-executor-a2a`,
     `awaken-execution-composition`, `awaken-agent-channel`, `awaken-acp-contract`,
     `awaken-dispatch-execution`, `awaken-dispatch-erasure`. The latter owns
     immutable execution bindings and role projections; Dispatch's published
     leaf ports remain in `awaken-run-ingress-contract`. Backend adapters and
     Worker transports consume that binding through their existing execution
     entrypoint, without a second cross-context infrastructure dependency.
   - **The `worker` context is retired.** `awaken-worker-runtime`,
     `awaken-worker-registry`, `awaken-worker-contract`,
     `awaken-worker-transport-security` and `awaken-connection-plan` join Dispatch. A
     separate Fleet context waits until they show an independent change rhythm.
   - **Committed Truth** (new): `awaken-store-schema`, `awaken-store-sqlite`,
     `awaken-store-postgres`, `awaken-store-fs`, `awaken-store-conformance`. They
     implement the SDK's commit SPI; they are not the Session aggregate.
   - **Config** gains the executable Agent catalog: `awaken-executable-agent-contract`,
     `awaken-executable-agent-catalog`, `awaken-acp-application`.
   - **Session** keeps the lifecycle: state, configuration, resource, mcp, event,
     budget and error contracts, the repository and application contracts,
     `awaken-session-application`, `awaken-session-store`, the Dream application, and
     the neutral `awaken-session-realization-driver`, which executes actions the
     Session aggregate projects for both topologies.

3. **Published languages belong to the upstream context.**
   - What the Session publishes to Execution (`FrozenSessionProjection`, the projected
     realization actions) belongs to Session. Environment-side control ports stay in
     Environment. They are split into two crates only if their reader sets differ;
     otherwise only the metadata changes.
   - `awaken-session-runtime-contract` is split by direction when it is next changed:
     the roles the Session requires from Execution, and the callbacks Execution makes
     into the Session application.
   - Session-specific remote ports leave the dispatch contract after the Session
     authority port lands. Dispatch carries them; it does not own them.

4. **The SDK–Platform contract is one explicit list.**

   | Part | Content | Who may depend on it |
   |---|---|---|
   | Published language (values) | `awaken-agent-contract` (Run, message, thread read, content, event, audit; `ScopeId` and `ExecutionScopeRef` after ADR-0086 #6); `awaken-tool-contract` values; the Agent definition (`resolved`, `snapshot`, with `resolution` and the builder per ADR-0086 #5); credential reference vocabulary in `awaken-credential` (ADR-0086 #7); capture values (ADR-0086 #1, #2) | Any context |
   | SPI (implemented by the Platform) | `RunExecutor`, `LlmExecutor`, `ToolExecutor`, `Plugin`, `CaptureSink`; commit SPI `OperationCoordinator`, `CheckpointReader`, `RunRecoverySource` | Execution and Committed Truth only |
   | Shared kernel | `awaken-time`, `awaken-lease-contract` | Any context |
   | Direction | SDK → Platform is forbidden | Existing guard `sdk-standalone` |
   | Change control | public-api snapshots for all 22 publishable SDK crates, semver | Release process |

   A per-module tier table appended to ADR-0086 (value / SPI / leaves the public
   surface) is the single source for this list; the guard reads it. The Platform
   translates its own concepts into SDK inputs in one place: Execution.
   `awaken-time` stays a separate SDK leaf: it is cohesive, has no internal
   dependencies and 75 dependents; merging it into `awaken-lease-contract` or
   `awaken-agent-contract` would give 54 or 27 crates a dependency they do not need.

5. **The map is executable.** Two guard rules are added, each ratcheted from the
   current state (may only decrease): Platform crates may depend on context pairs
   that this ADR lists; engine-SPI modules and crates may be used only by Execution
   and Committed Truth.

6. **Order.** This ADR first. Then the metadata changes and the two guard rules,
   before the Session authority port. The realization-language split comes before
   the realization crate moves out of the runtime host; the remote-port move after
   the Session authority port; the `session-runtime-contract` split with the
   glossary renames (`session-state-contract` keeps its name; see the amendment). ADR-0086 is implemented in its own order
   (4, 3, 2, 6, 7).

## Alternatives

- **Keep contexts by process.** Every guard would keep enforcing a deployment shape;
  realization and execution code would keep landing in `session`.
- **New crates for every context seam now.** Most seams share reader sets today;
  splitting before readers differ adds crates without narrowing anyone's dependency.
- **Opaque SDK interfaces for all Platform use.** The Agent definition and the Run
  vocabulary are genuinely shared; wrapping them would duplicate a published language.

## Consequences

- Contexts describe domain problems; `session` shrinks to the lifecycle; Execution
  and Committed Truth become visible owners.
- Two new ratchets make the context map and the SDK boundary fail closed.
- The Platform side of the SDK boundary is reduced to a counted list of sites to
  clean up, most of which disappear when Run execution is classified as Execution.
- No public paths change in this ADR; later renames list their awaken-cloud impact
  when they are made.

## Amendment 2026-10-07: a crate states why it is separate

**Principle.** Crates are merged or split only for an effect that lasts: the shape
of a public contract (SDK API, paths awaken-cloud binds to), content placed in the
wrong widely-used contract, or a baseline a guard will freeze. A merge that changes
the crate count but not the dependency graph is not made.

**Rule.** Every crate sets `[package.metadata.awaken] separation` to one reason, and
`scripts/ci/_crate_separation_fitness.py` (run by `check_crate_boundaries.py`)
checks it:

| `separation` | Check |
|---|---|
| `publish` | product `sdk`, publishable |
| `process` | a binary target, or layer `bootstrap` (the composition root a binary assembles) |
| `contract` | layer `contract`, and a crate of another bounded context depends on it |
| `isolation` | `isolates` lists dependencies from the script's heavy / optional / OS list; the crate depends on each, and another crate of its bounded context does not |
| `protocol` | `protocol` names an external wire protocol from the script's list; no other crate of its bounded context adapts it |
| `sole-in-layer` | the only crate of its bounded context at its layer |
| `test-support` | layer `tooling` |

The 20 crates without a checkable reason at adoption are listed in `UNJUSTIFIED`.
The list only shrinks. Being listed is not an order to merge.

**A dependency is declared where it is used.** The context map and every crate-graph
ratchet read the declared graph, so a declared edge nobody uses becomes an accepted
crossing. `scripts/ci/_dependency_use_fitness.py` requires each normal dependency on
a workspace crate to be named in production code (`cfg(kani)` code counts, `cfg(test)`
code does not); a dependency named only by tests is a dev-dependency, and one kept only
to turn on features is listed in `metadata.awaken.enables`. Adopting it removed or
moved 47 declarations and two context pairs that only such edges held
(dispatch → config, dispatch → resources).

**Published languages re-homed after the coupling review.** Five crates were labelled
with a context their readers do not share; correcting the metadata removed seven
context pairs and two `context-contract-seam` baseline entries without moving code:
`awaken-session-realization-contract` → session (decision 3: the Session publishes it);
`awaken-environment-contract`, `awaken-agent-channel` and `awaken-acp-contract` →
environment (the language for reaching an agent process in an environment, read by
sandbox, dispatch, credentials and execution alike); `awaken-sandbox-fs` → storage
(capability file-system infrastructure). `awaken-connection-plan` stays in dispatch:
moving it to execution would add a sandbox → execution edge.

**Contexts are ordered.** Listing allowed context pairs does not stop cycles: the 70
pairs seeded from the graph held 19 two-way pairs, and all eleven Platform contexts
formed one strongly connected set. The Platform contexts are therefore ordered, each
depending only on contexts below it (top to bottom): committed-truth, protocols,
deployment, config, execution, sandbox, resources, dispatch, environment, access,
session, credentials, governance; the shared `fencing` kernel and `storage`
infrastructure sit under all of them. The order is the one that left the fewest upward
edges on 2026-10-07 while keeping the Session's published language low. The
`context-order` rule in `_context_map_fitness.py` counts every upward crate edge;
today's 39 are listed in `context_order_baseline.json`, each naming the change that
removes it, and a new one fails. The order is reviewed when fewer than ten remain;
changing it amends this ADR.

**Decisions made with it.**
- The SDK extensions stay separate crates (`publish`): SDK users pick plugins and
  their dependencies; a merge would not remove a single dependency edge.
- The small Session contracts are not merged: `session-state-contract` already
  depends on the budget, configuration, event, mcp and resource contracts, so a
  merge leaves the graph unchanged. What lasts is moving misplaced content:
  `agent_config` goes to `executable-agent-contract`, `SessionEnvironmentSource`
  moves up to the application contract, and `session-runtime-contract` is split by
  direction (decision 3). `session-state-contract` keeps its name.
- `awaken-protocol-ag-ui` and `awaken-protocol-ai-sdk` stay separate (`protocol`):
  they adapt two external protocols that evolve independently. Seven functions
  (46 lines) are identical; that does not justify a shared module binding them.

## Amendment 2026-10-09: reconciled with ADR-0081

The change-control row of decision 4 does not mean every publishable SDK crate has a
snapshot today. [ADR-0081](0081-contract-layering-and-ratchet-guardrails.md) §9
(amended 2026-09-28) decides that the public-API gate snapshots only crates whose
manifest sets `[package.metadata.awaken] api_snapshot = true`. Five crates opt in
today (`awaken-agent-contract`, `awaken-tool-contract`, `awaken-runtime-contract`,
`awaken-mcp-wire`, `awaken-runtime`; `public-api/` holds those five files). The
row is reached when every publishable SDK crate opts in; that is planned work (W6).

## Amendment 2026-10-09: reconciled with ADR-0086

Decision 4 places the credential reference vocabulary in `awaken-credential`. The
2026-10-08 amendment of [ADR-0086](0086-runtime-sdk-public-surface.md) moved it into
the `credential` module of `awaken-agent-contract` and removed `awaken-credential`.
The code follows ADR-0086: `crates/contract/awaken-agent-contract/src/lib.rs:7`
(`pub mod credential;`); no `awaken-credential` crate exists. Read the row as
"credential reference vocabulary in `awaken-agent-contract::credential`".

## Amendment 2026-10-10: the dispatch contract stops depending on Session contracts

Decision 3 says Dispatch carries the Session's ports and published values but does
not own them. `awaken-run-ingress-contract` still owned six of them, so its contract
depth was 9. They now live with the Session, and the dispatch contract's depth is 6.
Its one remaining Session dependency, `awaken-session-configuration-contract`, is
legacy-only: claim admission decodes the runtime projection of rows written before
the Session published typed MCP credential requirements. It is removed once those
rows have drained. Persisted bytes did not change; golden tests in
`crates/contract/awaken-run-ingress-contract/tests/persisted_wire_golden.rs` pin
them.

Public paths that moved (ADR-0091 D9 requires recording them):

| Before | After |
|---|---|
| `awaken_run_ingress_contract::{ClaimedSessionControlError, ClaimedSessionRealizationControl}` | `awaken_session_realization_contract::…` |
| `awaken_run_ingress_contract::ClaimedSessionCoordinationControl` | `awaken_session_runtime_contract::ClaimedSessionCoordinationControl<Claim>` (Host uses `<RunClaim>`) |
| `awaken_run_ingress[_contract]::SessionRunAdmissionObserver` | `awaken_dispatch_execution::SessionRunAdmissionObserver` |
| `awaken_run_ingress[_contract]::{ResourceOperationFence, ArtifactPublicationFence}` | `awaken_session_realization_contract::ResourceOperationFence`; the alias is deleted |
| `awaken_run_ingress_contract::run_dispatch::DispatchedSessionRuntimeProjection` | `awaken_session_configuration_contract::SessionRuntimeProjection` |
| `awaken_session_runtime_contract::{SessionRunReplacement, SessionRunCommandFingerprint}` | `awaken_session_types::…`; `SessionRunCommandFingerprint::current(&cmd)` is `cmd.command_fingerprint()` |
| `SessionResourceEnvelope::from_manifest(&m)` | `SessionResourceEnvelope::at_revision(m.workspace_id.clone(), m.revision, m.encode_resources()?)` |
| `envelope.decode_manifest()` (resource) | `envelope.decode_with(SessionResourceManifest::decode_resources)` |
| `SessionRuntimeEnvelope::from_projection(env, tools, stages)` | `SessionRuntimeEnvelope::new(SessionRuntimeProjection { environment, tools, mcp_stages: Some(stages) }.encode()?)`; a current admission uses `.dispatch()`, which also returns the MCP credential requirements to place in `PlacementRequirements::session_mcp_credentials` |
| `envelope.decode_projection()` (runtime) | `envelope.decode_with(SessionRuntimeProjection::decode)` |

awaken-cloud production code uses none of these paths. These awaken-cloud test
locations no longer compile, and each takes the replacement above:

| Location | Removed call | Replacement |
|---|---|---|
| `crates/awaken-hosting-durable/tests/phase1_cross_process.rs:1079` | `SessionResourceEnvelope::from_manifest` | `at_revision(…, manifest.encode_resources()?)` |
| `crates/awaken-hosting-durable/tests/phase1_cross_process.rs:1108` | `.decode_projection()` | `.decode_with(SessionRuntimeProjection::decode)` |
| `crates/awaken-hosting-durable/tests/deployed_cluster_causal.rs:271` | `SessionResourceEnvelope::from_manifest` | `at_revision(…, resources.encode_resources()?)` |
| `crates/awaken-hosting-durable/tests/deployed_cluster_causal.rs:336` | `.decode_projection()` | `.decode_with(SessionRuntimeProjection::decode)` |
| `crates/awaken-hosting-durable/tests/deployed_cluster_causal.rs:404` | `.decode_manifest()` | `.decode_with(SessionResourceManifest::decode_resources)` |
| `crates/awaken-hosting-durable/tests/support/retained_session.rs:49` | `SessionRuntimeEnvelope::from_projection` | `SessionRuntimeEnvelope::new(SessionRuntimeProjection { … }.encode()?)` |

A row built as at `retained_session.rs:49` carries no typed requirements, so claim
admission takes the legacy full-decode path for it; that stays correct until the
fallback is removed, after which the helper must also set
`placement.session_mcp_credentials` from `.dispatch()`.

## Amendment 2026-10-11: checkpoint capture and Session authorization are distinct

The two `SandboxCheckpointRequest` definitions are different boundary values.
The Session-owned value carries the complete durable operation, activity epoch,
realization lease, generation and exact source binding. The provisioning value
names the format, bounds and object metadata for capturing, packing and
persisting the physical filesystem; it does not grant a Session effect.

Rename only `awaken_provisioning_contract::SandboxCheckpointRequest` to
`awaken_provisioning_contract::SandboxCheckpointCaptureRequest`, without a
compatibility alias. The Session definition keeps its name and fields. Runtime
reuses its one `provisioning_checkpoint_request` projection for ordinary suspension
and terminal takeover. Capture, WAL, object writes and receipt replay still require
the separate effect fence and current execution authority, then the Session root
admits the exact receipt through its existing CAS. No new mechanism is added.

All request fields, `CheckpointObjectMetadata`, `SandboxCheckpointRef`, upload
fingerprints and marker JSON remain unchanged. Neither request is a serialized
wire DTO. This is a Rust path change, not an object or persistence migration.

awaken-cloud's `awaken-hosting-durable` must use the new provisioning name in
`src/container_continuation.rs`, `src/container_continuation/upload.rs`,
`src/container_continuation/tests.rs` and
`src/container_continuation/tests/upload_tests.rs`. Its capture implementation and
tests must compile against the matching Native candidate before their dependency
pin advances; an older Cloud checkout still naming the old type is not compatible.

## Amendment 2026-10-13: Session configuration owns MCP and Resource values

ADR-0091's configuration-boundary merge is implemented physically. Remove
`awaken-session-mcp-contract` and `awaken-session-resource-contract`; their types
and algorithms move into the `mcp` and `resource` modules of
`awaken-session-configuration-contract`. There are no compatibility facade crates.
The serialization, generation witnesses, fingerprints and fence laws are unchanged.

Platform and awaken-cloud Rust consumers replace `awaken_session_mcp_contract::`
with `awaken_session_configuration_contract::mcp::` and
`awaken_session_resource_contract::` with
`awaken_session_configuration_contract::resource::`. Former foreign re-exports
are imported directly: MCP targets and canonical JSON hashing from
`awaken-agent-contract`, `SessionId` from `awaken-session-types`, and checkpoint
references and restore requests from `awaken-provisioning-contract`.

The Runtime SDK release boundary and wire protocols are unchanged. Cloud must
update these imports and compile its consumers against the matching Native
candidate before advancing its dependency pin; this repository cannot certify
an external Cloud checkout's build.

`SessionRuntimePlacement` and the secret-free `SessionRecoveryQuarantine` report
are defined in `awaken-session-types`; consumers import that leaf directly.
`SessionEnvironmentProjectionSource` declares its `Snapshot` associated type,
and `ResolvedSessionEnvironment<Snapshot>` carries that exact value. The Session
application's composition requires `Snapshot = EnvironmentSnapshot` and retains
its concrete `ResolvedSessionEnvironment` root alias. Environment implementers
declare their output type without making the Environment contract depend on
the Session configuration contract.


## Amendment 2026-10-15: Environment definition roles and registration delivery

`awaken_environment_contract::EnvRegistry` is replaced by three roles in the same
contract crate: `EnvironmentDefinitionQueries` reads current and immutable exact
revisions; `EnvironmentDefinitionStore` authors definitions; and
`EnvironmentRegistrationOutbox` reads and acknowledges committed delivery facts.
SQLite, PostgreSQL, and the in-memory reference implement all three roles directly
over the same physical state. Create, update, and archive still commit the current
definition, immutable revision, and exact registration intent atomically; splitting
the interfaces does not split that transaction or introduce a second owner.

`EnvironmentApplication::new` now accepts one concrete `Arc<T>` implementing the
three roles, derives its internal views from that instance, and constructs the
concrete `EnvironmentRegistrationDelivery`. Delivery receives only queries and the
outbox, with no definition write role. The Control registration supervisor now
accepts `Arc<EnvironmentRegistrationDelivery>`; callers obtain it through
`EnvironmentApplication::registration_delivery()`. Existing application operations
continue to delegate to the same delivery implementation.

Platform and awaken-cloud Rust consumers import the role their operation uses
instead of `EnvRegistry`. Composition passes the concrete backend `Arc` to the
application constructor before erasing it to individual roles. Custom authority
adapters implement the three roles over their one transaction boundary. Cloud must
compile any affected adapters and supervisor construction against the matching
Native candidate before advancing its dependency pin; this repository does not
certify an external Cloud checkout's build.

The Environment use cases share one application boundary. Move the former
`awaken-environment-execution-application` implementation into the existing
`awaken-environment-application::execution` module and remove the former crate.
The surviving application is the sole application-layer crate of the Environment
bounded context; its separation reason is now checkable and it leaves
`UNJUSTIFIED`. The authoring service, exact-intent delivery, executable projection,
and Work lease use cases remain separate components with their existing authority
and typed dependencies. Execution-private queue views and token issuance stay
private to the execution module. This merge removes duplicate crate and dependency
edges; it does not turn Control and Coordinator into one process or one database.

Rust callers replace `awaken_environment_execution_application::Type` with
`awaken_environment_application::execution::Type` and depend only on
`awaken-environment-application`. Existing type names, including
`EnvironmentExecutionApplication`, `CoordinatorEnvironmentRegistrar`, `WorkQueue`,
`WorkQueries`, and the session access roles, stay unchanged. This path migration
also applies to any awaken-cloud consumers; they must compile against the matching
Native candidate before updating their pin.

The Runtime SDK release surface and Anthropic Managed Agents HTTP paths, DTOs,
headers, errors, pagination, and Environment archive behavior are unchanged. There
is no wire-format or database migration.


### Memory caller capabilities and shared mutation observation (draft amendment)

The former `MemoryRepository` public Rust trait is replaced by the caller roles
`MemoryQueries`, `MemorySnapshotSource`, `MemoryWriter`, `MemoryFilesystem`,
`MemoryCopyWriteback`, `MemoryCopyConflictQueries`,
`MemoryCopyConflictPreservation`, `MemoryVersionQueries`,
`MemoryVersionRedaction`, and `MemoryContentPurge`. Backend type names remain
unchanged. Their atomic implementation methods remain private and are reached
through the actual role implementations; no compatibility supertrait or public
broad inherent method set remains.

A Resources root constructs one `InvalidatingMemoryRepository` and derives every
role view from that Arc. `MemoryInvalidator` lives in the existing Resource
contract so the Resources caller and Sandbox implementation remain isolated.
The existing `LocalInvalidator` transport is instantiated once by the process
root and passed to both persistence and the selected mounter. Mounters consume
already-decorated views and never create another mutation chain.

The HTTP client implements its six actual capabilities: Queries, SnapshotSource,
Writer, CopyWriteback, CopyConflictQueries and CopyConflictPreservation. It implements no filesystem rename,
history, redaction or purge role. Querying retained conflicts and acknowledging
preservation are separate capabilities: the Managed API only lists, while the
mounter only preserves. Preservation does not manage TTL; failure keeps the
source undisposable. Copy-only construction requires its four actual
roles, whereas FUSE construction requires a real MemoryFilesystem and the shared
bus. Actor attribution, complete-generation/location CAS, history and conflict
retention remain in the original backend transaction/lock.

Anthropic HTTP objects, public version ids, request parameters, authorization,
error taxonomy and paging retain their existing protocol owners. Rust consumers
must migrate to the actual role constructors at a coordinated repository pin.
This draft does not certify an external Cloud build or any unexecuted test.


Memory role construction also preserves the original single-repository identity: private-field `MemoryExecutionContent`, `MemoryMountSource`, and `MemoryManagementContent` are derived only from one concrete `Arc<T>`. Public Host/Managed/mounter inputs accept these controlled values rather than independent view arrays. FUSE validates the actual decorator's required invalidator identity before retaining a projection; a different bus is an explicit construction error. No broad supertrait, optional capability discovery, second owner, or runtime unsupported adapter is introduced.
## Implementation note 2026-10-16: resource realization boundary

Following accepted ADR-0089, `awaken-session-realization` contains the existing
Execution effects and process-local realization state; it neither owns the Session
aggregate nor depends on Runtime Host. Environment holding, Resource staging,
terminal cleanup and execution authority operate on the original installed-slot
source and lock identities. Host retains Run assembly; the E slice below moves
claimed-dispatch routing to its existing execution owner. The independent Session
driver remains unchanged.

`awaken-credential-materializer` belongs to Execution: it translates frozen
credential pins into exact runtime/provider effects over the installed neutral
material resolver. It owns no credential repository or Vault selection. Its
metadata now reflects that existing responsibility. All its Platform consumers
are Execution; the source/custody contract remains in Credentials. The realization
crate's isolation reason names this concrete provider/crypto/optional-HTTP adapter;
the existing separation guard verifies both that dependency and an Execution
sibling that remains free of it. No context-pair or dependency-direction waiver
is introduced.

Moved functions and persistence-port usage retain their debt under the new paths.
The Host budget is measured after the move, including Run/dispatch glue that was
historically misclassified as extracting realization. This correction does not
satisfy the final Host size, dependency or foreign-persistence-port targets.
There is no wire-format, SDK release surface or database migration.

## Implementation note 2026-10-16: Session dispatch hand-off

The existing `awaken-dispatch-execution` owns `SessionWorkerResolver`, dispatch
envelope compilation, placement, durable control and terminal scan scheduling.
It has no Runtime Host dependency. Its consumer-owned interfaces select the
existing Run factories and Session projection effects; they create no queue,
Session registry, claim authority, or alternate execution path.

| Owner | Structure and retained authority | Execution boundary |
|---|---|---|
| Dispatch | Existing pool, transition kernel, leases, reservation and settlement | Reservation repair and cancellation select the Environment-free Worker before publication or Resource preparation |
| Session | Original aggregate/CAS and independent realization driver | The current Work/Run claim obtains frozen projection authority; the same Session permit serializes Control and installation |
| Realization | Existing ResourceStaging and Worker projection synchronizer over the original slot source | Desired-only resources keep the exact transition/replay checks; atomic permit admission observes the original slot identity before returning |
| Run execution | Host Run factory, committed Thread boundary and configured Session effects | Builds root/child executors only after dispatch preparation; terminal recovery retains the exact opened commit object |

The approved Host-to-neutral-driver dependency is retained at the existing Run
composition seam (`application/worker_projection.rs`). R supplies the projection
and effect adapters. Neither E nor R gains another cross-context implementation
dependency. All-in-one and remote Worker topologies use the same phase driver.
MCP compatibility stages remain suppressed once the complete canonical Session
projection is installed. Durable cancellation records the original intent before
notifying the pool and local Runtime; absence of a live subscriber cannot undo
accepted intent. Pause/wake remain live-only, and text resume still requires the
committed ManualPause ticket.

The old Host production modules `worker_resolver`, `dispatch_resolution`,
`dispatch_reconciliation`, `durable_control`, `placement`, and
`dispatch_session_runtime` no longer own these algorithms. Host-composed tests
remain test-only. H still owns the remaining Host role decomposition.

Moved public Rust helpers have one owner and no Host re-export:

| Previous path | Authoritative path |
|---|---|
| `awaken_runtime_host::remote_worker_placement` | `awaken_dispatch_execution::placement::remote_worker_placement` |
| `awaken_runtime_host::self_hosted_inference_holder` | `awaken_dispatch_execution::placement::self_hosted_inference_holder` |

At the checked Cloud source, only `awaken-hosting-durable` tests
`deployed_cluster_causal.rs` and `phase1_cross_process.rs` use the first helper.
They must use its owner directly when upgrading the common Awaken dependency pin;
an old remote pin is not evidence of compatibility with this local source. This
note records the consumer change, not a Cloud release or physical qualification.

## Implementation note 2026-10-18: Session execution vocabulary

The existing Session execution error and startup builder are named for their
responsibilities. Their fields, constructor requirements,
and one synchronous component-construction path retain the same behavior.

| Previous Rust path | Authoritative Rust path |
|---|---|
| `awaken_session_executor::HostError` | `awaken_session_executor::SessionExecutionError` |
| `awaken_session_executor::HostErrorKind` | `awaken_session_executor::SessionExecutionErrorKind` |
| `awaken_session_executor::HostDependencies` | `awaken_session_executor::SessionExecutorBuilder` |
| `awaken_session_executor::HostProcess` | `awaken_session_executor::SessionExecutionProcess` |
| `DeploymentConfig::host_process` | `DeploymentConfig::session_execution_process` |
| `awaken_sandbox_local::content_fingerprint` | `awaken_resource_contract::content_id` |
| `awaken_coordinator::content_fingerprint` | `awaken_resource_contract::content_id` |
| `awaken_scenario_host::content_fingerprint` | `awaken_resource_contract::content_id` |

The error projection functions live beside `SessionExecutionError`; the removed
Managed forwarding module no longer supplies another import path. Live inbox
edit errors are projected by the existing `LiveInboxes` owner. Startup still
selects its process facts and authority, fills the same builder, and constructs
the same execution components once. A poisoned read follows the existing typed
refusal path through the one Run-error projection.

Run application errors already use `RunError`. Their duplicate classifier is
removed, so classified Internal faults and classified BadRequest faults (such
as `not_found`) keep the original code through the execution boundary. The
kind, code and message decision table covers default and classified forms of
all four faults. The existing private durable-operation envelope retains its
own two-field projection at that same error owner.

Resource bytes keep the existing BLAKE3 content identity. Every Sandbox,
provisioning, File and transport caller imports its Resource owner directly;
the three forwarding aliases are removed. The retained identity test belongs
to that owner and checks the standard empty-input digest and nonempty digest
width. Persisted ids and wire fields do not change.

The checked local `awaken-cloud` source contains no use of the removed symbols.
Consumers upgrading a different pin must use the paths above; this source
inspection does not certify an external Cloud build or SDK publication.

## Implementation note 2026-10-19: one durable application error projection

`awaken-session-runtime-contract` remains the sole owner of `ApplicationError`,
`ApplicationErrorKind`, `DurableRunOperations`, `DurableDispatchStatus` and
`DurableSupersedeResult`. Native callers use that owner directly; the two
`awaken-run-ingress` forwarding layers are removed. Its pure `bad_request`
constructor alias is removed in favor of the existing `invalid` constructor.
The kind, message, private HTTP status and error body remain unchanged.

The existing Worker HTTP `respond` function is the single application-response
projection. Durable operations and the Claimed Commit error arm reuse it; no
third helper or error type is introduced. Claimed Commit's earlier identity
refusal and successful `CommitReceipt` serialization retain their own behavior.
Success preserves its complete value, and each of the four error kinds retains
its exact status and single `error` message field. Classified Run errors,
`SessionExecutionError` and typed realization-control failures keep their
separate boundary projections and stable codes.

The CLI adds a direct dependency on the existing owner instead of naming its
Ingress facade. The owner was already in its normal transitive closure through
Ingress; no implementation or contract node is added by that edge. Guards record
only actual measured changes without loosening their limits. SDK definitions,
versions, snapshots and wire/storage formats do not change in this cleanup.

At the checked Cloud continuation source `f6719247` and old main worktree
`b735ddb3`, none of these five symbols or an Ingress glob is used. This records
source compatibility impact, not a pinned Cloud build or publication result.

## Amendment 2026-10-19: platform plugin declaration leaves Execution

The CLI and scenario host consume the single platform plugin declaration from
`awaken-plugin-composition`, an `apps` process-bootstrap library. Discovery
metadata and pure extension-owned configuration admission are composed together;
ConfigService receives those declarations through the existing Config contract.
Workspace-dependent publication resolution stays at its existing owner.

SessionExecutor no longer produces the Config capability DTO or depends on
`awaken-executable-agent-contract`. No Platform Execution crate now depends on
Config, so the obsolete `execution -> config` context-pair allowance and the
SessionExecutor upward-edge exception are removed. This narrows the dependency
graph; it does not create an Execution-owned copy of the configuration catalog,
change the published discovery wire shape or alter Runtime plugin activation.

## Implementation note 2026-10-20: preserve Resource projection faults

Frozen projection installation consumes typed Run errors from the Resource
transition and staging owners through that same inverse projector. Missing
prior generations remain unavailable, invalid transitions remain request
faults, and Skill staging retains its owner code. Converting these values to
an Internal message would create a second classification path. Caller-level
tests exercise the actual installation boundary before correlated inputs are
published; no new failure class or retry policy is introduced.


## Implementation note 2026-10-20: one Session role forwarding boundary

`DeploymentSessionEnvironment` and `SessionObservation` remain the existing
caller-shaped Session capabilities. Their implementations consume the same
constructor-selected Environment/profile and Runtime coordination ports directly.
The matching inherent `SessionApplication::deployment_environment`,
`deployment_unavailable_delegate` and `subscribe_session_thread_live` entries are
removed. They carried no additional admission, persistence or translation law.
Managed's Deployment launcher uses the narrow role it already holds; the three
private `ManagedState` deployment forwarding methods are removed. It receives no
broader Runtime reservation/activation capability or underlying catalog handle.

Deployment first-refusal order and Session replay, exact Thread membership and
committed/live stream ownership remain unchanged. The existing application
constructors, Store/Clock, query/mutation/lifecycle services and effect owners are
not replaced by a new service, factory, subscriber registry or compatibility path.
These are Platform Rust entrypoints, outside the SDK22 version/snapshot promise.
The bounded two-Cloud-tree source search found no named callers; Flow's known
subscription adapter uses the unchanged Runtime contract, not these App entries.
Other concrete App callers must select the existing narrow role when upgrading
that pin. This source search is not unknown-downstream compile qualification.


## Implementation note 2026-10-20: one selected Session profile port

`SessionQueries` is the single holder of the constructor-selected
`ExecutableAgentProfileSource`. `SessionApplication` no longer retains a second
copy or synchronizes two selections. The existing Builder transfers the same
handle once; its repository, Runtime projection/capability roles and Clock
selection are unchanged. Profile, complete executable snapshot and custom
delegate-availability operations remain distinct calls to that source.

The application keeps its original budget-roster, frozen projection,
coordination and admission algorithms. Their source-presence refusals and exact
revision/error/effect ordering remain in place. Queries returns values, never
a source capability getter; it creates no publication or cache. The selected
handle may resolve its underlying control source on each call, as the existing
Flow handle does, so construction never captures its current catalog or value.

The existing `set_config_source` signature remains under its original
`cfg(any(test, feature = "test-support"))` boundary and exclusive-Queries check.
It selects that one component only; this does not introduce a production late
replacement operation. Flow's older pinned constructor still calls that entry,
so source inspection alone does not qualify a future downstream pin upgrade.
No public Session business entry, protocol wire, durable transaction or SDK
release surface changes in this bounded dependency-ownership cut.

## Implementation note 2026-10-20: Session Query read ownership

The existing `SessionQueries` component owns the frozen execution-input projection
and scoped transcript read cases. Its only constructor takes the repository,
a construction-only borrow of the already-selected `SessionExecutor`, and the
selected Agent profile handle. It stores only the original read roles, so an
independently supplied projected source cannot disagree with its raw reader.
The canonical committed-prefix adapter and all Thread/Run validation remain
unchanged; the raw role serves only exact committed-message reads here.

`SessionApplication::frozen_session_projection`, `session_transcript` and the
redundant `committed_messages` forwarding entry are removed. Callers use the
Queries business methods or the existing Runtime raw read role, without aliases.
No new query context, raw mirror, factory, cache or persistence authority exists.
Session commands retain their original lease, CAS, Clock and physical effects.

These are Platform source-path and constructor-signature changes, outside SDK22.
Known Awaken callers migrate together; bounded current Flow/two-Cloud source
searches found no calls to these three App entries or the Query constructor.
That evidence does not qualify an unknown downstream or an old pinned build.
The scoped read keeps its current-root ownership gate; historical prefix reads
keep their already-frozen reference and never require a live source projection.


## Amendment — 2026-10-20: one shared native filesystem owner

The policy-free native filesystem component previously named
`awaken-sandbox-fs` belongs to the existing storage boundary identified above.
Its code is received by `awaken-fs-durability`; the old safe crate and its public
namespace are removed, and every actual caller uses the same new owner. The
component owns physical inode/handle identity, no-follow file/directory effects,
atomic publication and bounded scans. Session leases, realization marker CAS,
Workspace identity, Repository mutations and provider retry decisions stay in
their existing callers. Paths, serialized identities, error projections and
native effect ordering do not change.

Directory flush remains the existing default API. One additive, default-off
`native-fs` compile capability selects the original native component; the five
normal FS consumers and the Executor fixture explicitly request it through
their dependency declarations. Missing selection is a compile error, never a
runtime fallback. Flush-only consumers do not request the capability, so this
edge retains its original default dependency contribution. Cargo can unify
features when another consumer in the same build requests native support; this
is not a promise that the complete CLI/workspace graph stays flush-only.

The original small Win32 unsafe isolation component moves as
`awaken-fs-win32`, in shared storage. Its checked safe interface and private
unsafe implementation remain separate from the safe owner: the safe crate
keeps the workspace unsafe prohibition. Local fixed NTFS, 64-bit physical
identity, ancestor pinning, reparse/link refusals, unsupported targets and
platform-specific contracts retain their existing meaning. This changes real
implementation ownership and callers, not the D3 recognizer or an exemption.
The old package is removed; no compatibility wrapper or alternate syscall
implementation is introduced.

Tests and their original decision tables move with the physical owner. The
existing portability executor explicitly requests native support and still
requires its original Windows long-path case. Default/native compile boundaries
and actual target-native execution remain distinct evidence. Source movement
or Linux tests do not revise historical Windows/macOS execution observations.
The Coordinator keeps its durable FS adapter, single-flight commit authority
and distinct Thread/Worker checkpoint layouts. The Store-FS D3 entry shrinks
only because that adapter no longer brings a Worker filesystem implementation.

These are Platform implementation paths, outside the SDK22 public surface.
The bounded Cloud/Flow source review found no import of the removed native
filesystem namespaces; no downstream compatibility alias is retained. Actual
Git-pin, archive and downstream consumption remain separate qualification, and
SDK versions, deep paths and release-cohort decisions do not change here.


## Implementation note 2026-10-20: canonical wake and durable capability declarations

The single two-method `WakeSignal` declaration belongs to the existing
`awaken-run-ingress-contract`, alongside its original `DispatchError` vocabulary.
Local notification and the cancellation/100ms publication budget remain in
`awaken-dispatch-execution`; PostgreSQL and NATS adapters and their factories
remain with Ingress. A hint still grants no queue, claim, cancellation-settlement
or terminal authority, and durable polling remains the recovery truth.

The existing five-method `RuntimeAuthority` and its unchanged typed error are
declared by `awaken-dispatch-erasure`, beside its existing LocalCommit and erased
Dispatch roles. This module opens no backend. Coordinator keeps its selected
Clock, process pool, per-Thread initialization and checkpoint policies; Executor
keeps SessionAuthority, Local/Upstream permission choices and the explicit
Ephemeral reference implementation. The declaration's wake type no longer
requires a reverse dependency on the execution implementation.

All known workspace consumers use those defining owners. Old public trait/error
exports from Executor and the old WakeSignal exports from Execution/Ingress are
removed, without aliases or duplicate declarations. Construction and effect
ordering are unchanged. The explicit direct dependency edges name actual new
source consumers; they are not a claim that Coordinator's remaining deployment
configuration or Host implementation dependency is closed.

These are Platform source paths, outside the SDK22 release boundary. Flow's
known pinned RuntimeAuthority implementation and WakeSignal return type need
canonical import/dependency migration when that pin is upgraded; no Flow/Cloud
source, pin, SDK snapshot, registry or persisted wire changes here. Source-only
controls and historical caller searches do not replace final-source compilation,
configured transport execution or real downstream qualification.


## Implementation note 2026-10-20: Session-owned File read observation

The existing `SessionQueries` now owns the Session-root File observation used
by a claimed Worker Run. The Resource HTTP boundary depends on the one-method
`SessionFileReadAuthorization` in `awaken-session-realization-contract`, rather
than opening the Session repository or interpreting its root. Queries reuses
its selected read-only Store; no new state, permission issuer, Runtime lookup,
cache, mutation boundary or durable grant is introduced.

The HTTP boundary still authenticates the Worker and holds the same exact live
Run claim guard through the File read. Its trusted request-start Clock sample
continues to judge Registry, claim and root-lease observations. The original
owner/read refusal order, frozen nonterminal baseline, exact Worker incarnation,
live realization lease and desired File membership stay unchanged. Missing or
unreadable root evidence continues to refuse the read. A frozen inline dispatch
manifest and an exact Session materialization intent remain distinct existing
authorities; the latter still uses its own generation/lease/target control.

`WorkerFileContentService::with_session_file_authorization` replaces
`with_session_repository`; no forwarding constructor remains. Coordinator
passes its already-constructed `SessionApplication::queries()` instance. The
real HTTP fixture also uses `SessionQueries` over its original SQLite Store and
the canonical `NeutralRuntime`, with no copied authorization implementation.

The known Flow router must likewise replace its old repository injection with
`with_session_file_authorization(self.application_session_control.queries())`
when moving to the matching Native source. Its observed `27b139c4` checkout still
pins Native `f6ccb85c`; the source migration is retained separately and does not
claim that old-pin Flow compiles against this new Platform API. Flow/Cloud Git
pins, locks, full consumers and deployment qualification remain independent.
No HTTP route, wire field, persisted representation or SDK22 release surface
changes in this cut.

## Amendment — 2026-10-24: crate names follow the context vocabulary

ADR-0091 D7 names a crate by context and responsibility and keeps "runtime"
for the SDK engine and qualified container engines. Seven workspace crates did
not, and their names are visible to awaken-cloud, so ADR-0091 D9 requires them
to change together and to be recorded here. Each rename moves the crate
directory, package name, every reference and every guard baseline or registry
keyed by the crate name; no alias or forwarding crate remains.

| Before | After | Responsibility the name states |
|---|---|---|
| `awaken-run-ingress-contract` | `awaken-dispatch-contract` | the Dispatch boundary: queue, claim, lease and settlement ports and values |
| `awaken-run-ingress` | `awaken-dispatch` | the Coordinator's durable dispatch queue (SQLite / PostgreSQL / NATS wake) and claimed-commit application |
| `awaken-run-ingress-http` | `awaken-dispatch-http` | the Coordinator's dispatch, durable-operation and claimed-commit HTTP interface |
| `awaken-run-ingress-testkit` | `awaken-dispatch-testkit` | the `DispatchQueue` conformance suite |
| `awaken-session-runtime-contract` | `awaken-session-execution-contract` | the Session ↔ execution boundary |
| `awaken-worker-runtime` | `awaken-coordinator-client` | the Worker's authenticated HTTP clients of the Coordinator |
| `awaken-store-runtime` | `awaken-sql-storage` | the SQL storage primitives every SQL store shares (exact BIGINT codec, PostgreSQL checkout policy) |

`awaken-dispatch` pairs with `awaken-dispatch-contract` (`<x>` ↔
`<x>-contract`). It does not collide with the two existing shared crates:
`awaken-dispatch-execution` is the one Worker/Pool execution path of an
already-claimed Run (bounded context Execution) and `awaken-dispatch-erasure`
binds the required dispatch roles once at process startup; neither owns the
queue. `awaken-coordinator-client` is named for the process it calls
(D7.4: `Coordinator` is the Coordinator process); its server side is
`awaken-dispatch-http`. `awaken-sql-storage` is not `awaken-store-<medium>`
(D7.1 reserves that for storage-medium backends) and not `awaken-postgres-pool`,
because its codec is shared with the SQLite stores. The crates'
`metadata.awaken.authority` labels follow the names (`durable-dispatch`,
`dispatch`, `dispatch-http`, `session-execution`, `coordinator-client`).

Also renamed in the same delivery, inside the Platform only: the Session
executor's `slot` / `slots` wording for an installed Session becomes
`installed` / `installed_sessions` (plan v4 D9); a genuine capacity or physical
attempt slot keeps its name. Twenty test functions were renamed and recorded in
the test inventory ledger. The unused hidden re-export
`awaken_worker_runtime::{claimed_stream, execution_capacity, worker_context}`
is deleted; those modules are imported from `awaken_dispatch_execution`.

Unchanged: no wire field, route, persisted byte, SQL object, environment
variable or CLI flag contains these crate names. Names that keep an old word
on purpose: the formal obligation ids `run_ingress.*` (the formal ledger's
stable identities), the design document `run-ingress-message-delivery.md` and
ADR-0009's title (historical names), the Kubernetes service account and
RoleBinding `worker-runtime` / `awaken-worker-runtime` in `deploy/k3d`
(deployed resource names), and the persisted golden fixture
`session_runtime_projection.json` with the types `SessionRuntimeProjection` /
`SessionRuntimeEnvelope`. Tracing targets derive from crate paths, so a log
filter such as `awaken_run_ingress=debug` must be written `awaken_dispatch=debug`.

The naming guard now holds two more ratchets (`_naming_fitness.py`):
`crate-name` (a non-SDK crate name with `runtime`, or a mechanism word outside
a composition root; baseline `awaken-mcp-server-core`, `awaken-scenario-host`)
and `runtime-word` (a non-SDK public type whose name has the word `Runtime`,
other than a qualified container engine; baseline 39). The remaining
`XxxRuntime` Session roles and `Runtime*` capability types are renamed when
their code moves (ADR-0091 D9), not in a separate sweep.

Not done here: `SessionEnvironmentProvider` (the record of Session environment
role handles) and its adapter `SandboxEnvironmentProvider` are not yet merged
into `SandboxProvider`. That merge removes the five Session environment role
traits in favour of the one supply abstraction; it is a design change across
the realization and executor crates, not a rename, and is made with that code.

awaken-cloud must, when it moves its Native pin past this amendment, rename the
dependency keys and `use` paths as follows (read from its `main` branch; Cargo
lock files regenerate):

| awaken-cloud path | Change |
|---|---|
| `Cargo.toml` | `awaken-run-ingress-contract` → `awaken-dispatch-contract` |
| `crates/awaken-cloud-billing/Cargo.toml`, `src/compute_metering.rs` (3) | `awaken[-_]run[-_]ingress[-_]contract` → `awaken[-_]dispatch[-_]contract` |
| `crates/awaken-hosted-run/Cargo.toml`, `src/completion.rs`, `src/lib.rs`, `src/state_model.rs`, `tests/hosted_run.rs` | same contract rename |
| `crates/awaken-model-access/Cargo.toml`, `src/inference_candidate.rs`, `src/lib.rs`, `tests/materialize.rs` | same contract rename |
| `crates/awaken-hosting-durable/Cargo.toml` | `awaken-run-ingress` → `awaken-dispatch`, `awaken-run-ingress-contract` → `awaken-dispatch-contract`, `awaken-run-ingress-http` → `awaken-dispatch-http` |
| `crates/awaken-hosting-durable/src/{completion,compute,lib,placement,query_consumption,telemetry,worker_model_access}.rs` | `awaken_run_ingress[_contract]` → `awaken_dispatch[_contract]` |
| `crates/awaken-hosting-durable/tests/{deployed_cluster_causal,phase1_cross_process}.rs`, `tests/support/retained_session.rs` | same |
| `Cargo.lock`, `crates/{awaken-cloud-management,awaken-hosting-durable,awaken-model-access,awaken-pro}/Cargo.lock` | regenerate (they also list `awaken-session-runtime-contract`, `awaken-store-runtime`, `awaken-worker-runtime` transitively) |
| `docs/adr/0021-…`, `docs/adr/0022-…`, `docs/design/open-seam-contract.md` | historical text; links only if they must resolve |

awaken-cloud source imports none of `awaken_session_runtime_contract`,
`awaken_store_runtime` or `awaken_worker_runtime` directly. This source review
does not certify an awaken-cloud build at a new pin.
