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
  - github: `{type:"github_repository", authorization_token, url,
    checkout?:{type:"branch",name}|{type:"commit",sha}, mount_path?}` → `/workspace/<repo>`
  - memory: `{type:"memory_store", memory_store_id, access?:"read_write"|"read_only", instructions?}` → `/mnt/memory/<slug>`
- **`session.resources[]`** (resolved): file = `{id, created_at, file_id,
  mount_path, type:"file", updated_at}`; github adds `{url, checkout?}`; memory
  keys off `memory_store_id` (no top-level id) and renders `description`/`instructions`
  into the **system prompt**.
- **subresource `/v1/sessions/{id}/resources`**: `add` is **file-only**;
  `update` is **github token rotation only**; github/memory attach only at
  create-time; `delete` → `{id, type:"session_resource_deleted"}`.
- **memory-stores** is an independent API `/v1/memory_stores` (id `memstore_…`).

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
  `awaken-server-local` now; a future `awaken-sandbox-*d` when split out).
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
turn at runtime*, and before that routed artifacts through a Checkpoint harvest —
both wrong). Two rulings:

**(a) Resource prompts are written at bind/resolve time into the agent's effective
system prompt.** When a resource is bound to an agent — the consumption binding,
mirroring `ProjectAgentConfig` / `AgentMcpConfig` for MCP — the resolver renders a
short description of that resource (its `mount_path`, `access`, a memory store's
`instructions`, a repo's branch, a skill's purpose, the outputs path) and appends
it to the agent's **effective** system prompt (base `AgentConfig.instructions` +
one fragment per binding), handed to the runtime at the config→runtime boundary
(ADR-0031) — exactly as an MCP binding resolves to a `ResolvedMcpServer`. This is
**config-time**, not per-turn runtime compose, and **every resource kind shares the
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

### Consequences of the amendment

- The resource plane has **exactly one content-addressed blob store** as its byte
  substrate — shared by `file` in and `skill` bundles — plus one mutable keyed
  store (`memory_store`) and the vault (`secret`). Reference resources add zero
  storage; **artifacts add none either** (outputs path injected into the system
  prompt at bind time, bytes read on demand); the skill registry is a control-plane
  index, not a backend.
- Every resource kind's prompt is injected **at config-bind/resolve time** into the
  agent's effective system prompt (A3a), mirroring the MCP binding path — not
  rendered per-turn at runtime; provisioning realizes bytes/mounts and never
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
