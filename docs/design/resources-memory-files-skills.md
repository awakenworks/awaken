# Resources, Memory, Files, And Skills

This document is the normative design for platform-managed resource inputs in
awaken. It defines the static domain model, the end-to-end lifecycle, the owner
of every stage, recovery and reclamation, and the boundary with authorization
and Runtime Core. [ADR-0063](../adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md)
owns the load-bearing identity and version decisions.

## Scope and Ubiquitous Language

This design covers three Agent input resources:

- **File** — a Workspace-scoped logical `FileId` referencing one immutable,
  content-addressed blob;
- **MemoryStore** — mutable, Workspace-owned long-term state;
- **Repository** — mutable reference to an external Git repository.

A resource may be an **Agent default input** or a **Session attachment**. Those
are two sources for one binding language, not two resource models.

The key terms are:

| Term | Meaning |
|---|---|
| Resource identity | stable logical typed id: `FileId`, `MemoryStoreId`, or `RepositoryId` |
| Binding | identity + mount path + access + optional instructions |
| Revision | optimistic-concurrency counter on a mutable authoring aggregate |
| Config version | immutable published Memory/Repository configuration |
| Content version | Memory entry version or Git revision; never a Memory/Repo binding pin |
| Resolved Session resources | one secret-free manifest containing resolved inputs and separately frozen Skill capabilities |
| Activation | Session-local realization of one resolved input |
| Reclamation | asynchronous physical cleanup after logical denial and reference/lease checks |

An Artifact is output, not an input kind. It can later become a File through an
explicit publish/attach operation. A Skill is a versioned capability bundle with
tool-policy implications; it remains in the Skill domain and is not forced into
the File/Memory/Repository lifecycle.

Awaken knows Org/Workspace at the platform edge and Workspace inside resource
operations. It does not know Project or WorkUnit. A higher-level product maps its
own scopes and work lifecycle before it invokes this boundary.

## Owning Contexts

| Concern | Owning bounded context |
|---|---|
| Resource definitions, config versions, current lifecycle state | Resources |
| Agent default input associations | Agent Configuration |
| Temporary attachments and effective input manifest | Session |
| Principal, policy, action-to-scope applicability | Authorization/IAM |
| Immutable File bytes and logical File metadata | Resources / File aggregate |
| Mutable Memory entries, history, redaction | Resources / MemoryStore aggregate |
| External Git truth | remote Git provider; awaken owns only config and Session working tree |
| Mounts, working trees, live handles | Environment/Sandbox provisioning |
| Runtime facts, messages, decisions, verdicts | Runtime Core |

Runtime Core does not load resource configuration, parse product resource DTOs,
resolve credentials, or authorize access. Product/session code hands the
environment a complete logical input manifest; the environment creates local
paths and handles.

## Static Domain Model

### Binding model

```rust
enum InputResourceId {
    File(FileId),
    MemoryStore(MemoryStoreId),
    Repository(RepositoryId),
}

struct InputBinding {
    binding_id: BindingId,
    target: InputResourceId,
    mount_path: SandboxPath,
    access: ResourceAccess,
    instructions: Option<String>,
}

struct AgentInputConfig {
    agent_id: AgentId,
    bindings: Vec<InputBinding>,
    revision: Revision,
}

struct SessionInputAttachment {
    binding: InputBinding,
    replaces: Option<BindingId>,
}
```

`AgentInputConfig.revision` protects concurrent edits. It is not an execution
pin. Agent Memory/Repository bindings contain no config version.

### Resource aggregates

```rust
struct File {
    id: FileId, // logical Workspace-scoped identity
    blob_id: BlobId, // BLAKE3 immutable content identity
    workspace_id: WorkspaceId,
    size: u64,
    filename: Option<String>,
    media_type: Option<String>,
}

struct MemoryStore {
    id: MemoryStoreId,
    workspace_id: WorkspaceId,
    state: ResourceState,
    current_config_version: ConfigVersion,
}

struct MemoryStoreConfigVersion {
    memory_store_id: MemoryStoreId,
    version: ConfigVersion,
    retention_policy: RetentionPolicy,
}

struct Repository {
    id: RepositoryId,
    workspace_id: WorkspaceId,
    state: ResourceState,
    current_config_version: ConfigVersion,
}

struct RepositoryConfigVersion {
    repository_id: RepositoryId,
    version: ConfigVersion,
    remote_url: RepositoryUrl,
    credential_binding: CredentialBinding,
    initial_branch: Option<BranchName>,
    clone_policy: ClonePolicy,
}
```

Memory configuration never contains a physical database path or node locator.
The logical `MemoryStoreId` continues to route to one content truth when storage
adapters change. Repository configuration contains a secret reference, never the
secret value.

### Resolved Session model

```rust
struct ResolvedSessionResources {
    inputs: Vec<ResolvedInput>,
    skills: Option<Vec<ResolvedSkillBinding>>,
}

struct ResolvedSkillBinding {
    skill_id: SkillId,
    version: u64,
    bundle_sha256: Sha256,
}

struct ResolvedFileInput {
    binding_id: BindingId,
    file_id: FileId,
    mount_path: SandboxPath,
}

struct ResolvedMemoryInput {
    binding_id: BindingId,
    memory_store_id: MemoryStoreId,
    config_version: ConfigVersion,
    access: MemoryAccess,
    mount_path: SandboxPath,
}

struct ResolvedRepositoryInput {
    binding_id: BindingId,
    repository_id: RepositoryId,
    config_version: ConfigVersion,
    remote_url: RepositoryUrl,
    credential: Option<ResolvedRepositoryCredential>,
    initial_branch: Option<BranchName>,
    access: RepositoryAccess,
    mount_path: SandboxPath,
}

struct ResolvedRepositoryCredential {
    access: CredentialAccess,
    selected_plaintext_holder: PlaintextHolder,
}
```

