# Resources, Memory, Files, And Skills

This document is the normative design for platform-managed resource inputs in
awaken. It defines the static domain model, the end-to-end lifecycle, the owner
of every stage, recovery and reclamation, and the boundary with authorization
and Runtime Core. [ADR-0063](../adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md)
owns the load-bearing identity and version decisions.

## Scope and Ubiquitous Language

This design covers three Agent input resources:

- **File** — immutable content addressed by `FileId`;
- **MemoryStore** — mutable, Workspace-owned long-term state;
- **Repository** — mutable reference to an external Git repository.

A resource may be an **Agent default input** or a **Session attachment**. Those
are two sources for one binding language, not two resource models.

The key terms are:

| Term | Meaning |
|---|---|
| Resource identity | stable typed id: `FileId`, `MemoryStoreId`, or `RepositoryId` |
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
| Resource definitions, config versions, current lifecycle state | Resource Catalog / product control plane |
| Agent default input associations | Agent Configuration |
| Temporary attachments and effective input manifest | Session |
| Principal, policy, action-to-scope applicability | Authorization/IAM |
| Immutable File bytes | File data plane |
| Mutable Memory entries, history, redaction | Memory data plane |
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
    id: FileId, // BLAKE3 content identity
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
    recall_policy: RecallPolicy,
    extraction_policy: ExtractionPolicy,
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
    recall_policy: RecallPolicy,
    extraction_policy: ExtractionPolicy,
    mount_path: SandboxPath,
}

