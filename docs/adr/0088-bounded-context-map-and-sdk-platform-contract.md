# ADR-0088: Bounded Context Map and SDK–Platform Contract

- Status: Accepted
- Date: 2026-10-07
- Builds on: ADR-0081 contract layering and ratchet guardrails; ADR-0086 Runtime SDK
  public surface; ADR-0087 SQLite through sqlx.
- Affects: crate `bounded_context` metadata, the crate-boundary guards, and
  awaken-cloud only through later renames (none in this ADR).
- Amended by: ADR-0089 (Accepted: decision 2 separates Execution's realization
  effects from the independent Session driver shared by both topologies)
- Amended by: ADR-0091 (Proposed: the 2026-10-07 amendment sentence on not merging
  the small Session contracts)
- Amended: 2026-10-09 (reconciled with ADR-0081: public-API snapshots are an
  `api_snapshot` opt-in)
- Amended: 2026-10-09 (reconciled with ADR-0086: credential vocabulary lives in
  `awaken-agent-contract`)
- Amended: 2026-10-10 (the dispatch contract stops depending on Session contracts;
  public paths that moved and their awaken-cloud impact)
- Amended: 2026-10-11 (provider checkpoint capture input has its own name;
  Session authorization and checkpoint object bytes are unchanged)

## Context

Crates already carry `product` and `bounded_context` metadata, and guards ratchet
dependency direction between contexts. The tactical model is sound: the Session
aggregate and its pure transition laws live apart from persistence and application
services; `SessionRepository`, the lifecycle outbox and the Managed timeline
projection are standard.

The strategic layer is not. Contexts were drawn by where code runs, not by which
domain problem it solves:

- `session` holds 26 crates that mix four concerns: the Session lifecycle, the Run
  execution host (`awaken-session-executor`, `awaken-session-mcp`), the runtime's thread
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
   - **Execution** (new): `awaken-session-executor`, `awaken-session-mcp`, the future
     `awaken-session-realization`, `awaken-run-executor-acp`, `awaken-run-executor-a2a`,
     `awaken-execution-composition`, `awaken-agent-channel`, `awaken-acp-contract`,
     `awaken-dispatch-execution`, `awaken-dispatch-erasure`. The latter owns
     immutable execution bindings and role projections; Dispatch's published
     leaf ports remain in `awaken-dispatch-contract`. Backend adapters and
     Worker transports consume that binding through their existing execution
     entrypoint, without a second cross-context infrastructure dependency.
   - **The `worker` context is retired.** `awaken-coordinator-client`,
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
   - `awaken-session-execution-contract` is split by direction when it is next changed:
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
not own them. `awaken-dispatch-contract` still owned six of them, so its contract
depth was 9. They now live with the Session, and the dispatch contract's depth is 6.
Its one remaining Session dependency, `awaken-session-configuration-contract`, is
legacy-only: claim admission decodes the runtime projection of rows written before
the Session published typed MCP credential requirements. It is removed once those
rows have drained. Persisted bytes did not change; golden tests in
`crates/contract/awaken-dispatch-contract/tests/persisted_wire_golden.rs` pin
them.

Public paths that moved (ADR-0091 D9 requires recording them):

| Before | After |
|---|---|
| `awaken_dispatch_contract::{ClaimedSessionControlError, ClaimedSessionRealizationControl}` | `awaken_session_realization_contract::…` |
| `awaken_dispatch_contract::ClaimedSessionCoordinationControl` | `awaken_session_execution_contract::ClaimedSessionCoordinationControl<Claim>` (Host uses `<RunClaim>`) |
| `awaken_dispatch[_contract]::SessionRunAdmissionObserver` | `awaken_dispatch_execution::SessionRunAdmissionObserver` |
| `awaken_dispatch[_contract]::{ResourceOperationFence, ArtifactPublicationFence}` | `awaken_session_realization_contract::ResourceOperationFence`; the alias is deleted |
| `awaken_dispatch_contract::run_dispatch::DispatchedSessionRuntimeProjection` | `awaken_session_configuration_contract::SessionRuntimeProjection` |
| `awaken_session_execution_contract::{SessionRunReplacement, SessionRunCommandFingerprint}` | `awaken_session_types::…`; `SessionRunCommandFingerprint::current(&cmd)` is `cmd.command_fingerprint()` |
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
source and lock identities. Host retains Run assembly and claimed-dispatch routing
until the separate E/H slices. The independent Session driver remains unchanged.

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