The manifest is serializable and secret-free. It contains no host path, live
handle, credential value, Memory entry version, or Git commit. The immutable
Repository config retains its source binding; the optional resolved credential
adds only an exact source revision, canonical usage, execution policy, and
Environment-selected holder. An anonymous Repository has no credential pin.

At the durable execution boundary it is wrapped, not copied into another model:

```rust
struct SessionResourceManifest {
    workspace_id: String,
    resources: ResolvedSessionResources,
}
```

`RunDispatch` persists this envelope beside `execution_scope`. A claiming worker
must prove the two Workspace coordinates are equal, install the manifest through
its injected resource ports, and only then create or adopt the Session sandbox.
The envelope never carries a principal, policy decision, credential value, local
path, Memory entry revision, or Repository commit.

The dispatch bounded context treats `resources` as an opaque serialized payload;
one Runtime Host ACL performs the lossless encode/decode against
`ResolvedSessionResources`. This preserves the typed Session model without adding
a Dispatch-to-Session dependency edge or duplicating resource resolution.

`skills = None` identifies a legacy Session; `Some([])` is an explicit empty
selection. Skill bundle bytes remain in the Skill repository and are loaded by
the frozen id/version/hash only.

A Skill-version delete retires it from current management views while retaining
the immutable bytes for existing pins. Version ordinals are monotonic and never
reused. Recovery installs `ResolvedSessionResources` before opening the runtime
commit history, so context construction cannot consult or materialize today's
Agent/Skill configuration into an older Session.

### Activation model

```rust
struct SessionResourceActivation {
    activation_id: ActivationId,
    session_id: SessionId,
    revision: u64,
    binding_id: BindingId,
    resource_id: InputResourceId,
    access: ResourceAccess,
    state: ActivationState,
    attempts: u32,
    lease_expires_at: Option<Timestamp>,
    last_error: Option<String>,
}

enum ActivationState {
    Prepared,
    Active,
    Releasing,
    Released,
    Failed,
}

struct SessionResourceState {
    revision: u64,
    active: ResolvedSessionResources,
    pending: Option<ResolvedSessionResources>,
    activations: Vec<SessionResourceActivation>,
}
```

An activation record is Session application state used by crash recovery and
reclamation. `pending` and its Prepared records are committed before Host or
worker IO; `active` changes only after realization commits. A failed synchronous
replacement first re-applies `active`; if rollback also fails, the pending state
remains discoverable by the reclaimer. The record stores neither a secret nor a
process-local handle.

## Resource Input Component Catalog

