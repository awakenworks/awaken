# ADR-0038: Managed Resource Injection — Store Organization and the Provisioning Descriptor

- Status: Accepted
- Date: 2026-07-02
- Builds on: [ADR-0035](0035-environment-provisioning-tools-skills-resources.md)
  (provisioning is the seam; delivery is external; kernel is environment-agnostic),
  [ADR-0036](0036-skills-as-runtime-extension-single-tool.md) (skills as two
  tools over materialized files), [ADR-0037](0037-managed-capability-advertisement-wire-alignment.md)
  (Stage-1 wire alignment; `resources` advertised empty until a real producer exists)
- Relates to: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (managed
  protocol vs environment axis; presentation-only fingerprint), G3, G9, G13, G16, G21
- Reference cross-check: the `awaken-next` reimplementation (its ADR-0057 memory
  write-through, ADR-0060 file read-only fan-out, ADR-0064 fault domains, ADR-0091
  realized mounts, ADR-0101 path-free addresses, ADR-0108 capability reconcile,
  ADR-0088 plane isolation)

## Context

ADR-0037 closed Stage-1 by advertising `session.resources` as `[]` — honest,
because awaken had no Files-API-backed producer. This ADR records the design for
Part B: **real resource injection** for the three Managed Agents resource kinds,
and — the harder question — **how the resource stores are organized** so the
design scales from local-first to distributed without a rewrite.

The scope was fixed with the user as **maximal**: all three kinds (`file`,
`github_repository`, `memory_store`), a **durable** file store, and **both**
ingress paths (create-time inline `SessionCreateParams.resources[]` and the
`/v1/sessions/{id}/resources` subresource endpoints).

The wire shapes below were aligned from the installed official SDK
(`@anthropic-ai/sdk@0.105.0` typings under `e2e/node_modules`), never guessed
(the standing constraint). The organization was cross-checked against the
`awaken-next` reimplementation, which has mature analogues
(`awaken-sandbox-resourced`, `awaken-sandbox-memoryd`, `awaken-managed-contract`,
`awaken-management-contract`, `awaken-capability`). We borrow its *spine* and
deliberately decline its *distributed-first machinery* (per-kind daemons, a
capability kernel, a two-plane crate split) until distribution forces each.

### Aligned wire (ground truth)

- **Files API `/v1/files`**: `POST` multipart → `FileMetadata{id, created_at,
  filename, mime_type, size_bytes, type:"file", downloadable?, scope?:{id,type:"session"}}`;
  `GET /v1/files/{id}/download` → raw bytes; `GET /v1/files/{id}`; `DELETE` →
  `{id, type:"file_deleted"}`.
- **create-time inline `resources[]`** and their default mounts:
  - file: `{type:"file", file_id, mount_path?}` → `/mnt/session/uploads/<file_id>`
  - github: `{type:"github_repository", authorization_token?, url,
    checkout?:{type:"branch",name}|{type:"commit",sha}, mount_path?}` → `/workspace/<repo>`
  - memory: `{type:"memory_store", memory_store_id, access?:"read_write"|"read_only", instructions?}` → `/mnt/memory/<slug>`
- **`session.resources[]`** (resolved): file = `{id, created_at, file_id,
  mount_path, type:"file", updated_at}`; github adds `{url, checkout?}`; memory
  keys off `memory_store_id` (no top-level id) and renders `description`/`instructions`
  into the **system prompt**.
- **subresource `/v1/sessions/{id}/resources`**: `add` is **file-only**;
  github/memory attach only at create-time; `delete` →
  `{id, type:"session_resource_deleted"}`. Repository `authorization_token` is
  write-only on create and update: the Managed adapter seals it through the
  canonical Vault ingress, persists only an opaque binding plus exact revision
  pin, and never echoes material or the internal binding.
- **memory-stores** is an independent API `/v1/memory_stores` (id `memstore_…`).

The aligned Managed schema permits an explicit GitHub Repository `mount_path`;
`/workspace/<repo>` is only its default, not a protocol restriction. Awaken's
current *substitutable provider profile* has a narrower, explicit realization
capability: Repository working-tree continuity is guaranteed only for one
canonical child of `/workspace`. New Session resolution and Resource Manifest
replacement therefore return a client error before Repository/Vault/Git effects
for `/repo`, a relative path, aliases, traversal, or cross-platform separator
forms. Historical aggregates retain their exact durable value and fingerprint,
but runtime admission rejects an unsupported value before environment/Git
realization. No layer rewrites `/repo` into `/workspace/repo`. Widening this
profile requires Namespace and every Container/checkpoint/continuation backend
to preserve the same exact path; it is not an adapter-local normalization.

## Decision

### D1: Three store aggregates, never one `ResourceStore`

`file`, `memory_store`, and `skill` are three distinct aggregates with
incompatible write-side lifecycles — immutable content-addressed blob vs. a
mutable read-write named store vs. an immutable versioned package with lineage.
They get **three separate ports**, `FileStore` / `MemoryStore` / `SkillStore`,
each resolving an address to content/handle. A unified `ResourceStore` would be a
false abstraction that conflates the three lifecycles; it is rejected (the same
reasoning `awaken-next` follows with distinct `BlobSource` / `MemoryClient` /
`SkillRegistry` ports).

### D2: Unify at the *mount descriptor*, not the store — and factor it as data

The one thing all three stores share is what they hand the provisioner: a
provisionable mount reference. That is the unification point. Model it as a
**data-factored descriptor**, not an enum-per-kind:

```
MountRequirement {
    source:    SourceKind,       // File | Repo | Memory | Skill
    address:   ResourceAddress,  // path-free ref + content_hash (never a host path, G3)
    access:    Access,           // ReadOnly | ReadWrite
    lifetime:  Lifetime,         // PerRun | Durable
    mount_path: String,
}
```

A new kind is data (a `SourceKind` + a realizer), not a new enum variant threaded
through every match. This supersedes the ADR-0035 `Mount::Resource|…` enum sketch:
`SourceKind × Access × Lifetime` mirrors `awaken-next`'s `MountRequirement` /
`RealizedMount` and is the right shape for ADR-0035 D1's "typed provisioning
inputs". Provisioning stays a two-step seam: **compose** `Vec<MountRequirement>`
then **realize** each into a `RealizedMount` + `ProvisionReceipt` pin (ADR-0035 D3).

