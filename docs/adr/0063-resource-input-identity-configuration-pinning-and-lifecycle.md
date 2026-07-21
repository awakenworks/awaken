# ADR-0063: Resource Input Identity, Configuration Pinning, and Lifecycle Ownership

- Status: Accepted
- Date: 2026-07-21
- Builds on: [ADR-0038](0038-managed-resource-injection-and-store-organization.md)
  (resource injection and provisioning descriptors),
  [ADR-0041](0041-sandbox-execution-environment-provider.md) (sandbox lifecycle),
  [ADR-0053](0053-memory-store-fuse-mount.md) (path-addressed Memory and CAS), and
  [ADR-0061](0061-selectable-identity-and-platform-managed-resource-scopes.md)
  (Workspace ownership and IAM boundary)
- Detailed design:
  [Resources, Memory, Files, And Skills](../design/resources-memory-files-skills.md)
- Supersedes:
  - ADR-0038 D2 only where a content fingerprint was implied for every resource
    family;
  - ADR-0038 amendment A3a only where an Agent-bound resource prompt was rendered
    before Session attachments were resolved;
  - ADR-0053 D1 only where `MemoryBlobStore` and copy/harvest were retained as a
    long-term parallel Memory truth;
  - ADR-0056 descriptions of managed Memory as a copied value harvested on release.

## Context

Files, Memory stores, and repositories are all inputs to an Agent, and all may be
declared as Agent defaults or attached to one Session. They do not, however, have
one content lifecycle:

- a File is immutable and content-addressed;
- a MemoryStore is a stable, mutable aggregate containing independently changing
  entries;
- a Repository is a stable, mutable reference to an external Git truth.

Before this decision was implemented, `ResourceBinding` and `SessionResource`
were separate stringly shapes, protocol and Runtime both merged inputs, and
access could be lost during lowering. Agent defaults now persist the same typed
`InputBinding` language as Session attachments, composition happens once in
`SessionInputResolver`, and `ResolvedSessionResources` is the only durable
runtime manifest. Legacy flat File/Memory/Repository JSON is accepted only by a
read migration adapter; legacy Outputs/Skill input variants fail closed.

Treating every resource as content-pinned would make mutable Memory and Git look
replayable when they are not. Treating every resource as live would discard the
useful stability of immutable Files and of the configuration that selected a
Memory or Repository endpoint. The design needs one explicit resolution point,
one owner for each lifecycle stage, and resource-specific reclamation rules.

## Decision

### D1: Pin according to the resource's content lifecycle

The version rule is:

| Resource | Agent binding carries | Session resolution pins | Never pinned |
|---|---|---|---|
| File | `FileId` | the same immutable content id | a separate File version |
| MemoryStore | `MemoryStoreId` | `MemoryStoreConfigVersion` | entry id/version, head, content hash, checkpoint |
| Repository | `RepositoryId` | `RepositoryConfigVersion` | Git commit, tree, branch head, internal file version |
| Skill capability | Skill resource id | immutable Skill version + bundle SHA-256 | a later Skill version |

An Agent does not pin a Memory or Repository configuration version. It names the
resource identity. The `SessionInputResolver` reads the current published
configuration version exactly once while creating the Session and persists the
secret-free result in `ResolvedSessionResources`. A Session does not pin an Agent
version; changes to Agent defaults affect later Sessions, not the effective
inputs already recorded for an existing Session.

Skills are capabilities rather than input mounts, so they are not added to
`InputResourceId`. The same durable `ResolvedSessionResources` manifest carries a
separate optional `skills[]` collection. `None` preserves legacy behavior;
`Some([])` explicitly selects none; each non-empty entry freezes
`skill_id + version + bundle_sha256` once at Session creation.

`Revision` is reserved for optimistic concurrency on a mutable authoring
aggregate. `ConfigVersion` is an immutable published Memory/Repository
configuration. Neither term names mutable resource content.

### D2: One binding language, one resolution point

Agent defaults and Session attachments use the same typed binding vocabulary:

```text
InputBinding {
    binding_id,
    target: File(FileId) | MemoryStore(MemoryStoreId) | Repository(RepositoryId),
    mount_path,
    access,
    instructions,
}
```

The Session control/application layer is the sole composer:

```text
current Agent bindings + Session attachments
                       |
                       v
              SessionInputResolver
                       |
                       v
          ResolvedSessionResources
```

An explicit Session attachment may replace a named Agent binding. Accidental
mount-path collisions fail closed; array order is never an override policy.
Resource prompts are rendered from `ResolvedSessionResources`, after replacement,
scope, access, and mount paths are final. The Runtime Host does not read the
Agent binding repository or merge resources a second time.

### D3: Configuration and live safety state are different