| Component | Status | Owner | Responsibility | Must not own |
|---|---|---|---|---|
| `ResourceCatalog` / `ResourceConfigSource` / `ResourceCatalogRules` | Existing | Resource Catalog | Memory/Repository definitions, immutable config versions, current version and lifecycle state; the narrow resolution port returns only the selected config version; one backend-neutral rules object defines creation, publication, lifecycle, and config-identity invariants for every adapter | content bytes, IAM policy/decision/envelope, principal, sandbox paths |
| `ResourceBindingValidator` | Existing | Resource Catalog | at activation/use validate trusted Workspace ownership, live state, and the exact frozen config version without returning or re-selecting configuration | authorization decisions, current-version substitution, secret material |
| `AgentInputBindingRepository` | Existing | Agent Configuration | Workspace-scoped Agent default bindings and authoring revision; every operation requires Workspace | Session merge, content resolution, authorization |
| Managed Session adapter | Existing | Protocol/product ACL | parse/project Anthropic resources; accept temporary attachments | raw DTO leakage into neutral/resource services |
| `SessionInputResolver` | Existing | Session control plane | merge once, replace explicitly, validate mount paths, select current config versions, create `ResolvedSessionResources` and prompts | secret material, runtime loop, physical mounts |
| front-door PEP | Existing/evolving | Server edge | authenticate, construct trusted Workspace target, call PDP, enforce obligations | resource content and domain policy implementation |
| authorization PDP/PIP | External/shared authorization domain | IAM | decide principal/action/scope/resource facts under active policy | mounts, resource configuration, storage |
| `SessionResourceCoordinator` | Existing in Managed Session application service | Session application/host | activation state, ordered provision/release, recovery handoff | Agent config loading, IAM policy language |
| `ResourcesApplication` | Implemented canonical application assembly | Resources | derive the one File command service and purge scheduler from one `ResourceComponent`; expose the same ports to HTTP, Runtime artifact harvesting, and cleanup | HTTP, Runtime, Coordinator, IAM, or a second resource aggregate |
| `FileApplicationService` / `FileApplication` | Implemented sole File command path | Resources | logical File get/list/create/bytes/delete, quota, idempotent artifact harvesting, ownership reference ordering, and purge intent | route DTOs, Runtime execution, direct physical reclamation, or parallel catalog writes |
| `FileStore` | Existing | Resources / File aggregate | immutable content-addressed bytes | Workspace authorization; mutable overwrite |
| `FileContentSource` | Implemented boundary port | Worker/File boundary | resolve one exact Workspace-scoped public `FileId` under the current claim and verify the immutable digest before read-only materialization | File ownership, mutable write-back, private blob-id authority, database access |
| `StoreFileContentSource` | Implemented local adapter | Resources / File aggregate | reuse `FileCatalog` then `FileStore` as the sole logical-to-content resolution path | Worker identity, dispatch ownership, HTTP, a second catalog |
| `HttpFileContentSource` / Worker File handler | Implemented network adapters | Worker/File boundary | carry Workspace, File, and exact claim; authenticate, prove the File belongs to the frozen dispatch manifest, hold the claim guard through the read, and verify the response digest again on Worker | authoring/list/delete, generic Resource dispatch, Worker database access |
| K8s live File projector | Implemented Environment adapter | K8s Container provider | seed `/mnt/session/uploads` once, expose the shared volume read-only to the Agent, replace/remove later File generations only through an isolated sidecar, and freeze that capability in the durable Sandbox handle for adoption | File identity/content truth, arbitrary-path hot mounts, writable Agent access, ServiceAccount credentials |
| `MemoryRepository` | Existing canonical port | Resources / MemoryStore aggregate | scoped entries, one atomic `snapshot_heads` primitive shared by copy materialization, recovery, and Recall, CAS, atomic history, redaction, retention hooks | Agent/Session binding and IAM policy; callers must not reconstruct a snapshot with list-plus-read |
| `HttpMemorySnapshotSource` / `HttpMemoryWritebackClient` | Implemented network adapters | Worker/Memory boundary | obtain one atomic snapshot and apply claim-fenced CAS create/update/delete-if-match under the frozen Workspace/store/config/access binding | config selection, silent overwrite, authoring/history/purge, Worker database access |
| `MemoryRuntime` | Existing, moving to extension ownership | `awaken-ext-memory` | recall Plugin, terminal extraction observer, selector/extractor capability, stable intent/receipt | resource identity, default store, IAM policy, Host lifecycle |
| `BoundMemory` | Existing | Session Runtime | one resolved store handle + pinned policy + maximum access shared by recall/extraction | workspace lookup, current-config resolution, authorization |
| `RepositoryRealizer` | Existing neutral port | Environment adapter | clone current remote config, construct working tree, publish Agent-authored commits with ephemeral transport credentials | remote repository ownership, authorization policy, or commit pinning |
| `RepositoryBindingVerifier` | Implemented boundary port | Worker/Repository boundary | verify one exact frozen Workspace/Repository/config binding before Git realization/use | configuration selection, Git transport, credential material, generic Resource dispatch |
| `CatalogRepositoryBindingVerifier` | Implemented local adapter | Resource Catalog | delegate the exact live check to the existing `ResourceBindingValidator` | claim or HTTP policy, a second catalog |
| `HttpRepositoryBindingVerifier` / Worker Repository handler | Implemented network adapters | Worker/Repository boundary | authenticate the current Worker, prove the exact binding belongs to the frozen dispatch manifest, and hold the claim guard through catalog validation | Git bytes, clone/publish behavior, plaintext credential, Worker database access |
| Repository credential pin compiler | Existing in Managed Session application service | Session application/Vault ACL | compile a Repository config binding once into exact active source revision, canonical usage, `Forbidden` exposure, and selected Resource holder before persistence | material opening, Runtime lookup, generic Service state |
| `CredentialMaterialResolver` | Existing canonical port | Credential execution boundary | validate and open one exact access/holder/Workspace/target-use binding for an installed adapter; shared by Model, MCP, and Repository | source enumeration, revision/holder/target selection, Agent prompt, persisted plaintext |
| `SkillBundleSource` | Implemented boundary port | Worker/Skill boundary | retrieve and verify one exact immutable custom capability bundle under a live claim | generic Resource lifecycle, Skill policy selection, database access |
| `StoreSkillBundleSource` | Implemented local adapter | Resources / Skill aggregate | load the exact frozen version from the authoritative `SkillStore` and validate its identity and digest | Worker identity, dispatch ownership, HTTP, a second Skill catalog |
| `HttpSkillBundleSource` / Worker Skill handler | Implemented network adapters | Worker/Skill boundary | authenticate the current Worker, prove the exact custom binding belongs to the frozen dispatch manifest, hold the claim guard through the read, and verify the returned bundle again on Worker | authoring/list/delete/purge, built-in Skill transport, Worker database access |
| `SandboxProvider` | Existing | Environment provisioning | realize validated mounts/working trees and dispose them | product resource authoring and policy |
| `ResourceReclaimer` | Existing, durable and per-resource | Product/session operations | reconcile crashed activations and purge intents; retention, reference checks, fenced claims, per-kind receipts | authorization decisions, remote Git deletion |
| `ResourceReclamationFence` | Existing resource lifecycle port | Resource consistency | atomically prove zero physical references, fence `(kind, resource_id)`, and reject racing reference writes | principal, role, policy, API key, Org/Project/WorkUnit |
| `ResourceComponent` | Existing canonical port assembly | Resources composition | select one Resource Catalog, File, Memory, Skill, and lifecycle implementation family | application command ordering, IAM/PDP data, authorization decisions |
| `ResourcesRouterInput` / `resources_router` | Implemented HTTP composition | Resources adapter | mount File, Memory, and Skill route families exactly once over application ports | stores, business state, Worker claims, or another resource service |
| `SqliteResourceStore` / `PostgresResourceStore` | Existing adapters | Resource consistency persistence | persist purge intents, intrinsic references, and reclamation fences for embedded or multi-node deployment | IAM/PDP data and File/Memory/Skill content |

The catalog names roles rather than forcing them into one crate. Local mode may
compose several roles in one process; cloud mode may deploy them separately.
Their contracts and ownership stay the same.

### Distributed composition

The common boundary ends at `SessionResourceManifest`. A distributed Worker
receives the same frozen, secret-free value as AllInOne, then invokes the narrow
adapter for each resolved kind:

```text
SessionResourceManifest
  |- File       -> FileContentSource -> read-only mount
  |- Memory     -> MemorySnapshotSource -> MemoryMounter
  |                `- MemoryWritebackClient (CAS)
  |- Repository -> CredentialMaterialResolver -> RepositoryRealizer
  `- Skill      -> SkillBundleSource -> capability load
```

The File branch is implemented. The Session manifest continues to carry its
existing public `FileId`; it does not expose the private content-store key. In a
distributed read, the File handler validates the authenticated Worker's exact
claim, Workspace execution scope, and frozen File binding while holding the
dispatch epoch guard. It then reuses `StoreFileContentSource`. The Worker stages
the returned bytes through the existing `InlineBytes` mount path and independently
checks the digest. This removes the earlier direct staging branch instead of
maintaining local and remote materialization algorithms. It also removes the
old post-staging direct catalog lookup: immutable File existence and integrity
are decided once by the per-kind materialization port, while mutable/configured
Resource kinds retain their operation-time binding checks.