### D3: Store ports co-locate with their realizers; the contract holds only vocabulary

Evidence from `awaken-next`: the resource-store ports are **not** centralized in a
contract crate — `BlobSource` lives in `awaken-sandbox-resourced`, `MemoryClient`
in `awaken-sandbox-memoryd`, `SkillRegistry` in `awaken-ext-skills`, each beside
its implementation. Only the **DTOs** (`awaken-managed-contract`) and the **mount
descriptors** (`awaken-management-contract`) sit in contracts. We adopt the same
rule — and it matches our own precedent (`SandboxProvider` lives in
`awaken-sandbox-local` beside its impl):

- A new **`awaken-provisioning-contract`** carries *only shared neutral
  vocabulary*: `MountRequirement`, `SourceKind`, `Access`, `Lifetime`,
  `ResourceAddress`, `RealizedMount`, `ProvisionReceipt`, `content_fingerprint`,
  and the `SandboxProvider` seam. Near-leaf (no `Message`/truth types).
- `FileStore` / `MemoryStore` ports co-locate with their local realizers (in
  `awaken-coordinator-local` now; a future `awaken-sandbox-*d` when split out).
- `SkillStore` is the existing `awaken-ext-skills` registry, named as this family
  member (folding in `InMemorySkillRegistry`); skills materialize as a
  `SourceKind::Skill`/`Resource` mount (ADR-0036 D6), same as `awaken-next`'s
  `skill_brain_mount → MountSource::Resource`.

This corrects the earlier notion of pulling all three store ports into the
contract crate: **the contract carries descriptors/DTOs/vocabulary; store ports
follow their realizers.**

### D4: The resource wire vocabulary stays in the managed ACL (G16)

All Anthropic resource vocabulary (`type:"file"`, `github_repository`,
`memory_store`, `session_resource_deleted`, `agent_toolset_20260401`) is confined
to `awaken-protocol-managed::project`, per G16 and ADR-0037. The neutral
`provisioning-contract` and the store ports name none of it. A pure-serde
`awaken-managed-contract` + a `-bridge` ACL split (as `awaken-next` has) is a
possible later refactor, noted but not required now.

### D5: Attach/compose is control-plane; the runtime only consumes realized mounts

Following `awaken-next`'s plane isolation (its ADR-0088): the logic that resolves
inline/subresource `resources[]` into `Vec<MountRequirement>` and drives
(re-)provisioning is **host/control-plane** work. The kernel and `SessionRuntime`
gain **no** resource-store methods; the runtime consumes only `RealizedMount`s and
never calls back into the control plane (ADR-0035 D2, kernel stays
environment-agnostic). The session's attached-resource set is **session state +
`ProvisionReceipt`**, not committed runtime truth; memory's `description`/`instructions`
snapshot lives on the attachment (the wire says later store edits do not
propagate), while the store's canonical description lives in `MemoryStore`.

### D6: Distributed swap = reference-passing + config-time binding, realizers deferred

The design is distribution-ready without building distribution:

- **Reference-passing.** `MountRequirement.address` is a path-free
  `ResourceAddress` (ref + `content_hash`), never inline bytes — the same idea as
  `awaken-next`'s ADR-0101 and already the shape of the ADR-0035 D3 receipt. A
  provider (local or remote) resolves the ref against its **own** node's store
  handle; store and sandbox are never assumed co-located. (Local B-slices may
  still ship content inline via the existing `Mount::Resource{content}` path; the
  descriptor is what must be reference-shaped.)
- **Config-time binding.** Local ↔ distributed swaps by injecting a different
  store impl at host build (`Arc<dyn FileStore>` in-memory/on-disk vs. an HTTP
  client), not runtime polymorphism — as `awaken-next` binds `BlobSource` /
  `MemoryClient` at worker startup.
- **`MemoryStore` returns an opaque locator, not a `PathBuf`** — so a remote
  provider can mount it over a network FS/bridge without the port leaking a
  local-path assumption.
- **Deferred (YAGNI, distributed-first).** Per-kind realizer daemons (`-d` fault
  domains), a capability-reconcile kernel, and a two-plane crate split are
  `awaken-next`'s distributed machinery; we keep one `LocalSandboxProvider` that
  matches on `SourceKind`, and add them only when a concrete multi-node
  requirement lands. The port signatures (`ResourceAddress`, `content_hash`,
  `Access`, `Lifetime`, memory locator) are fixed now so that adding those impls
  later does not change the ports.

## Implementation slices

- **B0 — provisioning-contract + descriptor.** Extract
  `awaken-provisioning-contract`; relocate `SandboxProvider` / `Mount` / receipt;
  introduce the D2 descriptor; make `awaken-ext-skills` implement `SkillStore`.
  Behavior-preserving.
- **B1 — Files API.** Durable on-disk `FileStore` + `/v1/files`
  upload/download/get/delete. Verify with `client.beta.files.upload`.
- **B2 — file resources.** Inline + subresource `add/list/delete` →
  `SourceKind::File` mount → real `session.resources`. Verify: agent `read(mount_path)`.
- **B3 — github_repository.** `SourceKind::Repo` + clone/checkout realizer + token
  (and `update` rotation). External network/git dependency.
- **B4 — memory_store.** `/v1/memory_stores` CRUD + read-write `SourceKind::Memory`
  mount + system-prompt injection of description/instructions.

Each slice re-aligns its own shapes against the SDK before coding and `log()`s any
scope it bounds.

## Consequences

- One neutral descriptor spine (`MountRequirement`) carries every kind; adding a
  kind is data + a realizer, not a new port or an enum sweep.
- Contracts stay thin and correctly layered: `agent-contract` = truth,
  `runtime-contract` = execution, `provisioning-contract` = environment/delivery
  vocabulary — three orthogonal contracts; the kernel and the first two are
  untouched by resource injection (ADR-0035 D2).
- Store ports live beside their realizers, so a store's dependencies never leak
  into a contract; the ACL never learns a store.
- Local-first stays simple (one provider, no daemons); distributed is enabled, not
  built — the swap is a store-impl injection plus reference-shaped addresses.

## Alternatives considered