struct ResolvedRepositoryInput {
    binding_id: BindingId,
    repository_id: RepositoryId,
    config_version: ConfigVersion,
    remote_url: RepositoryUrl,
    credential_binding: CredentialBinding,
    initial_branch: Option<BranchName>,
    access: RepositoryAccess,
    mount_path: SandboxPath,
}
```

The manifest is serializable and secret-free. It contains no host path, live
handle, credential value, Memory entry version, or Git commit.

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
| `ResourceCatalog` | Existing | Resource Catalog | Memory/Repository definitions, immutable config versions, current version, lifecycle state | content bytes, IAM policy, sandbox paths |
| `AgentInputBindingRepository` | Existing | Agent Configuration | Workspace-scoped Agent default bindings and authoring revision; every operation requires Workspace | Session merge, content resolution, authorization |
| Managed Session adapter | Existing | Protocol/product ACL | parse/project Anthropic resources; accept temporary attachments | raw DTO leakage into neutral/resource services |
| `SessionInputResolver` | Existing | Session control plane | merge once, replace explicitly, validate mount paths, select current config versions, create `ResolvedSessionResources` and prompts | secret material, runtime loop, physical mounts |
| front-door PEP | Existing/evolving | Server edge | authenticate, construct trusted Workspace target, call PDP, enforce obligations | resource content and domain policy implementation |
| authorization PDP/PIP | External/shared authorization domain | IAM | decide principal/action/scope/resource facts under active policy | mounts, resource configuration, storage |
| `SessionResourceCoordinator` | Existing in Managed Session application service | Session application/host | activation state, ordered provision/release, recovery handoff | Agent config loading, IAM policy language |
| `FileStore` | Existing | File data plane | immutable content-addressed bytes | Workspace authorization; mutable overwrite |
| `MemoryRepository` | Existing canonical port | Memory data plane | scoped entries, CAS, atomic history, redaction, retention hooks | Agent/Session binding and IAM policy |
| `MemoryRuntime` | Existing | Runtime Host | store-less recall selector/extractor capability and background-run drain | resource identity, default store, IAM policy |
| `BoundMemory` | Existing | Session Runtime | one resolved store handle + pinned policy + maximum access shared by recall/extraction | workspace lookup, current-config resolution, authorization |
| `RepositoryRealizer` | Existing neutral port | Environment adapter | clone current remote config, construct working tree, publish Agent-authored commits with ephemeral transport credentials | remote repository ownership, authorization policy, or commit pinning |
| `CredentialResolver`/Vault | Existing | Credential product domain | turn a credential binding into a short-lived lease and rotate/revoke it | Agent prompt, persisted Session secret material |
| `SandboxProvider` | Existing | Environment provisioning | realize validated mounts/working trees and dispose them | product resource authoring and policy |
| `ResourceReclaimer` | Existing, durable and per-resource | Product/session operations | reconcile crashed activations and purge intents; retention, reference checks, fenced claims, per-kind receipts | authorization decisions, remote Git deletion |
| `ResourceReclamationFence` | Existing resource lifecycle port | Resource consistency | atomically prove zero physical references, fence `(kind, resource_id)`, and reject racing reference writes | principal, role, policy, API key, Org/Project/WorkUnit |
| `ResourcePlaneStores` / `ResourcePlanePorts` | Existing composition bundle/Host wiring value | Composition root | select File, Memory, Skill, and lifecycle adapters together and inject them atomically before local stores open | aggregate behavior, IAM/PDP data, authorization decisions |
| `SqliteResourceStore` / `PostgresResourceStore` | Existing adapters | Resource consistency persistence | persist purge intents, intrinsic references, and reclamation fences for embedded or multi-node deployment | IAM/PDP data and File/Memory/Skill content |

The catalog names roles rather than forcing them into one crate. Local mode may
compose several roles in one process; cloud mode may deploy them separately.
Their contracts and ownership stay the same.

## Lifecycle Stage Ownership

The following matrix is normative. Every transition has one orchestration owner;
resource-specific repositories enforce their own intrinsic invariants.

| Stage | Primary component | Collaborators | Durable result |
|---|---|---|---|
| Configure File | Files API / File application service | `FileStore`, ownership repository | immutable `FileId` plus Workspace ownership edge |
| Configure Memory | Resource Catalog service | Memory config repository, `MemoryRepository` | `MemoryStore` + config v1 + logical namespace |
| Configure Repo | Resource Catalog service | Repository config repository, Vault | `Repository` + config v1 referencing credential binding |
| Bind Agent default | Agent Configuration service | `AgentInputBindingRepository`, PEP/PDP | identity-only `InputBinding`, authoring revision increments |
| Attach to Session | Managed Session adapter | PEP/PDP | temporary `SessionInputAttachment` |
| Resolve | `SessionInputResolver` | Agent binding repo, Resource Catalog, PEP result | `ResolvedSessionResources`; Memory/Repo config versions selected once |
| Activate File | `SessionResourceCoordinator` | `FileStore`, `SandboxProvider` | read-only mount + activation `Active` |
| Activate Memory | `SessionResourceCoordinator` | `MemoryRepository`, Memory realizer | `ScopedMemoryStore`/mount + activation `Active` |
| Activate Repo | `SessionResourceCoordinator` | Repository realizer, Vault, Sandbox | current clone + working tree; credential lease not persisted |
| Use | sandbox/tool adapters | File/Memory/Git domain ports | domain writes and receipts; no second config resolve |
| Release | `SessionResourceCoordinator` | Sandbox manager, Vault, per-kind realizer | activation `Released`; sandbox-local material removed |
| Reconcile crash | `ResourceReclaimer` | Session activation repository, workers | stale activation released or retried |
| Archive/Delete | Resource Catalog service | PEP/PDP, resource repository | live deny state/tombstone before physical cleanup |
| Fence physical identity | `ResourceReclaimer` | `ResourceReclamationFence`, reference writers | durable zero-reference fence; racing bindings fail closed |
| Purge | `ResourceReclaimer` | per-kind store and reference indexes | idempotent physical deletion followed by auditable purge receipt |

Resource services receive trusted Workspace coordinates and authorized operation
intent. They do not receive API keys, roles, IAM syntax, Project, or WorkUnit.

## Common Configure-to-Reclaim Flow

```text
Resource API / Files API
          |
          v
Resource Catalog or FileStore
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

The Files API streams bytes into `FileStore`. The store computes BLAKE3 and
returns `FileId`; equal bytes deduplicate. The ownership repository records the
Workspace access to that content id. Knowing a hash is never authority.

### Bind and resolve

Agent and Session bindings carry the immutable `FileId`. Resolution checks the
Workspace ownership edge, current deletion state, mount path, and content existence. No
File config or version lookup exists.

### Activate and use

The coordinator asks `FileStore` for binary bytes and the Sandbox provider
materializes them read-only. The actual bytes must hash back to `FileId`.
Text conversion is forbidden. An Agent may edit a working copy, but the result
is a new File or Artifact; the original blob is never overwritten.

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