The Memory branch is implemented without a second content model. Copy
materialization, recovered-copy reconciliation, and Recall all call the existing
atomic `MemoryRepository::snapshot_heads`; list-plus-read snapshot emulation has
been removed. Distributed staging adds a process-local
`materialization_reference` to the provisioning requirement while preserving the
logical `MemoryStoreId`. The reference binds Workspace, exact config version,
maximum access, and `RunClaim`, but is not persisted in the Session manifest.
`HttpMemorySnapshotSource` and `HttpMemoryWritebackClient` project only snapshot
and runtime mutations. The handler authenticates the current Worker, holds the
claim epoch guard, matches the frozen manifest, validates live Resource state,
and rejects write-back for read-only input. The existing copy/harvest algorithm
then supplies CAS conflict and delete-if-match behavior across the network.

The Skill store's latest-version view is likewise one backend-owned atomic
operation. `SkillCatalog` calls `snapshot_latest_versions`; it no longer rebuilds
that view by listing definitions and fetching each mutable latest pointer in a
separate operation. Exact custom bundles use the same `SkillBundleSource` port
locally and remotely. The HTTP handler proves the current Worker incarnation,
live claim, Workspace, and frozen kind/id/version/hash before it reads the store;
the Worker recomputes the digest before activation. Built-in Anthropic Skills
remain runtime-owned and never cross the Resource boundary.

The Repository branch reuses the existing `RepositoryRealizer` and exact
credential pin; it does not proxy Git content. `RepositoryBindingVerifier`
supplies only the missing live invariant check. Its HTTP handler verifies the
current Worker incarnation, claim, Workspace, Repository id, config version, and
frozen manifest before delegating to the Resource Catalog. The Worker then calls
the unchanged environment realizer with the frozen secret-free plan and
ephemeral credential. The former direct Worker catalog connection and empty
composition marker were removed.

`ResourceComponent` selects local implementations at the Resources composition
root. `ResourcesApplication` adds command ordering once and remains free of HTTP
and Runtime concerns. A separately deployed provider retains its per-kind
contract and data authority. The design does not add a universal
`ResourceService` or `ResourceMaterializer`.

## Lifecycle Stage Ownership

The following matrix is normative. Every transition has one orchestration owner;
resource-specific repositories enforce their own intrinsic invariants.

| Stage | Primary component | Collaborators | Durable result |
|---|---|---|---|
| Configure File | `FileApplication` | `FileStore`, `FileCatalog`, lifecycle repository | immutable blob plus logical `FileId` and Workspace/reference edge |
| Configure Memory | Resources application | Resource Catalog, `MemoryRepository` | `MemoryStore` + config v1 + logical namespace |
| Configure Repo | Resources application | Resource Catalog, Vault | `Repository` + config v1 referencing credential binding |
| Bind Agent default | Agent Configuration service | `AgentInputBindingRepository`, PEP/PDP | identity-only `InputBinding`, authoring revision increments |
| Attach to Session | Managed Session adapter | PEP/PDP | temporary `SessionInputAttachment` |
| Resolve | `SessionInputResolver` + Repository credential pin compiler | Agent binding repo, Resource Catalog, Vault source metadata, frozen Environment | `ResolvedSessionResources`; Memory/Repo config versions and exact Repository credential execution pin selected once |
| Activate File | `SessionResourceCoordinator` | `FileContentSource`, `SandboxProvider` | immutable-source working copy with no write-back + activation `Active` |
| Activate Memory | `SessionResourceCoordinator` | `MemoryRepository`, Memory realizer | `ScopedMemoryStore`/mount + activation `Active` |
| Activate Repo | `SessionResourceCoordinator` | Repository realizer, exact material resolver, Sandbox | current clone + working tree; exact pin persists, material does not |
| Use | sandbox/tool adapters | File/Memory/Git domain ports | domain writes and receipts; no second config resolve |
| Recall for Run | Memory Recall Plugin | scoped Memory handle, optional Selector Agent | Run-scoped request-only `ContextMessages`; query derives from current `RunInput` |
| Extract after terminal Run | Memory Extraction terminal observer | committed Run/Thread facts, Extractor Agent, Memory repository | stable extraction intent and receipt; at-least-once delivery is idempotent |
| Release | `SessionResourceCoordinator` | Sandbox manager, Vault, per-kind realizer | activation `Released`; sandbox-local material removed |
| Reconcile crash | `ResourceReclaimer` | Session activation repository, workers | stale activation released or retried |
| Archive/Delete | Resources application service | PEP/PDP, resource repository | live deny state/tombstone plus deterministic purge intent before physical cleanup |
| Fence physical identity | `ResourceReclaimer` | `ResourceReclamationFence`, reference writers | durable zero-reference fence; racing bindings fail closed |
| Purge | `ResourceReclaimer` | per-kind store and reference indexes | idempotent physical deletion followed by auditable purge receipt |

Resource services receive trusted Workspace coordinates and authorized operation
intent. They do not receive API keys, roles, IAM syntax, Project, or WorkUnit.

## Common Configure-to-Reclaim Flow

```text
Resources API
          |
          v
ResourcesApplication / FileApplication
          |
          v
Resource Catalog and per-kind stores
          |
          v
Agent default binding  +  Session temporary attachment
          |                         |
          +------------+------------+
                       v
             front-door PEP/PDP
                       |
                       v
             SessionInputResolver
                       |
                       v
             ResolvedSessionResources
                       |
                       v
          SessionResourceCoordinator
                       |
       +---------------+----------------+
       |               |                |
       v               v                v
   File mount     Memory handle      Repo clone
       |               |                |
       +---------------+----------------+
                       v
                    Agent use
                       |
                       v
              release activation
                       |
                       v
       ResourceReclaimer / per-kind GC
```

### Merge and replacement

Agent defaults are read from the current `AgentInputConfig`. Session attachments
may add a binding or explicitly replace one by `binding_id`.