Memory and Repository configuration versions are immutable and append-only.
They contain behavior and connection configuration, not live authorization or
physical content identity. In particular, a Memory config never contains a
database path or node locator; `MemoryStoreId` continues to route to one logical
store through the Memory data plane during backend migration.

Current ownership, lifecycle state, credential revocation, and authorization
remain live deny overlays:

```text
pinned config + current state + current ownership + current authorization
              = allow or fail closed
```

An old config version cannot revive a suspended/deleted resource or a revoked
credential. Credential configuration contains only a binding/reference; secret
material is injected as a short-lived lease at activation or remote operation
time and is never persisted in `ResolvedSessionResources`.

### D4: Resource services own data invariants, not authorization policy

The platform edge is the PEP. It obtains the principal and trusted Workspace,
asks the authorization domain for a decision, and passes scoped, typed input to
the resource application service. Resource services enforce intrinsic
invariants—Workspace ownership, resource state, immutable File identity, Memory
CAS, and safe paths—but do not parse API keys, roles, IAM policy, Org hierarchy,
Project, or WorkUnit.

Awaken's resource scope stops at Workspace. Higher-level products translate
their own Project or work concepts before calling this boundary.

The local/cloud composition root applies a separate resource PEP to File,
MemoryStore, and Skill HTTP families. Embedded mode and Awaken Cloud mode use
different identity adapters but the same action/scope decision shape. Only an
explicit allow stamps `WorkspaceScope`; the inner Resource Catalog and content
stores never receive a principal, credential, role, decision, or policy object.
No-login local mode omits the PEP and injects the platform-provisioned default
Workspace, preserving the same resource contracts without a fake identity.

### D5: Lifecycle stages have explicit component owners

| Stage | Owning component | Responsibility |
|---|---|---|
| Configure | Resource Catalog application service and per-kind repositories | create definitions; publish immutable Memory/Repository config versions; maintain current version and live state |
| Bind | Agent configuration service / Managed Session adapter | persist Agent defaults or accept temporary Session attachments; carry identity, mount, access, instructions only |
| Resolve | `SessionInputResolver` + Skill resource resolver in the Session control plane | merge inputs once; resolve current config/Skill versions; validate paths/collisions; produce secret-free `ResolvedSessionResources` |
| Authorize | front-door PEP + authorization PDP/PIP | evaluate principal, action, Workspace, target facts, and active policy; return allow/deny/obligations |
| Activate | `SessionResourceCoordinator` + `SandboxProvider` + per-kind realizer | create activation records; materialize File, open Memory, clone Repo; inject short-lived credentials |
| Use | sandbox tools plus `FileStore`, `ScopedMemoryStore`, and Git/MCP adapters | enforce read/write capability and resource-specific consistency during the Session |
| Release | `SessionResourceCoordinator` + sandbox manager | release handles and credentials, preserve governed outputs, dispose Session-local material |
| Reclaim | `ResourceReclaimer` + per-kind repository/store | reconcile crashed activations; enforce retention; purge only after references and leases are gone |

The Session application state records `SessionResourceActivation` as
`Prepared -> Active -> Releasing -> Released | Failed`. It is a reliable cleanup
record, not a new product resource aggregate, and contains no secret or live
handle.

The implemented `SessionResourceState` keeps the applied manifest, an optional
pending manifest, a sequential revision, and the per-binding activation records
in one persisted Session value. Prepared/Releasing commits precede external IO;
Active/Released commits follow it. Startup reconciliation reads the separately
persisted trusted owner envelope and never imports a principal, role, API key,
PDP decision, or policy document into this state machine.

### D6: Each resource reclaims according to its own invariant

- File deletion revokes a Workspace grant first. Physical blob GC requires no
  remaining grants, Agent bindings, active/effective Session references,
  Artifacts, or retention hold.
- Memory Session release closes the scoped handle but never deletes long-term
  content. Store deletion tombstones the aggregate, denies operations, drains
  handles and durable extraction jobs, then purges content/history according to
  retention and records a receipt.
- Repository Session release disposes the working tree and credential material;
  it may preserve a patch/Artifact or push receipt. Deleting the platform
  Repository definition never deletes the external remote repository.

Repository activation crosses a neutral `RepositoryRealizer` port with a
secret-free `RepositoryRealizationPlan`. The plan contains the resolved config
version's URL, optional initial branch, mount path, identity, and access—never a
Git commit/tree pin or authorization object. Credential bytes are a separate,
ephemeral transport argument. Publication and authored-Skill persistence occur at
binding replacement or Session release; a Files GET is never a hidden write edge.

`Active -> Suspended -> Archived -> Deleted -> Purged` is the managed-resource
lifecycle. `Suspended` is a reversible live deny; `Archived` rejects new
activation; `Deleted` is a tombstone; `Purged` is an asynchronous reclamation
receipt rather than a reusable identity.

