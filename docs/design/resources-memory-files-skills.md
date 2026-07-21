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
| Effective Session inputs | one secret-free, resolved manifest after Agent defaults and Session attachments are merged |
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
struct EffectiveSessionInputs {
    files: Vec<ResolvedFileInput>,
    memories: Vec<ResolvedMemoryInput>,
    repositories: Vec<ResolvedRepositoryInput>,
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

### Activation model

```rust
struct SessionResourceActivation {
    session_id: SessionId,
    binding_id: BindingId,
    resource_id: InputResourceId,
    state: ActivationState,
    lease_expires_at: Option<Timestamp>,
}

enum ActivationState {
    Prepared,
    Active,
    Releasing,
    Released,
    Failed,
}
```

An activation record is Session application state used by crash recovery and
reclamation. It stores neither a secret nor a process-local handle.

## Resource Input Component Catalog

| Component | Status | Owner | Responsibility | Must not own |
|---|---|---|---|---|
| `ResourceCatalog` | Target evolution of current config stores | Resource Catalog | Memory/Repository definitions, immutable config versions, current version, lifecycle state | content bytes, IAM policy, sandbox paths |
| `AgentInputBindingRepository` | Existing as `ResourceStore`, rename/evolve | Agent Configuration | Workspace-scoped Agent default bindings and authoring revision | Session merge, content resolution, authorization |
| Managed Session adapter | Existing | Protocol/product ACL | parse/project Anthropic resources; accept temporary attachments | raw DTO leakage into neutral/resource services |
| `SessionInputResolver` | Target; replaces duplicate merge/resolve | Session control plane | merge once, replace explicitly, validate mount paths, select current config versions, create `EffectiveSessionInputs` and prompts | secret material, runtime loop, physical mounts |
| front-door PEP | Existing/evolving | Server edge | authenticate, construct trusted Workspace target, call PDP, enforce obligations | resource content and domain policy implementation |
| authorization PDP/PIP | External/shared authorization domain | IAM | decide principal/action/scope/resource facts under active policy | mounts, resource configuration, storage |
| `SessionResourceCoordinator` | Target evolution of Host preparation | Session application/host | activation state, ordered provision/release, recovery handoff | Agent config loading, IAM policy language |
| `FileStore` | Existing | File data plane | immutable content-addressed bytes | Workspace authorization; mutable overwrite |
| `MemoryRepository` | Existing behavior behind `MemoryFs`; naming evolution remains | Memory data plane | scoped entries, CAS, atomic history, redaction, retention hooks | Agent/Session binding and IAM policy |
| `RepositoryRealizer` | Target evolution of repo staging | Environment/host adapter | clone current remote config, construct working tree, mediate Git credentials | remote repository ownership or commit pinning |
| `CredentialResolver`/Vault | Existing | Credential product domain | turn a credential binding into a short-lived lease and rotate/revoke it | Agent prompt, persisted Session secret material |
| `SandboxProvider` | Existing | Environment provisioning | realize validated mounts/working trees and dispose them | product resource authoring and policy |
| `ResourceReclaimer` | Target | Product/session operations | reconcile crashed activations, retention, reference checks, per-kind purge receipts | authorization decisions, remote Git deletion |

The catalog names roles rather than forcing them into one crate. Local mode may
compose several roles in one process; cloud mode may deploy them separately.
Their contracts and ownership stay the same.

## Lifecycle Stage Ownership

The following matrix is normative. Every transition has one orchestration owner;
resource-specific repositories enforce their own intrinsic invariants.

| Stage | Primary component | Collaborators | Durable result |
|---|---|---|---|
| Configure File | Files API / File application service | `FileStore`, ownership repository | immutable `FileId` plus Workspace grant |
| Configure Memory | Resource Catalog service | Memory config repository, `MemoryRepository` | `MemoryStore` + config v1 + logical namespace |
| Configure Repo | Resource Catalog service | Repository config repository, Vault | `Repository` + config v1 referencing credential binding |
| Bind Agent default | Agent Configuration service | `AgentInputBindingRepository`, PEP/PDP | identity-only `InputBinding`, authoring revision increments |
| Attach to Session | Managed Session adapter | PEP/PDP | temporary `SessionInputAttachment` |
| Resolve | `SessionInputResolver` | Agent binding repo, Resource Catalog, PEP result | `EffectiveSessionInputs`; Memory/Repo config versions selected once |
| Activate File | `SessionResourceCoordinator` | `FileStore`, `SandboxProvider` | read-only mount + activation `Active` |
| Activate Memory | `SessionResourceCoordinator` | `MemoryRepository`, Memory realizer | `ScopedMemoryStore`/mount + activation `Active` |
| Activate Repo | `SessionResourceCoordinator` | Repository realizer, Vault, Sandbox | current clone + working tree; credential lease not persisted |
| Use | sandbox/tool adapters | File/Memory/Git domain ports | domain writes and receipts; no second config resolve |
| Release | `SessionResourceCoordinator` | Sandbox manager, Vault, per-kind realizer | activation `Released`; sandbox-local material removed |
| Reconcile crash | `ResourceReclaimer` | Session activation repository, workers | stale activation released or retried |
| Archive/Delete | Resource Catalog service | PEP/PDP, resource repository | live deny state/tombstone before physical cleanup |
| Purge | `ResourceReclaimer` | per-kind store and reference indexes | auditable purge receipt |

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
             EffectiveSessionInputs
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
returns `FileId`; equal bytes deduplicate. The ownership repository grants the
Workspace access to that content id. Knowing a hash is never authority.

### Bind and resolve

Agent and Session bindings carry the immutable `FileId`. Resolution checks the
Workspace grant, current deletion state, mount path, and content existence. No
File config or version lookup exists.

### Activate and use

The coordinator asks `FileStore` for binary bytes and the Sandbox provider
materializes them read-only. The actual bytes must hash back to `FileId`.
Text conversion is forbidden. An Agent may edit a working copy, but the result
is a new File or Artifact; the original blob is never overwritten.

### Release and reclaim

Session release deletes only the sandbox copy. Logical File deletion revokes a
Workspace grant. Physical blob GC is allowed only when all are false:

```text
Workspace grants
Agent bindings
active/effective Session references
Artifact references
retention or legal hold
```

A shared blob survives deletion of one tenant's grant.

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
  +-- resource routers ----------> ResourceCatalog / FileStore / MemoryFs / SkillStore

ResourceCatalog / stores -X-> IAM, principal, API key, role, policy
```

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

`EffectiveSessionInputs` and activation records are durable Session application
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
2. Session resolution selects validated descriptors and opaque credential refs;
3. the environment safely materializes Skill files and establishes MCP access;
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
- `MemoryFs` aggregate repository: path model, CAS, atomic history/redaction,
  monotonic ids, and local/SQLite/Postgres adapters;
- Agent default resource configuration and Managed Session attachment ingress;
- `SandboxProvider`, mount descriptors, and environment realization boundary;
- Vault credential references and host-side secret materialization.

### Rename or merge

- rename `AgentResourceConfig.version` to `revision`;
- evolve `ResourceStore` to `AgentInputBindingRepository`;
- replace `ResourceBinding.kind + resource_id` and string `SessionResource` with
  the typed common binding language;
- merge protocol-side and Host-side resource composition into
  `SessionInputResolver`;
- preserve `ResourceAccess` through the neutral Session contract and realizer;
- generate prompts from `EffectiveSessionInputs` once;
- rename the now-unified `MemoryFs` aggregate port to `MemoryRepository` when the
  remaining extraction call sites have migrated;
- evolve repo staging into a `RepositoryRealizer` consuming a platform-managed
  Repository config version and credential binding.

### Delete after migration

- `MemoryBlobStore` and all blob backends;
- single-file Memory materialization and `harvest_thread_memory`;
- `StagedResources.memory_mounts` and last-writer-wins Memory write-back;
- API-local independent `VersionRepository` (removed; legacy rows are imported
  once into the aggregate repository without continued dual reads/writes);
- Runtime's second read of the Agent resource store and
  `binding_as_session_resource` lowering;
- raw repository URL as `ResourceBinding.resource_id`;
- raw `auth_token` and string `git_ref` in the neutral Runtime resource shape;
- commit/tree/content pin types for Repository;
- File version abstractions above immutable `FileId`;
- `Outputs` and `Skill` variants from the File/Memory/Repository input union.

### Add

- typed ids and `InputResourceId`;
- `EffectiveSessionInputs` and activation records;
- version repositories for Memory/Repository config;
- `SessionInputResolver`, `SessionResourceCoordinator`, and
  `ResourceReclaimer` roles;
- `ScopedMemoryStore` capabilities;
- binary-safe File realization;
- resource-specific purge receipts and recovery tests.

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
| File | binary round-trip; content-id validation; read-only mount; shared-blob grant isolation; safe GC |
| Memory | config update affects only later Sessions; current content remains shared; read-only extraction denied; CAS conflict loses no update |
| Repository | config update affects later Sessions; no commit pin in manifest; credentials absent from logs/prompt/disk; remote is never deleted by GC |
| Scope/auth | cross-Workspace File/Memory/Repo access fails closed; old config cannot bypass suspension/deletion/revocation |
| Recovery | stale Prepared/Active/Releasing activations converge idempotently after restart |
| Reclamation | no referenced File purge; Memory drains handles/jobs; Repository cleanup removes only local material |
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

G3, G4, G8, G9, G13, G14, G21, G27, G37, and G38 in
[INVARIANTS](../INVARIANTS.md).