```text
different mount path             -> merge
same mount path without replace  -> reject
explicit replacement             -> re-authorize and replace
requested lower access           -> allow after normal validation
requested higher access          -> requires a new authorization decision
```

Array order never grants precedence. Prompts are rendered once from the final
effective set, so what the Agent is told is what the environment provisions.

## File Lifecycle

### Configure

The Files API delegates to `FileApplication`, which checks the Workspace quota,
stores immutable bytes, creates one logical File record, and writes its ownership
reference through the lifecycle repository. `FileStore` computes the BLAKE3
`blob_id`; equal bytes may share that blob while distinct uploads retain distinct
logical `FileId` values. Knowing either id is never authority.

### Bind and resolve

Agent and Session bindings carry the logical `FileId`. Resolution checks the
Workspace ownership edge, current deletion state, mount path, and referenced blob
existence. No File config or version lookup exists.

### Activate and use

The Worker asks `FileContentSource` for the logical File under its current claim.
The Resources adapter resolves `FileCatalog` to `blob_id`, reads `FileStore`, and
both sides validate the immutable digest. The Sandbox provider materializes a
disposable Session working copy;
the copy may be edited, but it has no write-back path to the immutable FileStore
object. Text conversion is forbidden. Edited bytes become a new File or Artifact
only through an explicit publish; the original blob is never overwritten.

### Release and reclaim

Session release deletes only the sandbox copy. Logical File deletion revokes a
Workspace ownership edge. Physical blob GC is allowed only when all are false:

```text
Workspace ownership edges
Agent bindings
active/effective Session references
Artifact references
retention or legal hold
```

A shared blob survives removal of one Workspace's ownership edge.

## MemoryStore Lifecycle

### Configure

The Resources application is the sole command path for Workspace-owned
`MemoryStore` identity, state, retention, and purge scheduling. It creates
config v1 and ensures the Resources aggregate recognizes the logical store id.
Config versions carry retention only; recall and extraction are not resource
policies. The Anthropic MemoryStore API exposes no Awaken-only config/version
route.

A Memory head update is likewise one resource-aggregate command. The public API
passes content, optional target path, and the head SHA precondition to
`MemoryRepository.update_head`; validation, CAS, rename-replace, and history are
one transaction. The API never sequences a content write followed by a second
rename. This invariant belongs to Resources and contains no principal,
role, policy, API key, or authorization decision.

### Bind and resolve

Agent and Session bindings carry only `MemoryStoreId`. At Session creation the
resolver freezes the current resource config version for lifecycle validation.
All Sessions still address the same mutable logical store. `recall_enabled`,
`extraction_enabled`, recall bounds, extraction/selector Agent ids, and their
online prompt overrides come only from the parent Agent's `memory` plugin
configuration.

### Activate and use

After live ownership/state checks, `MemoryRepository.open(workspace, store_id)`
returns a capability-limited `ScopedMemoryStore`. Recall, extraction, mounted
file operations, public Memory API operations, history, and redaction all use
that same repository.

Recall is an in-Run `BeforeInference` Plugin. It derives its relevance query from
the current `RunInput`, writes request-only context to Run-scoped
`ContextMessages`, and reuses that committed selection across later Steps and
Resume of the same Run. It does not infer a query by scanning the last User
message in the whole Thread.

Extraction is not a Step hook, continuation guard, or Host callback. After a
terminal `RunState::Ended` fact commits, the Memory Extension's
`RunTerminalObserver` receives an at-least-once observation and creates or reuses
`memory-extraction/{thread_id}/{run_id}`. The Extractor Agent runs asynchronously,
CAS-applies mutations, and records a receipt. Recovery redelivers a terminal
observation or resumes a pending intent; Awaiting is not terminal. Observer
failure cannot alter the committed `RunResult`.

```text
Session S1 resolves config v3
Session S2 resolves config v4
S1 and S2 both read current MemoryStore content
entry CAS/version prevents silent lost updates
```

`ReadOnly` is enforced by the handle/realizer. Extraction cannot write through a
read-only binding. Internal entry `version` and `content_sha256` remain CAS/API
data and are not binding pins.

A Managed Dream is deliberately not an ordinary mutable Session Memory binding.
It captures a purpose-built `MemoryStoreContentSnapshot`, mounts that evidence
read-only, clones an independent result MemoryStore from the same exact heads,
and gives its Dream Agent a required write-through mount of only
the result. The complete lifecycle and naming are owned by
[Managed Dream](managed-dream.md).
Ordinary Sessions may use ADR-0053's conflict-safe copy/harvest fallback; a Dream
result may not, because every accepted partial result must already be durable.

### Release and reclaim

Release closes/detaches the scoped handle; it never deletes the store. Reliable
extraction must reach a durable receipt or a durable retry/terminal conclusion
before the activation is considered fully settled. The Runtime Host supplies the
scoped handles and durable adapters but does not decide extraction eligibility,
cursor movement, prompt policy, or mutation semantics.

Suspension denies new opens and later writes. Deletion creates a tombstone,
rejects all operations, drains active handles and extraction jobs, applies
retention/redaction rules, purges the unified content/history transactionally,
and records a purge receipt. Store ids are never reused.

## Repository Lifecycle

### Configure

The Resource Catalog registers the remote URL, credential binding, optional
initial branch, and clone policy as Repository config v1. Updating URL,
credential binding, or clone policy publishes a new version. A secret value is
never part of the Repository aggregate.

### Bind and resolve

Agent and Session bindings carry only `RepositoryId`. The resolver selects the
current config version at Session creation and records its secret-free material.
No commit, tree, or branch-head SHA is resolved or persisted.

### Activate and use

The coordinator validates the Session-persisted exact credential access/holder
pin and asks the common material resolver to open that revision for the
host-mediated Git transport. A bound Repository without a pin, an anonymous
Repository with one, or a source/usage/holder/revision mismatch fails before Git
I/O. The Runtime never opens a bare Vault source id or reselects the current
revision. Material is held by the transport/broker and is not written into the
prompt, remote URL, durable manifest, or working tree.