### D7: Memory has one data truth

Mount, recall, extraction, Memory API, version history, and redaction operate on
one Workspace-scoped `MemoryRepository`/`ScopedMemoryStore`. Internal entry
versions and hashes remain CAS and API concurrency data but never become binding
pins. `MemoryBlobStore`, one-file copy/harvest, and an independently committed
HTTP version repository are migration-only and must be removed after the unified
path is live.

FUSE is one realization. A host without FUSE may materialize path-addressed
entries and apply CAS changes through the same `MemoryRepository`; it may not
fall back to a second blob truth.

The Runtime Host therefore owns a store-less `MemoryRuntime` capability and a
Session-scoped `BoundMemory`. The latter contains only the already-resolved store
handle, pinned config policy, and maximum read/write access. A Session without a
MemoryStore binding has no recall or extraction. The removed
`with_memory(mem_dir)`/`memory_scope_root` path is not a fallback in local mode;
local mode provisions and binds a normal store under its hidden default Workspace.

### D8: Anthropic compatibility is an adapter concern

The Managed adapter continues to accept and project Anthropic File, MemoryStore,
and GitHub Repository resources. It lowers that wire into the typed binding
language. Native APIs use platform-managed ids; a compatibility request carrying
a Repository URL/token is represented as a Session-scoped managed definition and
credential binding before resolution. Public tokens are never forwarded as
Runtime domain values.

This preserves Anthropic behavior—Session resource input, current repository
clone, no commit pin—while allowing awaken Agent defaults and internal config
version governance.

### D9: Skill versions and bundles have one durable truth

`SkillDefinition`, immutable `SkillVersion`, and binary-safe `SkillBundleFile`
belong to one Workspace-scoped `SkillStore` repository. The former HTTP-local
`SkillRegistry` and current-text-only store are removed. SQLite/Postgres/FS and
in-memory adapters implement the same aggregate semantics; legacy rows are
imported once without continued dual writes.

At Session creation the selected latest version is frozen into
`ResolvedSessionResources.skills`. Retry and restart load that exact version and
verify its bundle SHA-256. The Runtime materializer revalidates relative paths,
rejects traversal/symlinks, preserves binary bytes, and writes supporting files
under `.skills/<skill-id>`. `allowed_tools` remains a monotonic gate layered after
platform authorization, so a Skill can only remove tool authority.

Deleting one version is therefore a logical retirement from authoring/list views,
not physical byte deletion: its ordinal is never reused and an already-persisted
Session pin can still load it. On restart the Session repository is read and its
frozen resources are installed before runtime history opens; current Agent or Skill
configuration is never transiently resolved into the old Session's sandbox.

### D10: First vertical slice

The first coherent slice is:

1. introduce typed `InputResourceId`, `InputBinding`, and
   `ResolvedSessionResources`;
2. resolve Agent defaults and Session attachments once at Session creation;
3. preserve `ResourceAccess` through activation and generate prompts from the
   effective result;
4. make File mounts binary-safe and read-only;
5. add versioned Memory/Repository definitions and resolve their current config
   version into the Session manifest;
6. route all Memory use through `MemoryRepository`, then remove blob/harvest;
7. add activation reconciliation and resource-specific reclamation tests.

## Consequences

### Positive

- The public model remains small: three input identities and one binding shape.
- Immutable and mutable resources no longer pretend to share version semantics.
- Agent defaults remain convenient while Session attachments and replacement are
  deterministic.
- Configuration is stable for a Session without claiming content replayability.
- Resolution, prompt generation, access, and mount provenance cannot drift
  between the Managed adapter and Runtime Host.
- Resource services stay independent of IAM language and product hierarchy.
- Deletion and physical reclamation become safe, auditable, and recoverable.

### Negative and accepted

- Recreating a Repository working tree may clone a different commit from the
  same pinned configuration version. This is intentional and documented.
- A Session can observe Memory content written by another authorized Session.
  CAS prevents silent lost updates; no snapshot isolation is promised.
- Memory/Repository config version repositories and activation reconciliation
  add durable control-plane state.
- Existing string DTOs, duplicate merge logic, and Memory blob compatibility
  paths require migration before the design is fully active.

### Rejected alternatives

- Pin every resource by content hash: rejected because Memory and Repository are
  deliberately mutable.
- Keep every resource live with no config pin: rejected because endpoint,
  credential binding, and behavior could change during recovery without an
  auditable Session input.
- Pin resource config in the Agent binding: rejected because Agent defaults are
  identity associations; new Sessions should receive the current published
  resource configuration without republishing the Agent.
- Let Runtime merge or resolve resources: rejected because it duplicates the
  control plane and leaks product configuration into execution.
- One generic resource store: rejected because creation, consistency, deletion,
  and reclamation invariants differ by resource family.
