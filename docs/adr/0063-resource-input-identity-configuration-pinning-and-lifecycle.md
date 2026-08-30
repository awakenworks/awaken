# ADR-0063: Resource Input Identity, Configuration Pinning, and Lifecycle Ownership

- Status: Accepted
- Date: 2026-07-21
- Amended: 2026-08-01 — logical File identity and canonical Resources application
- Amended: 2026-08-10 — typed application-contributed Session inputs
- Amended: 2026-08-12 — one front-door IAM enforcement path
- Amended: 2026-08-27 — profiled creation, File item fencing, and Session-owned Repository adoption
- Amended: 2026-08-28 — explicit terminal Repository publication
- Amended: 2026-08-31 — expected-prior Repository CAS and durable rejection
- Builds on: [ADR-0038](0038-managed-resource-injection-and-store-organization.md)
  (resource injection and provisioning descriptors),
  [ADR-0041](0041-sandbox-execution-environment-provider.md) (sandbox lifecycle),
  [ADR-0053](0053-memory-store-fuse-mount.md) (path-addressed Memory and CAS), and
  [ADR-0061](0061-selectable-identity-and-platform-managed-resource-scopes.md)
  (Workspace ownership and IAM boundary)
- Detailed design:
  [Resources, Memory, Files, And Skills](../design/resources-memory-files-skills.md)
- Coordinated credential execution amendment:
  [ADR-0067](0067-credential-custody-model-exposure-and-secret-delivery.md)
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

- a logical File has an immutable mapping to a content-addressed blob;
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
| File | logical `FileId` | the same immutable logical id; Resources resolves its immutable `blob_id` mapping | a separate File version or caller-visible blob id |
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
separate canonical `skills[]` collection. An absent legacy field decodes to the
same empty list as an explicit empty selection; neither authorizes a mutable
global-catalog fallback. Each non-empty entry freezes
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
credential. Repository configuration contains only a binding/reference. Before
the resolved input enters the Session aggregate, the Session application freezes
the bound credential's exact active revision, canonical usage, `Forbidden`
model-exposure policy, and Environment-selected Resource holder as a secret-free
execution pin. Material is still opened only at activation or a remote operation
and is never persisted in `ResolvedSessionResources`.

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

All File, MemoryStore, and Skill HTTP adapters consume the shared
`RequiredWorkspaceScope` extractor. Missing or empty scope fails as not found;
an adapter never falls back to `Host.local_workspace`, parses an API key, or
constructs a Workspace. The local composition root stamps its hidden default
Workspace once, while authenticated compositions stamp the Workspace selected by
their PEP. Catalog ownership middleware consumes that stamp without rewriting it.

`ResourceConfigSource` is deliberately a narrow resource-domain port. Its
implementations verify the trusted Workspace ownership edge, current lifecycle
state, and current-version integrity internally, then return only the selected
`MemoryStoreConfigVersion` or `RepositoryConfigVersion`. They do not return an
authorization envelope or duplicate the resource definition into Session state.
Consequently an API/composition PEP can be embedded locally or backed by a remote
IAM service without changing the catalog, resolver, or data-plane contracts.

Activation and later Memory writes use the separate `ResourceBindingValidator`.
It receives the trusted Workspace, resource id, and already-frozen
`ConfigVersion`; it verifies current ownership/lifecycle plus the existence and
identity of that exact immutable version, returning no configuration. Runtime
therefore never calls `resolve_*` a second time and never substitutes the current
version for the Session pin. This validator is a resource-invariant port, not a
PEP or PDP: it has no principal, role, API key, policy, or allow/deny decision.

### D5: Lifecycle stages have explicit component owners

| Stage | Owning component | Responsibility |
|---|---|---|
| Configure | Resource Catalog application service and per-kind repositories | create definitions; publish immutable Memory/Repository config versions; maintain current version and live state |
| Bind | Agent configuration service / Managed Session adapter | persist Agent defaults or accept temporary Session attachments; carry identity, mount, access, instructions only |
| Resolve | `SessionInputResolver` + Skill resource resolver in the Session control plane | merge inputs once; resolve current config/Skill versions; compile Repository binding to an exact secret-free credential access/holder pin; validate paths/collisions; produce `ResolvedSessionResources` |
| Authorize | front-door PEP + authorization PDP/PIP | evaluate principal, action, Workspace, target facts, and active policy; return allow/deny/obligations |
| Activate | `SessionResourceCoordinator` + `ResourceBindingValidator` + `SandboxProvider` + per-kind realizer | validate the exact frozen config without re-resolving current; create activation records; materialize File, open Memory, clone Repo; inject short-lived credentials |
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