Rows retained from before the exact pin existed are migrated through that same
Session application compiler under the root Session CAS before any recovery
effect. This is a one-time schema-semantic migration, not a second Runtime
compatibility path; an unavailable Vault compiler or invalid live source leaves
the row unchanged and realization fails closed.

The Agent may use natural language to drive authorized Git/MCP operations inside
the bound repository: checkout, branch, edit, commit, fetch, push, and pull
request creation. Tool policy and live Repository state still gate remote
operations.

The consistency promise is deliberately limited:

```text
same live Session       -> same working tree
cold Session recovery   -> same config version, clone current remote content
new Session             -> current Repository config, clone current remote content
```

No cross-Session commit replay is promised.

### Release and reclaim

On release, the coordinator records push/PR receipts and, by policy, publishes
un-pushed changes as a Patch/Artifact. It removes the working tree, credential
helper, headers, and lease. A credential-free object cache may remain as an
optimization but is never authority.

Deleting the platform Repository tombstones only awaken's definition, revokes
its credential binding, denies clone/fetch/push, and eventually removes local
working trees/cache. Reclamation must never delete the external remote
repository; that requires a separate explicit high-risk operation outside this
lifecycle.

## Lifecycle State and Live Deny

```text
Active --suspend--> Suspended --resume--> Active
  |
  +--archive------> Archived
  |                    |
  +--delete------------+--> Deleted --reclaim--> Purged receipt
```

| State | New binding | New activation | Existing operations | Content |
|---|---:|---:|---|---|
| Active | allowed after auth | allowed after auth | allowed by access capability | retained |
| Suspended | denied | denied | deny at next enforceable operation; strict policy may terminate sandbox | retained |
| Archived | denied | denied | denied by default | retained |
| Deleted | denied | denied | denied; drain/reconcile | retained until safe purge |
| Purged receipt | denied | denied | denied | physically reclaimed according to resource kind |

A File already copied into an isolated sandbox cannot be made unread by changing
a database row. Strict revocation therefore terminates/reaps affected sandbox
activations. Memory and remote Repo operations additionally re-check live state
at their service/transport boundary.

## Authorization Boundary

```text
credential + request + resource id
                |
                v
        front-door PEP/target resolver
                |
                +----> PIP: owner/state/resource facts
                |
                v
          authorization PDP
                |
          allow/deny/obligation
                |
                v
       scoped application operation
                |
                v
        resource repository invariant
```

The PEP/PDP decision does not replace resource invariants. Successful lookup,
visibility, hash knowledge, config selection, or mount realization never grants
authority. Conversely, resource repositories do not embed IAM deployment mode,
roles, or policy syntax.

Static dependency direction:

```text
awaken-cli composition root
  +-- local/cloud Resource PEP ---> awaken-iam PDP/PIP/PAP
  +-- resource routers ----------> ResourceCatalog / FileStore / MemoryRepository / SkillStore

ResourceCatalog / stores -X-> IAM, principal, API key, role, policy
```

The composition root also places the durable `ManagedSessionRepository` beside
runtime truth. It persists the Session's intrinsic Workspace owner and frozen
resource manifest; it is not an authorization decision cache. The PEP obtains a
trusted Workspace stamp from the authentication/tenant edge, while the Session
repository can only compare that coordinate with aggregate ownership.

Dynamic request flow:

```text
request + credential/path selection
  -> Resource PEP authenticates and asks PDP
  -> deny/approval: stop at edge
  -> allow: stamp trusted WorkspaceScope
  -> ownership/lifecycle lookup
  -> content operation / CAS / safe materialization
```

The resource routers implement the last boundary with one shared
`RequiredWorkspaceScope` extractor. It accepts only the `WorkspaceScope` already
stamped by the composition edge and returns not found for a missing or empty
value. File, MemoryStore, Skill, and ownership handlers therefore have no local
Workspace fallback and no dependency on authentication or authorization values.

The route-to-action map is PEP configuration: File uses `file.read/write`, Skill
uses `skill.read/write`, and awaken's deliberately coarse MemoryStore governance
uses `workspace.read/write`. Replacing that map or policy does not change a
resource port or storage schema.

## Recovery and Reclamation

`ResolvedSessionResources` and activation records are durable Session application
state, not committed Runtime facts. Local paths, process ids, leases, and live
handles are reconstructed.

On restart, `ResourceReclaimer` scans non-terminal activations:

```text
Prepared  -> retry activation or mark Failed
Active    -> adopt live sandbox, or transition Releasing if lease stale
Releasing -> idempotently finish per-kind cleanup
Released  -> no action
Failed    -> clean any partial material, preserve diagnostic receipt
```

All release operations are idempotent. A stale worker or expired activation
lease cannot write Memory, push Git, or retain a secret lease. Configuration
versions referenced by retained Session manifests must remain readable; the
initial simple policy is append-only retention because config rows are small.

## Skills and MCP

Skills and MCP servers are capability material, not shortcuts around resource or
authorization policy:

1. Resources owns custom Skill versions, bundles, and lifecycle; Control owns
   Agent capability references and MCP configuration;
2. Session resolution freezes Skill id/version/hash and selects opaque MCP credential refs;
3. the environment verifies and materializes the complete binary-safe Skill bundle
   under `.skills/<id>` and establishes MCP access;
4. Runtime invokes by validated id through tool/backend ports;
5. effective tools are the intersection of platform authority, active lease, and
   Skill restrictions.

An unavailable required bundle, resource, or MCP endpoint is a typed
pre-execution failure, not instructions-only degradation.

### Skill authoring and import