- **One `ResourceStore` trait for all kinds.** Rejected (D1): conflates three
  incompatible write lifecycles.
- **Put the three store ports in `awaken-provisioning-contract`.** Rejected (D3):
  `awaken-next` and our own `SandboxProvider` precedent both co-locate store ports
  with realizers; contracts carry only shared vocabulary.
- **Adopt `awaken-next` wholesale** (per-kind `-d` daemons, `awaken-capability`
  reconcile, managed-contract/-bridge/-api split, management/execution plane
  crates). Rejected for now (D6): that is a distributed-first, multi-crate build;
  we take its descriptor spine and plane isolation but defer the machinery until a
  real multi-node requirement exists, consistent with our local-first stage and
  Kent Beck simple design.
- **A `Mount::Resource|Git|Memory` enum** (the ADR-0035 sketch). Superseded by the
  data-factored `SourceKind × Access × Lifetime` descriptor (D2).
- **Grow `SessionRuntime` with attach/detach methods.** Rejected (D5): resource
  composition is control-plane; the runtime consumes realized mounts only.

## Amendment — 2026-07-05: resource-family taxonomy, Skill-as-registry, artifacts-as-harvest

A Part B design review (with the user) surfaced three refinements the original
decision under-specified. The realized code uses a `MountSource` enum
(`File | Resource | MemoryStore | Secret | Other`) in
`awaken-provisioning-contract::vocab`, plus `Sandbox::{artifacts, read_artifact}`
for the reverse channel; these rulings pin how the remaining kinds
(`github_repository`, `skill`) and the output path fit, and re-scope D1/D3.

### A1: Two resource families — stateful (has a Store) vs reference (no Store)

The load-bearing axis is **not** "how many aggregates" (D1) but **does the
resource own durable server-side state?** Two families:

- **Stateful → backs a Store**: `file` (immutable blob), `memory_store` (mutable
  keyed + versioned), `secret` (vault). Bytes/rows live in an awaken-owned store;
  a `MountSource` variant addresses that store by id/reference.
- **Reference → no Store**: `github_repository` (the remote *is* the truth; the
  working tree is ephemeral), `mcp` (a server URL), `multiagent` (an agent-id
  roster). The descriptor is lightweight config living in `ConfigRegistry`;
  realize resolves against the external truth and persists nothing.

Corollaries:

- **Descriptor weight ≠ realize weight.** All reference descriptors are light, but
  `github_repository` realize is heavy I/O (clone + egress credential proxy +
  push). "Lightweight" describes *ownership of state*, not *cost of realization* —
  a git mount is not free.
- Reference resources are **not** peers of the store aggregates in D1; D1's "three
  store aggregates" is hereby narrowed to *the stateful family only*.
- `mcp` and `multiagent` are provisioning axes (tool sets / sub-run rosters),
  **not** `MountSource` mounts, and need no resource store: MCP config +
  credentials reuse `ConfigRegistry` + the vault (broker reference); multiagent
  reuses the run/checkpoint stores (no new state).

### A2: `SkillRegistry` is a thin index over the shared blob store, not a store engine

Refines D1/D3. A skill is an **immutable, versioned content bundle referenced by
id** — structurally a `file` with a manifest and a version line, not a distinct
storage lifecycle. Therefore:

- **`SkillRegistry` is an index, not a Store engine**: `skill_id → [version →
  bundle content_hash]`. The **bytes live in the same content-addressed blob store
  as `file`**; the registry is a thin name/version table over it
  (ConfigRegistry-adjacent), not a fourth backend.