Durable dispatch carries `SessionResourceManifest { workspace_id, revision, resources }`
beside the executable Agent snapshot. This envelope is secret-free and is not a
second Session model: `resources` is the exact persisted
`ResolvedSessionResources`, `revision` is its corresponding selected active or
pending generation, and `workspace_id` is the trusted intrinsic partition
already recorded by the Session. The aggregate's `SessionResourceState::revision`
remains an attempted-generation watermark after rollback and must never be paired
independently with the restored active resources. A cold worker verifies that the
Workspace equals the dispatch `execution_scope`, installs the manifest before
sandbox creation, and never re-reads current Agent bindings. A warm worker may
replay the exact generation or advance to a newer one; it rejects older and
same-revision/different-value manifests so a delayed Run cannot roll back live
inputs. Delegated child Runs inherit the same envelope because they execute in
the parent's Session environment.

### D6: Each resource reclaims according to its own invariant

- File activation verifies immutable bytes and creates a Session-local copy with
  no write-back authority. An Agent may edit that working copy, but publishing it
  creates a new File or Artifact; it can never mutate the original `FileId`. File
  deletion removes a Workspace ownership edge first. Physical blob GC requires no
  remaining ownership edges, Agent bindings, active/effective Session references,
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
Git commit/tree pin or authorization object. Its separately persisted credential
pin is validated and opened through the common exact material resolver; credential
bytes remain an ephemeral transport argument. Publication and authored-Skill persistence occur at
binding replacement or Session release; a Files GET is never a hidden write edge.

`Active -> Suspended -> Archived -> Deleted -> Purged` is the managed-resource
lifecycle. `Suspended` is a reversible live deny; `Archived` rejects new
activation; `Deleted` is a tombstone; `Purged` is an asynchronous reclamation
receipt rather than a reusable identity.

The implemented reclamation workflow writes a durable, idempotent
`ResourcePurgeIntent` before the logical File ownership removal or catalog/Skill
tombstone. A lease-generation and revision fence prevents a stale worker from
overwriting recovery. Physical adapters run only after the independently composed
catalog, reverse-reference, Agent-binding, Session-binding, retention, runtime,
and extraction guards report no blocker. These guards contain resource facts only;
authentication, API keys, roles, policy and PDP decisions remain at the PEP edge.

Multi-node deletion additionally uses `ResourceReclamationFence`, keyed by the
physical identity `(ResourceKind, resource_id)` rather than Workspace. Fence
acquisition and the zero-reference check are one resource-store transaction;
reference creation/replacement takes the same identity lock and fails closed while
the durable fence exists. The coordinator re-runs independently owned guards after
acquisition, performs idempotent physical deletion, releases the fence, and only
then commits the receipt. A crash before release lets the same intent resume; a
crash after release but before the receipt safely rechecks/repeats deletion.

Embedded composition uses local SQLite/filesystem adapters and cloud/multi-node
composition uses Postgres adapters, selected together once by
`AWAKEN_RESOURCE_DATABASE_URL`. The former `AWAKEN_RESOURCE_LIFECYCLE_DB` and
runtime-Postgres DSN fallbacks are transitional compatibility aliases; operators
should set the resource axis explicitly. Selection covers File bytes, Memory
content/history, Skill definitions/bundles, and lifecycle/reference/fence state,
while each bounded context retains its own port and migration scope. This is a
composition bundle, not one god repository.

The resource databases contain only resource content, intrinsic Workspace
ownership/reference edges, purge work, and consistency fences. They contain no
principal, token, API key, role, policy, PDP decision, credential material, Org,
Project, or WorkUnit. Resource backend selection is never derived from identity
mode or policy. Authentication and authorization remain independently deployable
at the PEP/PDP edge.

This separation is mechanically enforced by
`check_resource_authorization_isolation`: every dedicated resource-plane crate
is denied IAM/authz dependencies (including Cargo package aliases); resource
application modules living in mixed-role crates are scanned explicitly;
authorization-domain types and fields are denied in production Rust; and the
same fields are denied in resource SQL migrations. The check intentionally
permits Workspace because it is the resource partition/ownership coordinate
stamped by the PEP, not evidence that authorization was granted.

`ResourceAuthorities` injects the Resource Catalog, File store/catalog, Memory,
Skill, and lifecycle ports atomically when the process is constructed. A shared
deployment therefore never opens an unused local File/Memory/Skill/lifecycle
store before replacing it. A shared Runtime also requires the complete Resource
backend family, including its canonical Resource Catalog, to be shared; startup
fails closed when shared execution is combined with a node-local Resource
component. Agent bindings are published by Control and do not grant the
Coordinator direct ownership of Control's admin store.