Every custom-Skill authoring surface produces one complete immutable bundle;
there is no mutable file store beside `SkillStore`. Multipart individual files,
a ZIP import, and the browser's ephemeral editor draft all enter the same
canonical ingestion path owned by `awaken-skill-store::canonicalize_skill_bundle`;
the HTTP layer only collects transport fields. That path rejects traversal, links and special archive
entries, mixed ZIP/file input, multiple transport roots, duplicate normalized
paths, missing or non-UTF-8 root `SKILL.md`, and expanded size/count violations.
It strips one common transport directory before persisting normalized relative
paths, preserving binary references/assets and a constrained executable bit for
regular scripts rather than arbitrary archive permissions.
Browser publication preserves that bit through a bounded `executable_paths`
multipart field; it may only name files present in the same complete upload and
cannot override ZIP metadata.

Publishing appends a version and atomically advances `latest_version`; it never
overwrites version bytes. Browser publication supplies the opened version through
`If-Match`, so a stale draft conflicts instead of replacing a concurrently
published bundle. The browser draft is memory-local and disposable. Existing
Sessions retain their frozen id/version/hash, while later Session resolution sees
the new latest version.

```text
directory | ZIP | browser draft
             -> canonical bundle ingestion
             -> append immutable SkillVersion
             -> advance latest pointer
             -> new Sessions pin id/version/hash
```

## External Work

External work is tool or backend execution offload, not a second resource or
Session dispatcher. It may run in-process, through durable ingress that resumes
committed work, or through a remote adapter returning typed results. The same
effective resource identities, authorization decision, and activation leases
bound for the Session constrain the offloaded operation; an external worker does
not re-resolve Agent defaults or widen resource access.

For durable Run execution, capability placement makes that constraint executable:
`session-resources/v1` requires a shared File/Memory/Skill/lifecycle family plus
the Resource Catalog validator; `repository-credentials/v1` additionally requires
a shared credential injection seam. A resource-ineligible worker cannot claim the
dispatch. On every retry the eligible worker repeats live ownership, lifecycle,
frozen-config-integrity, and credential-revocation checks before using the retained
or rebuilt environment.

## Anthropic Compatibility

The Managed adapter owns Anthropic DTOs and projects the effective resource list.
File and Memory ids map directly. A GitHub resource carrying URL and token is
lowered into a Session-scoped managed Repository definition and credential
binding before the neutral resolver sees it. Agent defaults are expanded into
the same effective Session resource list.

This matches the observable Managed Agents model—resources enter a Session,
repositories clone current content, and credentials are not echoed—while the
internal config version remains an awaken governance detail.

## Current Implementation and Migration

### Keep

- content-addressed immutable `FileStore` and its local/SQLite/Postgres/S3
  adapters;
- Workspace-scoped resource ownership foundations;
- `MemoryRepository` aggregate repository: path model, CAS, atomic history/redaction,
  monotonic ids, and local/SQLite/Postgres adapters;
- Agent default resource configuration and Managed Session attachment ingress;
- `SandboxProvider`, mount descriptors, and environment realization boundary;
- Vault credential references and host-side secret materialization.
- the unified Workspace-scoped Skill aggregate repository and version-pinned
  Session Skill bindings.
- durable root and delegated dispatches carry the same Workspace-scoped frozen
  resource manifest; cold workers install it before sandbox creation and placement
  excludes workers without the required shared resource/credential seams.
- delivered Skill bundles form one exact `.skills` projection: rebuilding replaces
  the tree, and changing to an empty or different pin removes obsolete scripts,
  references, templates, and binary assets before the next Run.
- directory/ZIP import and browser editing publish through the same canonical
  complete-bundle ingestion path; no draft or archive store exists beside the
  versioned Skill aggregate.

### Completed consolidation

- `AgentInputConfig { agent_id, inputs, revision }` replaces
  `AgentResourceConfig { resources, version }`;
- `AgentInputBindingRepository` requires Workspace on every operation and applies
  sequential, idempotent revision transitions;
- Agent defaults and Session attachments share typed `InputBinding` /
  `InputResourceId`; Outputs and Skills are no longer input variants;
- `SessionInputResolver` composes and resolves once, preserves access, and emits
  the sole durable `ResolvedSessionResources` manifest;
- prompts are generated only from the resolved Session manifest;
- the former Runtime-side Agent repository read/lowering and duplicate
  `EffectiveSessionInputs` name are removed.
- Memory realization has one lifetime owner: `MemoryMount`; the Host-side
  `StagedResources.memory_mounts`, polling harvest, and `harvest_thread_memory`
  duplicate writer are removed;
- copy fallback retains only transient `(path, id, sha)` heads captured at
  materialization and reconciles with CAS update plus atomic `delete_if_match`;
  concurrent durable heads are preserved and reported.
- `RepoStage` and LocalSandbox-specific orchestration are replaced by
  `RepositoryActivation { plan, credential }` plus the neutral, secret-free
  `RepositoryRealizationPlan` / `RepositoryRealizer` environment port;
- Repository credentials use the canonical `HttpBasicAuth` consumption contract
  over typed `awaken.http-basic/v1` Vault material. Runtime translates it at the
  target boundary into a non-serializable `RepositoryHttpBasicCredential`; no
  scalar token or preformatted Authorization header remains as a second path;
- Repository Vault bindings are compiled before Session persistence into one
  exact secret-free `ResolvedRepositoryCredential`; Model, MCP, and Repository
  now share `CredentialMaterialResolver`, and the bare-source Runtime
  materialization path is removed;
- Repository publication and authored-Skill persistence run only at binding
  replacement or Session release; `GET /v1/files` is a read-only artifact
  projection and no longer triggers unrelated resource writes.
- the misleading `MemoryFs` family and `memfs` module are removed rather than
  retained as aliases: the port is `MemoryRepository`, with
  `VolatileMemoryRepository`, `SqliteMemoryRepository`, and
  `PostgresMemoryRepository` adapters. The unused JSON-filesystem adapter is also
  removed so embedded deployments have exactly one durable implementation.
- durable `MemoryExtractionIntent` application work replaces fire-and-forget
  extraction: terminal outbox recovery, process-unique lease fencing/heartbeat,
  staged mutation receipts, and idempotent CAS apply survive a real SIGKILL;