The Resource Catalog creates a Workspace-owned `MemoryStore`, publishes config
v1, and ensures the Memory data plane recognizes the logical store id. Config
updates append an immutable version and atomically advance
`current_config_version`. They do not copy or version the store's content.

### Bind and resolve

Agent and Session bindings carry only `MemoryStoreId`. At Session creation the
resolver selects the current config version and copies its policy values into
`ResolvedMemoryInput`. Existing Sessions retain that policy/config version;
later Sessions receive the new one. All Sessions still address the same mutable
logical store.

### Activate and use

After live ownership/state checks, `MemoryRepository.open(workspace, store_id)`
returns a capability-limited `ScopedMemoryStore`. Recall, extraction, mounted
file operations, public Memory API operations, history, and redaction all use
that same repository.

```text
Session S1 resolves config v3
Session S2 resolves config v4
S1 and S2 both read current MemoryStore content
entry CAS/version prevents silent lost updates
```

`ReadOnly` is enforced by the handle/realizer. Extraction cannot write through a
read-only binding. Internal entry `version` and `content_sha256` remain CAS/API
data and are not binding pins.

### Release and reclaim

Release closes/detaches the scoped handle; it never deletes the store. Reliable
extraction must reach a durable receipt or a durable retry/terminal conclusion
before the activation is considered fully settled.

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

The coordinator asks the Vault for a short-lived credential lease and asks the
Repository realizer to clone the current remote state into the Session sandbox.
The credential is held by the transport/broker and not written into the prompt,
remote URL, durable manifest, or working tree.

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

1. product/config code owns Skill versions, bundles, visibility, and MCP config;
2. Session resolution freezes Skill id/version/hash and selects opaque MCP credential refs;
3. the environment verifies and materializes the complete binary-safe Skill bundle
   under `.skills/<id>` and establishes MCP access;
4. Runtime invokes by validated id through tool/backend ports;
5. effective tools are the intersection of platform authority, active lease, and
   Skill restrictions.

An unavailable required bundle, resource, or MCP endpoint is a typed
pre-execution failure, not instructions-only degradation.

## External Work

External work is tool or backend execution offload, not a second resource or
Session dispatcher. It may run in-process, through durable ingress that resumes
committed work, or through a remote adapter returning typed results. The same
effective resource identities, authorization decision, and activation leases
bound for the Session constrain the offloaded operation; an external worker does
not re-resolve Agent defaults or widen resource access.

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
- publication-pinned inference and credential-injection vocabulary is shared by
  configuration, Session application, and Runtime through the foundation
  `awaken-inference-contract`; Runtime materialization injects only that frozen
  reference and never re-resolves a route or reads a global extractor credential;
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
- File version abstractions above immutable `FileId`;
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
| File | binary round-trip; content-id validation; read-only mount; shared-blob ownership isolation; safe GC |
| Memory | config update affects only later Sessions; current content remains shared; read-only extraction denied; CAS conflict loses no update |
| Repository | config update affects later Sessions; no commit pin in manifest; credentials absent from logs/prompt/disk; remote is never deleted by GC |
| Skill | binary bundle round-trip; traversal rejected; restart preserves history; v1 Session keeps v1 after v2 publication; hash mismatch fails closed |
| Scope/auth | cross-Workspace File/Memory/Repo access fails closed; old config cannot bypass suspension/deletion/revocation |
| Recovery | stale Prepared/Active/Releasing activations converge idempotently after restart |
| Reclamation | no referenced File purge; a racing cross-node reference loses to or blocks the durable fence; crash resumes the same fence; Memory drains handles/jobs; Repository cleanup removes only local material |
| Boundary | Runtime Core receives no product DTO, IAM policy, secret, absolute host path, Project, or WorkUnit |

Formal checks should model the common state machine, live deny overlay, and the
rule that purge requires zero active references/leases. E2E scenarios should
cover all three resources from configuration through Session use and recovery to
release/reclamation.

## First Vertical Slice

1. Introduce typed bindings and one `SessionInputResolver` without changing
   storage backends.
2. Preserve access, reject mount collisions, generate prompts from the effective
   manifest, and remove the second Host merge.
3. Make File materialization binary-safe and read-only.
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