Remote worker placement requires `session-resources/v1` whenever a frozen
manifest is present, including an explicit empty selection: the capability also
owns revocation of material from an earlier manifest. A credentialed Repository additionally
requires `repository-credentials/v1`. A worker advertises the former only when
the complete shared Resource component and its Resource Catalog validator are
installed, and the latter only with an explicit shared credential backend. Missing
or mismatched wiring is therefore an admission incompatibility, not a late
node-local fallback.

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

Memory behavior is mutable configuration, not mutable content and not IAM policy.
The Workspace-scoped Memory management API reads current or historical config and
publishes recall/extraction/retention changes with an explicit
`expected_config_version` CAS. Publication appends one immutable config and moves
the current pointer atomically; it never snapshots Memory entries. Descriptive
definition updates remain a separate operation so a rejected CAS has no partial
metadata side effect.

Reliable extraction is Session application work, not part of the Memory resource
aggregate and not an authorization decision. A durable `MemoryExtractionIntent`
freezes the terminal commit, Workspace/store/config binding, transcript, and the
secret-free inference access pin. The Session application repository owns its
lease-fenced `Pending -> Claimed -> Extracted -> Stored -> Completed` convergence;
`MemoryRepository` owns only CAS/idempotent mutation. Restart reconstructs a
missing terminal outbox intent, reclaims an expired process lease, and remounts a
dead FUSE realization before resuming. The resource stores never receive a
principal, API key, role, policy document, or PDP result.

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

#### 2026-08-23 amendment: capability-selected MemoryStore delivery

One frozen MemoryStore binding now has two mutually-exclusive runtime
projections, selected by the same effective filesystem-capability decision as
ADR-0036 Skills:

| Delivery | Runtime projection | Prompt contract |
|---|---|---|
| `ManagedFilesystem` | the existing `MountSource::MemoryStore` path and ordinary file tools | binding/display identity, mount path, access, and authored usage instructions |
| `SemanticTools` | no Memory mount; `list_memories`, `read_memory`, `write_memory`, `delete_memory` | binding/display identity, access, authored usage instructions, and the required `binding` argument |

This does not add a second Memory implementation. Both rows use the same
Session-scoped `BoundMemory`, live Resource validation, and D7
`MemoryRepository`. Semantic writes are create-only without
`expected_sha256`; updates require the current hash. Deletes require the exact
entry id and hash, preserving the repository's CAS and path-recreation ABA
fence. Read-only bindings reject both mutation tools at the data-plane adapter.

Multiple stores remain one frozen map keyed by Session `BindingId`. Every
semantic call must select one binding explicitly; model arguments never contain
Workspace ids, physical store ids, repository handles, or authorization state.
Native installs the shared descriptor/executor set as Session tools. ACP exports
that exact set—together with other Host-owned Session tools—through one MCP
server and one lifetime lease. A filesystem delivery exposes none of these
semantic tools, and a Session delivery mode cannot change after first
projection.

The optional Awaken automatic recall/extraction extension remains a separate,
explicit policy that selects one binding. It may contribute bounded
request-only recall and durable post-commit extraction, but it neither chooses
the standard binding set nor replaces direct file/semantic Memory operations.

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
under `.skills/<skill-id>`. The `.skills` directory is an exact runtime-owned
projection: every context rebuild replaces it, and a manifest transition removes
the old tree before the next Run can read a retired script or support file.
`allowed_tools` remains a monotonic gate layered after platform authorization, so
a Skill can only remove tool authority.

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
4. make File projections binary-safe and disconnect their writable working copies
   from FileStore mutation;
5. add versioned Memory/Repository definitions and resolve their current config
   version into the Session manifest;
6. route all Memory use through `MemoryRepository`, then remove blob/harvest;
7. add activation reconciliation and resource-specific reclamation tests.

### 2026-08-12 amendment: all typed Session inputs are supplied before creation

ADR-0075 removes the late Worker-authored input path. Agent defaults and direct
Session attachments are resolved through the same `SessionInputResolver`
before the Session root is inserted. The resulting
`ResolvedSessionResources` is committed with the frozen baseline.

Callers carry logical Resource identities only. A File input therefore contains
a logical `FileId`; it never contains a Resources blob id, storage URL, or
provider-specific mount key. Required replacements remain explicit through
`SessionInputAttachment::replaces`. Invalid identity, replacement, collision,
or ownership is rejected before physical realization.
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
- A shared Postgres runtime cannot silently use node-local resource content or a
  local fence; startup requires the complete shared resource backend family.