- publication-pinned inference and credential-injection vocabulary is owned by
  the executable snapshot in `awaken-runtime-contract`; Runtime materialization
  injects only that frozen reference and never re-resolves a route or reads a
  global extractor credential;
- stale FUSE Memory realizations are detected and detached at remount, so a dead
  process cannot prevent the same durable Session binding from being recovered.

### Completed deletion

- `MemoryBlobStore` and all blob backends;
- legacy single-file Memory materialization and Host-global extraction directory;
- `with_memory(mem_dir)`, `memory_scope_root`, `StagedResources.memory_mounts`,
  polling-route harvest, and Host `harvest_thread_memory` (removed);
- API-local independent `VersionRepository` (removed; legacy rows are imported
  once into the aggregate repository without continued dual reads/writes);
- raw repository URL support outside the legacy/protocol ingress adapter;
- raw `auth_token` and string `git_ref` in the neutral Runtime resource shape;
- commit/tree/content pin types for Repository;
- File version abstractions above the immutable logical-File-to-blob mapping;
- API-local `SkillRegistry`, text-only `SkillStore` overwrite semantics, lossy
  `String::from_utf8_lossy` bundle ingestion, and runtime lookup of `latest`.

### Completed reclamation slice

- one `ResourcePurgeIntent`/receipt state machine covers File, MemoryStore,
  Repository, and Skill without importing IAM vocabulary;
- the durable resource store owns purge work plus Workspace-scoped reverse
  references; Session manifest replacement updates its references atomically;
- a physical-identity fence serializes zero-reference proof with every new
  reference across processes; SQLite and Postgres implement the same port;
- the complete resource persistence family is selected once at composition by
  `AWAKEN_RESOURCE_DATABASE_URL`, independently of IAM/authentication mode;
- File bytes, Memory content/history, Skill bundles, and lifecycle fences cannot
  silently mix shared and node-local backends in a multi-node deployment;
- File GC checks every Workspace ownership/reference before deleting shared bytes;
- Memory GC requires the catalog tombstone, pinned config generation, retention,
  no Session/Agent binding, and no recoverable extraction before atomically
  deleting heads and history;
- Skill delete is a tombstone: new resolution is denied while retained Session
  pins can still read immutable versions until the reclaimer records physical
  purge; and
- Repository reclamation records only awaken-local cleanup and never calls a
  remote Git deletion operation.

## Failure Semantics

Fail before Agent execution when:

- ownership or authorization is missing;
- a resource is not Active;
- a pinned config version is absent or corrupt;
- File bytes do not match `FileId`;
- a mount path is unsafe or collides without explicit replacement;
- a required credential cannot be leased;
- a required resource cannot be activated.

Fail an operation without widening authority when:

- Memory CAS is stale;
- a read-only binding attempts a write/extraction;
- Repository state or credential is revoked before fetch/push;
- an activation lease is stale.

Cleanup failure leaves an activation in `Releasing` or `Failed` for the
reconciler. It never silently marks the resource released.

## Verification Matrix

| Area | Required proof |
|---|---|
| Binding | Agent defaults and Session attachments merge once; explicit replacement only; mount collision fails |
| File | binary round-trip; blob digest validation; distinct logical uploads may share bytes; edited Session copy cannot mutate the original blob; K8s live replacement/removal stays below the fixed input root, is Agent-read-only, and survives handle adoption; shared-blob ownership isolation; safe GC |
| Memory | config update affects only later Sessions; current content remains shared; read-only extraction denied; CAS conflict loses no update |
| Repository | config update affects later Sessions; no commit pin; exact source revision/usage/holder pin; anonymous/missing/stale/inactive/cross-Workspace/mismatched cases fail closed; retained pre-pin row migrates once before I/O; material absent from manifest/logs/prompt/disk; remote is never deleted by GC |
| Skill | binary bundle round-trip; traversal rejected; restart preserves history; v1 Session keeps v1 after v2 publication; hash mismatch fails closed |
| Scope/auth | cross-Workspace File/Memory/Repo access fails closed; old config cannot bypass suspension/deletion/revocation |
| Recovery | stale Prepared/Active/Releasing activations converge idempotently after restart |
| Reclamation | no referenced File purge; a racing cross-node reference loses to or blocks the durable fence; crash resumes the same fence; Memory drains handles/jobs; Repository cleanup removes only local material |
| Boundary | Runtime Core receives no product DTO, IAM policy, secret, absolute host path, Project, or WorkUnit |

`SessionResourceActivation.tla`, `ResourceDispatch.tla`, and
`ResourceReclamation.tla` model the common activation state machine, one-time
configuration pin plus live deny overlay, and the rule that physical purge
requires the current fence with zero references and leases. E2E scenarios cover
the resource families from configuration through Session use and recovery to
release/reclamation.

## First Vertical Slice

1. Introduce typed bindings and one `SessionInputResolver` without changing
   storage backends.
2. Preserve access, reject mount collisions, generate prompts from the effective
   manifest, and remove the second Host merge.
3. Make File materialization binary-safe and keep Session working copies outside
   FileStore mutation.
4. Add Memory/Repository config versions and platform-managed Repository ids.
5. Route Memory mount/API/recall/extraction/history/redaction through one
   `MemoryRepository`; remove blob/harvest.
6. Add durable activation reconciliation and per-kind reclamation.

## Non-Goals

- no Git commit or tree pinning;
- no Memory content snapshot or entry-version pinning;
- no File version layer above content identity;
- no exact Repository replay after cold sandbox reconstruction;
- no Project or WorkUnit concept in awaken resources;
- no IAM policy language inside resource services;
- no generic store abstraction combining File, Memory, and Repository;
- no implicit deletion of a remote Git repository.

## Guardrails

G3, G4, G8, G9, G13, G14, G21, G27, G37, G38, and G39 in
[INVARIANTS](../INVARIANTS.md).