- **Prebuilt skills carry no storage** — they ship with the runtime and are pure
  references (Anthropic's `{type:"anthropic", skill_id}`); only **custom** skills
  occupy the blob store (Anthropic's `/v1/skills`).
- Realize: `MountSource::Skill{skill_id, version}` resolves through the registry to
  a `bundle content_hash`, then materializes read-only via **the same `File`
  realize arm** (`Realization::Copy`/`Bind`) — one extra registry lookup, no new
  realizer.

This supersedes D1's implication that `SkillStore` is a peer storage aggregate,
**and relocates D3's registry placement**: the registry is **control-plane** — the
host resolves `skill_id@version → bundle content_hash` at *compose time*, when it
decides which bundle to materialize as a mount — and is **not** owned by
`awaken-ext-skills`. `awaken-ext-skills` stays a pure **runtime consumer**: it
reads the already-provisioned skill files mounted into the sandbox (ADR-0036's two
tools) and touches no registry or store. The naming law stays intact — `File` is
the immutable-blob storage; `SkillRegistry` is the control-plane naming/versioning
index above it.

### A3: Resource prompts inject at config-bind time into the agent system prompt; artifacts are never runtime truth

**Corrects earlier drafts** (which rendered the outputs path into the prompt *per
Run at runtime*, and before that routed artifacts through a Checkpoint harvest —
both wrong). Two rulings:

**(a) Resource prompts are written at bind/resolve time into the agent's effective
system prompt.** When a resource is bound to an agent — the consumption binding,
mirroring `ProjectAgentConfig` / `AgentMcpConfig` for MCP — the resolver renders a
short description of that resource (its `mount_path`, `access`, a memory store's
`instructions`, a repo's branch, a skill's purpose, the outputs path) and appends
it to the agent's **effective** system prompt (base `AgentConfig.instructions` +
one fragment per binding), handed to the runtime at the config→runtime boundary
(ADR-0031) — exactly as an MCP binding resolves to a `ResolvedMcpServer`. This is
**config-time**, not per-Run runtime compose, and **every resource kind shares the
one mechanism** (a template per kind). `AgentConfig.instructions` itself (the config
domain's truth, part of the publication fingerprint) is **not** mutated — the
fragment is composed at resolve, like every other resolved value.

**(b) Artifacts are never runtime truth.** The sandbox→host output path
(`Sandbox::artifacts()` + `read_artifact()`, files under `outputs_path`) is not
recorded as committed truth. The agent already knows where to write because the
outputs path was injected into its system prompt at bind time (a); the host
retrieves bytes **on demand** via `read_artifact()` when a client/API downloads an
output. `awaken-agent-contract` / `Checkpoint` **do not learn about artifacts** and
need **no** change; G13 is untouched. Copying bytes into the blob store for durable
download is a control-plane option, never a committed run fact.

Three write-backs stay distinct:

| Write-back | Direction | Driver | Mechanism |
|---|---|---|---|
| **outputs / artifacts** | sandbox → host | host, on demand | outputs path is in the system prompt (bound at config time, a); bytes via `read_artifact()`; **no Checkpoint, no G13** |
| **durable mount** | sandbox → source | provider-internal | `ReadWrite`+`Durable` mounts write back at `dispose`/`attach` (memory → new version; secret → broker) |
| **git push** | sandbox → remote | agent (bash git) | via the host git-proxy; host does **not** harvest git |

`artifacts()` is idempotent and content-addressed (`Artifact.id = content_hash`),
so retrieval is reconnect-safe from any node that can `adopt` the sandbox — but
nothing about it enters run truth. Artifacts add **no** new storage and touch
neither `awaken-agent-contract` nor the Checkpoint.

### A4: Managed outputs become durable Files before Sandbox release

**Supersedes A3(b)'s on-demand byte retrieval.** Anthropic's external lifecycle
contract makes a Files API File independent from its producing Session. Therefore
the Session owner must harvest outputs before disposal; a Files GET cannot be the
write edge and cannot depend on an adoptable Sandbox.

Static ownership remains deliberately small:

| Owner | Authority |
|---|---|
| `Sandbox::artifacts/read_artifact` | the one runtime output enumeration/read port |
| `FileStore` | immutable content-addressed bytes (`blob_id`) |
| `FileCatalog` | public `file_...` identity, metadata, Workspace, optional Session scope/path, harvest idempotency and complete output observations |
| resource lifecycle index | File-to-blob reference preventing premature physical reclamation |

`FileCatalog` is the logical Files API aggregate, not an Artifact Store. An output
has no second metadata model: it is a File record whose `scope_id` and
`logical_path` identify its origin. Equal bytes may share one private blob while
remaining independent public Files.

The authoritative transition is:

```text
live step completion or terminal release
  -> read FileCatalog output capture revision
  -> Sandbox::artifacts/read_artifact
  -> verify Artifact.id == content_hash == FileStore content id
  -> FileStore.put(bytes)
  -> FileCatalog.create(file_..., session/path/blob metadata)
  -> add Artifact lifecycle reference to blob
  -> CAS the complete receipt File-id set against the observed revision
  -> only then may terminal release dispose the Sandbox
```

The harvest key `(session_id, logical_path, content_hash)` makes retries converge
on the already-created File. A harvest failure blocks terminal disposal so cleanup
can retry without losing bytes. `GET /v1/files` is a pure, Workspace-scoped
catalog query; deleting a Session removes its bindings/Sandbox but not harvested
Files. Deleting a File commits logical denial, removes its File-owned blob
reference, and schedules physical reclamation, which remains blocked while any
other logical File references the deduplicated blob.

Managed input Files use their public File id for the API and resolve through the
catalog to a private blob id. They are OS-enforced read-only at
`/mnt/session/uploads/...`; a provider without path fidelity and enforced
read-only capability fails closed instead of silently projecting a writable copy.

### A5: Complete output observations reference the canonical Files

A4's harvest key deliberately reuses an existing File when content changes
A→B→A at one logical path. Creation timestamps and catalog list order therefore
cannot select the output of the latest complete observation. No existing File
metadata field, terminal publication receipt, or Session status owns that choice.

**Static ownership.** Reuse `FileStore`, immutable File identities, publication
receipts, and File-owned lifecycle references unchanged. Extend the existing
`FileCatalog` and `FileApplicationService` with a complete output observation:
`ArtifactCapture { workspace_id, session_id, revision, file_ids }`. The only new
persistence is the V6 reference row in the same catalog migration scope; it
contains no duplicated File metadata, bytes, ownership index, or Run ledger.
Memory, SQLite, and PostgreSQL share the contract's admission and CAS reducer.
SQL revision encoding reuses the foundation's `StoredU64` boundary.
V5 migration preserves all Files and does not infer a capture for old Sessions.

**Dynamic contract.** A caller reads the revision before enumerating outputs,
uses the existing publication path to persist every File and reference, then
submits the entire File-id set with that expected revision. Replacement validates
all selected Files under the same catalog lock/transaction: exact Workspace and
Session, live downloadable artifacts, valid distinct logical paths, canonical
harvest identities, and no duplicate ids. One invalid member rejects the entire
replacement. CAS conflict requires a fresh observation, not a revised expected
revision attached to stale bytes. An exact response-loss replay acknowledges the
same revision and canonical set without another mutation.

No capture means unknown; an explicit empty set means the complete observation
was empty. Deleting a selected File leaves its exact id in the observation and
the canonical download path denies access. It never falls back to another
same-name File. Captures add no blob references and do not revive deleted Files.
A capture records observed outputs; it does not assert Run success or the user's
final acceptance.

**Execution wiring.** The existing generic `ArtifactPublisher` owns capture
reads and replacement alongside per-File publication and terminal receipt
recovery. `ApplicationArtifactPublisher` delegates to `FileApplicationService`;
the local Coordinator decorator and registered-Worker HTTP adapter apply their
existing Run/checkpoint/terminal authority before every capture edge. Run guards
remain held across the File operation. Both topologies compare Workspace and
execution Thread with the locked Dispatch through
`CommitEpochGuard::matches_execution_scope` and reject an expired claim. A child
Run retains its own output scope; its parent Session recovery/history affinity
does not authorize reading or replacing the parent's outputs. Worker identity
checks remain at the HTTP boundary; checkpoint and terminal operations retain
their existing Session-root decisions.
No capture-specific execution permission or Resource authority is introduced.

The shared `ArtifactHarvester` used after Native/ACP/A2A attempts reads and
verifies the observation before the existing complete Sandbox scan, publishes
all File receipts, then submits the entire selected set. A publication failure,
invalid receipt or CAS conflict fails the harvest without a partial replacement
or an automatic rebase. An already-failed attempt retains its original execution
error while still attempting output preservation, as before. Exact replacement
replay is accepted; a new complete scan may advance the observation revision
even when its bytes are unchanged. A missing Environment is not an observed
empty directory. Terminal receipt-only recovery remains read-only and does not
invent a capture from historical File metadata.

**Public read projection.** `awaken-protocol-awaken` owns the read-only
`GET /v1/awaken/files/output-capture?scope_id=...` path and its strong wire types.
The existing Resources router injects a method adapter over the same
`FileApplicationService` and required edge-selected Workspace as standard Files.
The authorization route registry classifies this extension under `file.read`;
its existing Hosted profile and Workspace path wrapper project the same family.
A scope id is a File filter, not a Session existence query: an unknown or foreign
Session filter returns no capture in this Workspace and creates no Session state.
Missing Workspace fails 404, invalid query fails 400, and unavailable/corrupt
capture evidence fails 503 instead of being represented as unknown. There is no
public replacement endpoint. Revisions are decimal strings so JavaScript clients
retain the full portable catalog range without integer rounding.

Cowork reads this observation before fetching standard Files metadata, partitions
current versus historical files by the exact returned ids, and downloads by id
through the existing authenticated Files API. Unknown, explicit empty, deleted
selected File, and failed observation reads have distinct presentations. A missing
selected File stays unavailable; neither a historical version nor successful bytes
from another id can replace it. A failed refresh removes the current presentation
until authoritative reread succeeds; transient read errors clear after recovery.
Historical Files and immutable bytes keep their original lifecycle. An observation
is still not a Run success or user acceptance declaration.

**Implementation boundary (2026-09-06).** Catalog/application ports, three storage
adapters, Harvester, local/HTTP publication adapters and public read projection
are wired. Cowork consumes exact output selection and preserves historical Files.
Mechanism and API/UI tests are not completed Native/Claude multi-turn business E2E
or evidence of equivalent behavior in the Claude Cowork application.
Cause/effect rules live beside catalog/application, Worker HTTP transport,
Coordinator fencing and Runtime Host tests. They cover database migration and
reconnect, first-write races, A→B→A, authority and response validation, failures
at each capture stage, exact acknowledgement-loss replay and no stale rebase.

### Consequences of the amendment

- Resources has **exactly one content-addressed blob store** as its byte
  substrate — shared by `file` in and `skill` bundles — plus one mutable keyed
  store (`memory_store`) and the vault (`secret`). Output artifacts reuse the File
  aggregate and blob store; they do not add an Artifact Store or a parallel byte
  backend. The skill registry is a control-plane index, not a backend.
- Every resource kind's prompt is injected **at config-bind/resolve time** into the
  agent's effective system prompt (A3a), mirroring the MCP binding path — not
  rendered per-Run at runtime; provisioning realizes bytes/mounts and never
  injects prompts.
- Adding `github_repository`/`skill` stays "a `MountSource` variant + a realize
  arm": `skill` reuses the `File` arm; `github_repository` reuses the
  egress-substitution (broker) seam for auth (`EnvValue::Secret` /
  `EgressOnly`), keeping credentials out of the sandbox (G3).
- `SkillRegistry` naming stands (D3) but is re-scoped as an index, not a storage
  engine; D1's aggregate count is scoped to the stateful family.

## References

- [ADR-0035](0035-environment-provisioning-tools-skills-resources.md),
  [ADR-0036](0036-skills-as-runtime-extension-single-tool.md),
  [ADR-0037](0037-managed-capability-advertisement-wire-alignment.md).
- Official SDK typings: `e2e/node_modules/@anthropic-ai/sdk/resources/beta`
  (`sessions/resources.d.ts`, `files.d.ts`, `sessions/sessions.d.ts`,
  `memory-stores/`).
- `awaken-next` crates: `awaken-sandbox-resourced` (`BlobSource`),
  `awaken-sandbox-memoryd` (`MemoryClient`), `awaken-ext-skills` (`SkillRegistry`),
  `awaken-managed-contract` (DTOs), `awaken-management-contract`
  (`MountRequirement`/`RealizedMount`), `awaken-capability` (`reconcile`).
- [INVARIANTS.md](../INVARIANTS.md) — G3, G9, G13, G16, G21.

## Amendment — 2026-09-05: configurable provider-relative resource prompt variables

`ResolvedInput` remains the sole resource-binding authority and its existing
optional `instructions` is the configuration surface; no global template store
or backend-specific prompt track is added. The one resource prompt projection
renders File, MemoryStore, and Repository bindings for Native and ACP alike and
supports only these namespaced variables:
`{{awaken.workspace}}`, `{{awaken.outputs}}`,
`{{awaken.resource.binding}}`, `{{awaken.resource.path}}`,
`{{awaken.resource.directory}}`, `{{awaken.resource.logical_path}}`, and
`{{awaken.resource.access}}`. Unknown or unterminated `awaken.*` variables fail
before realization; unrelated braces remain authored text.

Substitution scans authored text once: replacement values are literal data and
cannot introduce additional substitutions. `resource.path` is the resource's
file or directory path; `resource.directory` is its lexical parent, including
for MemoryStore and Repository directory mounts. Tests require exact output for
all variables, both path separators, repeated tokens, and Unicode path names.

The frozen logical mount is never replaced by a host-absolute path. Namespace,
Container, and Native Hand tools render sandbox-absolute POSIX paths.
TrustedHost/Workdir ACP renders relative paths with the platform separator so
the CLI resolves them from its working directory without sending the
operator's private root through the model prompt. In that ACP view,
`{{awaken.workspace}}` is the relative path from the effective Agent cwd to
`/workspace` (`.` for the default cwd and `../..`, for example, from
`/workspace/repo/src`). Filesystem Memory
mounts keep their typed access intent there, but read-only is explicitly
advisory because an unconfined host Agent can mutate it; isolated Workdir
admission still rejects the same request, while Namespace/Container retain
enforced read-only. Filesystem Memory renders a mount instruction; semantic Memory
renders binding-scoped memory-tool instructions and rejects templates that use
workspace, outputs, path, or directory variables. Thus path choice occurs only
after the single Session Environment is known, while resource identity, access,
instructions, and generation remain pinned by the existing binding.

TrustedHost Repository presence must not promote the frozen Workdir spec to
Namespace during resource projection. Late File bytes use the existing Sandbox
`attach` port and rooted atomic writer, including manifest replay after adoption.
Create-time Memory participants cannot be replaced through that File port;
Memory teardown/recovery remains owned by its original mount evidence and store
CAS, including explicit conflict reporting for stale recovered copies.

The executable portability gate is `scripts/ci/check-trusted-host-portability.sh`.
It runs the same resource mission with native executable ACP peers for Codex and
Claude: official SDK resource creation, default/nested cwd, File reads, Memory
writes from concurrent Sessions, Repository edits without implicit remote
publication, and output download. File/Memory/Repository carry independent
random bytes; the output must combine all three and the exact Repository HEAD.
Downloaded patches must apply to an independent checkout and reproduce those
bytes. Each receipt has a new marker, so an old artifact cannot satisfy recovery.
The same live Session IDs continue after normal process replacement and SIGKILL;
before writing again they must read back their previous Memory and Repository
edits. This crash point is between turns, not while an ACP tool is executing.
The live `host-login` profile in `e2e/codex_acp_live_e2e.mjs` reuses that mission
and oracle; it requires an available real login and explicit live opt-in. One
pure selector (`e2e/acp_live_profile.mjs`) enables Linux scoped execution for
both live and fixture TrustedHost profiles. Its decision-table tests cross all
eight supported profile/runtime combinations with Linux, macOS and Windows;
the same selected flag drives deployment, Hand build features and the
process-produced PID-namespace oracle. This is test deployment selection, not
a second production capability detector. Non-Linux rows do not claim Linux
supervision, and Namespace/Container keep their own existing boundaries.
The local Git fixture
uses the existing `test-support` local Git transport aperture; the production
Repository transport remains HTTPS-only and does not inherit host Git identity.
Agent-owned Git commands are a separate trusted-process identity boundary. Fixture success
does not certify a vendor CLI, and a configured OS matrix is not evidence of
executed macOS/Windows tests. Cause/effect rules live next to their test owners.

The same runner's `native-fixture` profile uses the production CLI, its immutable
Hand companion, and the existing fake Messages upstream to execute that **same**
resource mission and independent output oracle through Native Bash in Namespace.
The Linux portability row requires this substrate; it cannot pass by skipping a
missing Namespace backend. Native changes the tool cwd explicitly between
`/workspace` and `/workspace/repo/src`: `model.configuration.working_directory`
remains an ACP setting, not a newly supported Native Agent configuration. This
distinction is part of the test, rather than claiming equivalent configuration
surfaces from equivalent file I/O. The local Git transport still needs
`test-support`; this does not close the production HTTPS acceptance gap.

Copy-mode concurrent same-base update/update, update/delete, create/create and
identical-edit cases are owned by
`awaken-sandbox-memoryd/tests/copy_lifecycle.rs`: synchronized dispatch plus both
ordered linearizations, retry, lost acknowledgement and durable SQLite reopen.
They also distinguish a missing/replaced root from intentional unlink, reject
unsafe/incomplete scans before write-back, and verify per-file partial CAS
commit and recovery without claiming multi-file atomicity. The official SDK CAS
matrix in `e2e/managed_memory_repository_durable_e2e.mjs` drives two clients with
the same base hash, both ordered linearizations and simultaneous dispatch,
identical/different updates, exact retries, version history and restart. It is
public API evidence, not two Agents contending on one mounted path.
The shared Agent resource mission additionally gives both mounted Sessions the
same two initial Memory heads: equal edits converge, distinct edits preserve a
single winner. It checks exact head/version history after concurrent release
and restart through the public API, for both ACP fixture identities and Native
Namespace. The registered Worker uses its existing copy-only mounter. This
exercises mounted Agent composition. Its R7/R8 assertions now additionally
require the losing proposal through the Memory API, the canonical Session
delete tombstones (transactionally gated by verified completed cleanup), removal
of both physical output projections, and candidate preservation across another
Worker restart. This is not in-flight
crash fencing or a vendor CLI/cloud acceptance claim.
The commit/ack fault matrix in `tests/writeback_faults.rs` adds 24 actual SQLite
teardown interruption rows, including a committed prefix and exact reopen.
Eight further rows pause inside the actual SQLite transaction while its caller
is cancelled, then retry before/after the background commit. Recovery now
feeds captured bytes into the same reconciliation function without an
intermediate plaintext directory; unusable-temp-location subprocess tests cover
success and cancellation. Physical projection admission also tests parent
aliases, Unicode normalization and exact no-edit path/ID/history preservation.
Both live and recovered copy writeback now admit all captured content as UTF-8
before any Repository mutation. Silently skipping binary bytes could otherwise
let terminal cleanup dispose their only copy while claiming reconciliation
success. The existing copy lifecycle suite owns a 16-row decision matrix:
live/recovered, new/replaced head, first/last invalid entry, read-write/read-only.
It asserts exact source and durable history preservation on repeated rejection,
followed by repair, idempotent replay and SQLite reopen. Read-only mounts still
do not publish local edits. The existing Host terminal Memory table adds resident
and cold rejection/retry rows: no acknowledgement, provider preparation or
physical disposal may follow rejected reconciliation; the exact binding survives
and the same terminal effect resumes after participant recovery.

Terminal Copy reconciliation preserves valid losing proposals in the original
Memory aggregate before it acknowledges reconciliation. `MemoryRepository`
owns this storage operation, its content-addressed proposal identity (original
path/base plus proposed content or deletion), bounded store-local capacity,
readback and whole-store purge. SQLite and PostgreSQL serialize admission;
the only schema append is in the existing converged Memory migration bundle.
Retaining a proposal changes neither live heads nor successful version history.
Identical proposals coalesce; these are content records, not another Session
operation ledger or a guarantee against later legitimate head transitions.

The existing flow remains `terminal fence -> stop/capture -> original-base CAS
-> retain every conflict -> acknowledge exact Memory evidence -> provider
prepare -> aggregate-authorized disposal`. A failed or unsupported retention
write leaves cleanup retryable, with the source intact. Twelve additional
fault rows interrupt candidate preservation before/after its real SQLite write,
by error/cancellation, for create/update/delete; retries and reopen must preserve
the exact candidate and winner. The deeper SQL-trigger matrix also cancels a
caller inside the candidate INSERT transaction.

Worker HTTP retention is terminal-v2-only and checks the same Worker, live
Session root, binding and original path/id/hash. Run v1 and realization v3
cannot append candidates. Embedded execution uses the same repository after
Host terminal admission. Content never moves to Files or a separate cloud
store. The Awaken-only `GET /v1/awaken/memory-stores/{id}/copy-conflicts` extension
is route-owned by `awaken-protocol-awaken`, using its existing injected-handler
pattern to reuse Store visibility and pagination, hiding content unless
`view=full`; the Anthropic Memory and version schemas are unchanged.
This does not supply automatic semantic conflict merging, change read-only
behavior, retain invalid/binary copies, or prove OS process-tree quiescence.
The existing `MemoryCAS` model adds the copy-recovery configuration, with
automatically rejected rebase, incomplete-scan-delete and replaced-root mutants.
It separates request admission, caller cancellation, late durable commit and
acknowledgement loss; a fourth mutant must reject erasing an in-flight request
on caller cancellation. Worker login heartbeat reordering runs
against the production registry under Loom; the existing Kani lease kernel and
TLA+ activation/projection/reclamation models retain their own boundaries. See
[formal verification](../../formal/README.md) for the exact model scope.

The local `ProcessHandle` construction boundary is now `LocalProcess::spawn` in
`awaken-local-process`. Existing ACP, Workdir and Namespace launchers keep their
argv, cwd, environment/credential projection and channel ownership unchanged;
they no longer separately configure, spawn, and wrap a raw child. Spawn enforces
the private Unix process group and direct-child `kill_on_drop` policy before
returning any handle. Mutable one-shot stdin/stdout extraction cannot expose or
replace the owned child. This also closes the normal-drop direct-child orphan window in the
Local/Namespace launchers that previously omitted `kill_on_drop`.

The lifecycle sequence is `project command -> spawn -> detach configured pipes
-> share handle -> poll/wait/signal`. Waiting releases the child mutex before
backoff; cancelling a waiter does not relinquish process ownership. A final
handle Drop requests direct-child termination, whereas abrupt Worker death does
not run Drop. Neither successful signal delivery nor a cached leader exit is a
whole-tree termination receipt. `ProcessHandle` explicitly documents that limit;
no process exit ledger, credential store or parallel cleanup machine is added.
Unix group signalling treats only accepted delivery or exact `ESRCH` as success.
`EPERM` and all other errors remain errors even after the leader exits; the old
leader-exit fallback could conceal a surviving group's failed termination. A
real process test reaps the leader while a descendant holds stdout, then requires
group KILL to close that pipe before the descendant's voluntary backstop. The
errno decision table includes ambiguous macOS denial; Windows keeps its existing
direct-child implementation. Numeric process-group identity reuse and detached
descendants remain limitations; this change does not provide a quiescence receipt.

The cause/effect rows next to the Rust tests cover executable failure, overridden
caller group/drop settings, cwd with spaces/Unicode, literal environment values,
one-shot pipes and stdout EOF after Drop while detached stdin remains open.
`LocalProcessLifecycle.tla` independently explores two waiters, cancellation,
signals, OS exit, normal Drop and Worker crash. Its five negative mutations must
fail the exact spawn, parked-lock, cancelled-lock, false-crash-cleanup and
denied-signal-after-leader-exit checks. Unix signals do not take the child mutex;
the model admits them concurrently with polling and cancellation.
This is bounded model checking, not Rust refinement or an OS containment proof.

The Linux portability gate also exercises the existing Namespace PID isolation
and `--die-with-parent` boundary without changing either mechanism. A separate
provider-owner process launches a three-node Bash tree; both descendants use
`setsid`, report their distinct process-group/session identities, verify the
managed File mount, and hold live sockets. The test SIGKILLs the owner (no Rust
Drop or graceful disposal), then requires all sockets to close before the fixture
backstop. Removing `--unshare-pid` must make that exact termination oracle time
out; restoring it restores the passing result. This is real Linux Namespace
substrate evidence, not a TrustedHost guarantee, full Worker/Coordinator failover
test, or authorization to reclaim a directory based only on leader exit.

These gates do not yet establish a complete release matrix: in-flight ACP
process fencing, a production-feature HTTPS resource composition, actual mixed
cloud/local Worker placement and loss, and exhaustive Agent-level conflict
linearizations still need dedicated end-to-end evidence. Windows local
process termination currently targets the direct child, unlike Unix process
groups; neither proves ownership recovery after abrupt Worker death while a
host Agent or descendant is still performing effects. Actual
vendor logins, macOS/Windows execution, and OS credential-manager/SSH integration
must be verified on their corresponding hosts; fixture or model success does
not substitute for them.

Linux supervision qualification uses the explicit
`check-trusted-host-portability.sh --linux-supervision` gate, with no systemd
manager or delegated cgroup dependency. The former systemd/cgroup adapter and
its separate test path have been replaced, not retained as a fallback.
`awaken-local-process::scoped::ScopedProcess` owns one execution tree:
sanitized bubblewrap -> trusted namespace helper -> private command and
descriptor handoff -> JOINED -> one-use GO -> workload. Namespace and host-view
launchers share `BUBBLEWRAP_PROCESS_BOUNDARY`; mount/network policy remains
provider-owned.

The bootstrap control socket is an inherited socket pair, not a discoverable
abstract address or a credential file. Its initial stdin is control-only; the
workload receives separate stdin/stdout/stderr descriptors via SCM_RIGHTS.
Received descriptors are CLOEXEC. The helper starts without workload environment
or loader hooks, applies the provider-resolved physical cwd exactly once, and
never renders payload bytes in diagnostics. The private codec remains distinct
from the non-serializable planning contract. Native filesystem tools and opaque
Agents still share the existing resource/prompt projection authority.

The helper sends a namespace descriptor and its init pidfd before GO. The caller
must durably bind that physical identity while retaining the realization lock,
then check CURRENT authorization and commit `ScopedLaunchGrant` inside the same
synchronous guard used by revocation. Missing commit denies; backpressure retries
with fresh authorization; an error after GO cannot discard the launched handle.
This callback is an integration seam, not a newly installed Session lease source.

Forced scope stop targets the pinned init pidfd; TERM/INT travel through the
private helper channel to the workload's process group, preserving graceful
signal semantics. Wait/poll require both the local owner
result and init termination observation, rather than treating leader exit as
tree emptiness. Drop only requests termination; errors/timeouts are never durable
cleanup receipts. The helper watches control disconnection and a conservative
BOOTTIME execution cutoff (including host suspend). The caller must derive that
cutoff from remaining current authority. `refresh_deadline_authorized` now reuses
the GO grant's synchronous authorization/retry seam to send a newer absolute
BOOTTIME cutoff. It is a transport projection, not a second lease owner or
scheduler. The caller derives and commits it inside the current Session guard;
omitted authorization denies, retry rechecks authority, and a committed frame
cannot be undone by a later callback error. A send receipt is not a helper ACK.
Signals and renewals share one short-write lock on the private channel. A partial
write shuts down that channel rather than retrying a suffix after releasing
authority; a zero-byte WouldBlock leaves it usable for fresh authorization.
The helper preserves partial-read state across cutoff checks, rejects expiry
before applying any queued renewal, and ignores older cutoffs. Transit delay
never adds time. Explicit revocation must force-stop the scope; denying a new
cutoff does not revoke an older still-live one.

Session lease-to-cutoff propagation, Session-wide launch/close
serialization, crash-safe marker participation, Native Hand routing and Worker
capability activation still require production integration. Built-in supervision
capabilities therefore remain false.

The real binary tests live in
`crates/bin/awaken-sandbox/tests/linux_scoped.rs`: physical cwd with spaces/Unicode,
synthetic credentials, binary stdio, loader-time namespace membership, denied
grant, failed exec, detached descendants, exact scope kill, separate owner
SIGKILL, helper SIGKILL, stalled-owner SIGSTOP and execution expiry (including
expiry while waiting for GO). TERM/INT must reach the exact workload trap before
normal exit. Renewal tests distinguish accepted/new, stale, denied, omitted,
expired and retry-then-revoked updates. The accepted update keeps all detached
members alive past the original cutoff; the replacement cutoff subsequently
terminates them even when the owner is SIGSTOPed. An observed exited scope
rejects further updates without calling the authorization adapter.
Descendants each retain their own
external observer socket; a forwarding process's EOF is not the oracle.
An independently live neighboring scope must survive every target termination;
cancelling its waiter retains ownership. Malformed private frames and descriptor
handoffs fail closed, and pipe EOF proves that rejected received descriptors do
not leak. The process boundary model reuses the existing admission/GO model,
distinguishes init stopping from exited, and checks all four failure links under
explicit kernel-progress fairness. `LocalProcessRenewal.cfg` composes that same
model with a bounded absolute clock, one queued renewal and a stalled proper
frame prefix. It checks fresh send authority, monotonic cutoff, no resurrection,
no minted transit time and clock progress despite a stalled prefix; five exact
mutants must violate those named properties. Unit tests own actual byte framing,
concurrent writer serialization and real short-write/EOF behavior. The model
does not prove a hard wall-clock bound, business lease renewal, host-kernel
correctness or production Session recovery.
Unavailable PID namespace/pidfd support fails rather than becoming a successful
skip. These tests qualify the Linux adapter, not full Worker/resource recovery,
real vendor login or cloud/local deployment.

This is crash containment for trusted local work, not isolation from a hostile
host user. Host-view namespaces can preserve physical paths while changing
supplementary-group and host-PID semantics; those compatibility constraints must
be qualified before admitting a TrustedHost supervision profile. External jobs
created through Docker or remote APIs remain the corresponding resource owner's
responsibility and cannot be undone by terminating a local execution tree.

Earlier cross-compilation of `awaken-local-process --tests` and
`awaken-sandbox-memoryd --lib` passed for `x86_64-pc-windows-gnu` and
`x86_64-apple-darwin` (the latter selects Clang for its target C compiler).
These are type/build checks, not executed platform tests. In particular, the
shared `awaken-sandbox-fs` still returns `Unsupported` on Windows for nofollow
directory identity and descriptor-relative atomic materialization. Workdir File
mounts call that materialization primitive in `awaken-sandbox-local/provider.rs`;
compiling its dependencies cannot qualify that runtime path. The separate
quiescent Memory copy's `ProjectionRoot` already uses retained same-file handles
on Windows and must not be replaced with a duplicate identity mechanism.
Apple Silicon cross-checking also needs a Darwin C sysroot for BLAKE3's NEON
build; this Linux host's Clang header search fails before checking Awaken code.
These implementation and toolchain gaps remain explicit release blockers.

The scoped-launch change also cross-checks `awaken-local-process --tests` and
the thin `awaken-sandbox --bin awaken-sandbox` on Windows GNU and Intel Darwin.
The broader Windows sandbox `--tests` check is blocked on this Linux host by
the missing MinGW C compiler required by its existing `ring` dev-dependency;
the passing thin-binary check does not substitute for that test target.
The Hand-enabled Seatbelt integration-test cross-check is likewise blocked on
this Linux host: `ring`'s target C build needs a Darwin sysroot. The portability
gate now requires the real Seatbelt test when run on macOS, and unavailable
Seatbelt is a failure, not a passing skip. Neither Linux path-renderer tests
nor the passing local-process cross-check qualify that macOS execution row.

Memory/Repository live addition remains an explicit
create-time-only contract, not a missing hot-plug implementation to emulate.


## Amendment — 2026-09-08: file identity accompanies the executable path

The existing `ResourcePromptProjection` renders a File reference as JSON data
containing the frozen File ID, binding ID and the `AgentPathView` path. This is
a process-local projection of `ResolvedInput`, not another resource record or
mount resolver. Namespace/Container use the sandbox path; Workdir uses the
existing provider-relative view. No provider-private host prefix is published.

A client selecting an existing File sends its immutable File and binding IDs.
It must not present the requested Managed `mount_path` as a shell path: File
realization confines that request beneath the upload root, and the Environment
may further require a relative view. The Agent joins the selected identities
to the current Runtime reference before reading or restoring bytes. Missing
identity/path evidence cannot authorize a same-name replacement or a guessed
mount prefix. Binding publication, attachment retries and File history retain
their existing owners and persistence behavior.

The resource-prompt decision tables own identity/path correlation, equal-name
versions, escaped identity data and absolute/relative views. This change adds
no authority transition or persistent state machine; those executable tests
complement the existing projection models without claiming formal proof of
model interpretation or filesystem availability.