### Negative and accepted

- Recreating a Repository working tree may clone a different commit from the
  same pinned configuration version. This is intentional and documented.
- A Session can observe Memory content written by another authorized Session.
  CAS prevents silent lost updates; no snapshot isolation is promised.
- Memory/Repository config version repositories and activation reconciliation
  add durable control-plane state.
- Versioned resource-plane schemas and one-time legacy imports add an explicit
  deployment migration step, even though runtime dual writes have been removed.

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

## Amendment: one front-door IAM enforcement path (2026-08-12)

D4's separate resource PEP described a separate policy namespace, not a license
to place two authentication/authorization middleware layers on the same HTTP
request. That implementation shape is superseded. Every public domain router is
wrapped exactly once by the canonical front-door IAM edge. Its one route-family
classifier selects either the management action profile or the resource action
profile before calling the same local or Cloud IAM adapter.

The static boundary remains unchanged: File, MemoryStore, Skill, Session, and
A2A services receive only a trusted `WorkspaceScope` and never receive identity
or policy types. The dynamic boundary is now unambiguous: authenticate once,
fence the requested Workspace once, select the action namespace, obtain one PDP
decision, stamp the Workspace only on Allow, and enter the handler. Deny,
approval, transport failure, and an unmapped route terminate at that same edge.
No `resource_guard` compatibility layer remains.

## Amendment: profiled inputs freeze in the original root (2026-08-27)

Profiled product adapters now pass direct `SessionInputAttachment` values beside
Repository inputs into the existing `create_profiled_session` composer. Agent
defaults and those direct inputs enter the one `SessionInputResolver` before
`SessionCreationIntent::finalize`; the initial `ResolvedSessionResources` is
therefore complete in the original Session insert. A post-create whole-manifest
call is not an alternative creation or recovery path.

After creation, Resource lifecycle and realization remain unchanged. The
Session application first applies the immutable baseline mutation authority
owned by
[ADR-0066](0066-session-service-binding-and-realization.md#2026-08-27-amendment-immutable-post-create-mutation-authority),
then admitted changes reuse the existing whole-manifest root CAS and realization
driver. The Resource Catalog, resolver, and protocol adapter neither infer a
product mode nor maintain another mutable policy.

### Session-scoped Repository adoption and mutation fencing

A Repository definition created while lowering one Managed or profiled Session
is a participant in the Session root command, not an independently successful
aggregate. Registry creation and optional Vault credential creation each return
their own `Applied | Replayed` provenance. Pure request, path, collision, Skill,
Environment, and baseline-policy validation runs before either participant.
Before the root is adopted, a failure compensates only an `Applied` participant;
it never retires a replayed participant. Compensation first reads the durable
root: an unavailable or corrupt root is preserved for reconciliation, a
concurrent winner that references the participant has adopted it, and only an
unreferenced participant is retired. Whole-manifest replacement uses this same
participant/root boundary rather than a second cleanup path.

After root adoption, the existing Session Resource state and reconciler own
release; there is no protocol-local cleanup saga. When a replacement generation
becomes Active, the same root commit moves each Repository present in the old
generation but absent from the successor into an exact durable retirement
intent. That intent remains both reconciliation work and a Resource/Vault
retention edge until cleanup and its completion CAS succeed. Local realization
attempts cleanup immediately after the Active commit; externally realized and
restarted Sessions are discovered by the same reconciliation scan. Terminal
cleanup consumes the same intents together with the active and pending
generations. Item DELETE and whole-manifest omission therefore have one
manifest/CAS/retirement path.

While an intent is unsettled, prepare and unattempted revise reject a successor
that reintroduces the same Repository id. This fail-closed admission barrier is
required before the external effect: detecting a later root CAS conflict cannot
undo a Repository already retired by an older reconciler. Cleanup may tombstone
a Session-created Repository only when both its closed
`managed:{session}:repository:*` or `profiled:{session}:repository:*` namespace
and the canonical owner-kind/session metadata match. Namespace resemblance,
missing metadata, or a shared definition is a successful no-op and never grants
Vault authority. For an owned inline credential, cleanup first retires the exact
source revision through the canonical Vault archive/material-reclamation path,
then retires the Repository. Either failure preserves the intent for idempotent
receipt replay or restart; an already absent Registry definition completes as a
no-op without inferring credential ownership.

The ordinary Managed File item create/delete verbs read one Session revision,
derive the complete desired manifest, and pass that exact revision to the same
root CAS. They do not rebase after a concurrent writer: one mutation wins and a
loser receives conflict (`409`) with the winner's manifest intact. Explicit
whole-manifest replacement retains its caller-supplied `If-Match` fence and the
same no-lost-update rule.

## Amendment: explicit terminal Repository publication (2026-08-28)

An exact Git branch and commit remain outside Repository binding and Session
configuration resolution. They become durable only when a caller explicitly
requests publication while releasing an idle Session. The application selects
exactly one active writable Repository binding, copies its already-frozen
`ResolvedInput`, and atomically archives the Session with a
`RepositoryPublicationExpectation` containing the symbolic branch and full
commit. It does not resolve the current Repository configuration, credential,
or remote branch head again. Ordinary release with no publication intent keeps
the existing behavior and exact v1 cleanup JSON and fingerprints.

### Static ownership

- Reused unchanged: `ResolvedInput` remains the configuration and credential-pin
  authority; `SessionCleanupOperation` remains the only durable terminal-effect
  operation; `RepositoryRealizer` remains the only clone/publish effect port.
- Modified: the cleanup operation may carry one optional immutable publication
  intent and its canonical receipt, and the same Worker Repository verifier may
  validate either ordinary Run-claim use or terminal command plus realization
  lease authority.
- Added: typed publication expectation, command, and receipt values bind the
  existing authorities together. They add no queue, registry, Resource
  generation, credential store, or second Git implementation.

The Session aggregate owns intent, command identity, ordering, and receipt
admission. The Environment adapter owns Git filesystem and transport effects.
The remote provider remains the external Git truth. A mediated transport may
replace only the effect URL: the frozen source URL remains the Repository
identity recorded in the receipt. Neither credential bytes nor a Gateway
capability enters durable Session or Resource state.

### Dynamic ordering and recovery

```text
archive + explicit expectation
  -> root CAS freezes the terminal fence and exact ResolvedInput
  -> quiesce parent; freeze root and delegated-child cleanup targets
  -> settle every child cleanup receipt
  -> derive one root-only Repository publication command
  -> RepositoryRealizer verifies exact local branch + HEAD
  -> exact remote already present: return canonical receipt
     remote absent: create only under an absent-ref lease, then verify
     remote at another commit: fail without overwrite
  -> root CAS records the canonical publication receipt
  -> only then expose root cleanup and dispose the shared working tree
```

Every phase is recoverable from the same cleanup operation. Command or response
loss replays the exact command; first success and already-current replay produce
the same receipt without a changed flag. A mismatched intent or receipt fails
closed, and a cleanup operation already frozen without publication cannot be
upgraded after archive.

### 2026-08-31 amendment: exact update lease and durable rejection

`RepositoryPublicationExpectation` may additionally freeze one
`expected_prior_commit`. Omission retains the original create-only contract:
only an absent remote ref may be created. Presence authorizes exactly one update
when the first remote observation equals that full commit. The sole Git adapter
uses the observed value as an exact force-with-lease precondition; it never
turns the field into general overwrite authority. A remote already at the
desired commit remains the absorbing replay for either form.

Desired, expected-prior, and observed Git object ids use one canonical wire:
exactly 40 lowercase hexadecimal characters. Uppercase or otherwise
non-canonical text is rejected before a Git write rather than normalized, so
one object cannot produce two command fingerprints or a false stale outcome.

After every push attempt, including a process-level failure, the adapter
reobserves the exact remote ref. Desired means the response was lost and returns
the canonical receipt; an unchanged absent/prior observation remains retryable;
an absent update lease or a third commit is a typed permanent rejection. No Git
transport, authentication, observation, or I/O error may claim permanent stale
evidence.

Permanent rejection is retained beside the receipt in the existing terminal
publication sidecar, with exactly one outcome bound to the same command
fingerprint. It is committed through the Session root CAS before ordinary root
cleanup may dispose the working tree. A local coordinator or registered remote
Worker records that same command-bound outcome through the existing realization
control; neither path adds a queue, store, phase machine, or Git writer. Exact
profiled replay returns the durable rejection without another Git call.
Every aggregate encode/decode and every Completed suppression or tombstone gate
rebinds that outcome to the outer Session id; an intent-local but foreign
receipt or rejection is corrupt evidence, never terminal truth.

Both the expected-prior field and rejection sidecar are first-write contract
changes under deny-unknown decoding. Deployment must therefore raise the
Session application, Runtime Host, registered Worker, and ingress/control plane
to the same contract floor before either value is emitted. A pre-floor request
continues to omit the prior and can only use the historical create-only path;
mixed old/new writers are not a compatibility mode.
