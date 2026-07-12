# ADR-0053: Memory Store as a Write-Through FUSE Mount

- Status: Proposed
- Date: 2026-07-12
- Builds on: [ADR-0038](0038-managed-resource-injection-and-store-organization.md)
  (data-factored `MountRequirement`; `awaken-provisioning-contract` holds the
  mount vocabulary; `MountSource::MemoryStore` and `Realization::Fuse` are already
  reserved), [ADR-0041](0041-sandbox-execution-environment-provider.md) (the one
  process-level `SandboxProvider::create` provisioning seam; OS-boundary isolation)
- Reference: sibling repo `awaken-next` — `awaken-sandbox-memoryd` (a `fuser`-backed
  FUSE ↔ HTTP bridge) and its ADR-0057 (memory as HTTP sidecar), ADR-0064 (sandbox
  process boundary / fault domains), ADR-0066 (resilience), ADR-0091 (unified
  execution-environment contract)
- Relates to: G1/G13 (committed state is authority), G15 (control names the store,
  never its contents)

## Context

A bound memory store is made available to an agent today by **snapshot copy**: at
session prep the host reads the whole blob (`SharedHost.memory_get(id)`), inlines
it into a legacy `Mount::Resource`, and the provider `fs::write`s it to
`.mnt/<logical>`; after each turn the host scans `.mnt/` and `put`s the bytes back
(`harvest_thread_memory`). This is **last-writer-wins with no CAS**: two sessions
bound to the same store silently lose each other's updates, the agent sees a stale
whole-file copy for a whole turn, and there is no per-file granularity.

The Anthropic memory model — which `awaken-next` implements — is instead a
**directory of path-addressed memory files the LLM edits with native filesystem
verbs (read/write/edit/grep)**, backed by a durable store with optimistic
concurrency. `awaken-next` realizes this with `awaken-sandbox-memoryd`: a real
`fuser` FUSE filesystem that projects the store as `/mnt/memory/{store_id}/*`,
reads lazily through a short-TTL cache, and writes through on `flush`/`release` as
a **CAS (`content_sha256`) update**. That FUSE logic is code-complete and tested in
`awaken-next` (inode interning, per-fd buffer, LRU, dirty-fd budget, CAS conflict
handling, rename-keeps-open-fd) but is **not yet wired into any production launch
path** there, and it targets a **distributed** deployment (memoryd is a per-pod
sidecar talking HTTP to a Postgres-backed memory service, coherence maintained by a
NATS `memory.invalidate` broadcast).

Our repo differs in two ways that shape the port:

1. **Single process, in-process store.** The seam is
   `Arc<dyn awaken_memory_store::MemoryBlobStore>` held on `SharedHost`. There is no
   sidecar and no cross-process boundary in the common case, so the FUSE can call
   the store **in-process** — no loopback HTTP, no bearer token.
2. **The store is blob-shaped, not path-addressed.** `MemoryBlobStore` is
   `(workspace, store_id) → one opaque blob`, with **no paths, no versions, no
   CAS**. FUSE-for-memory is pointless over a single blob — its whole value is
   *many files with per-file concurrency*. So the store model must be upgraded
   first; this is the pivotal piece of work, not the FUSE code.

We additionally require **concurrent mounts**: the same `store_id` bound by two live
sessions must be mounted in both sandboxes at once, with coherent reads and no lost
writes. `awaken-next` solves cross-host coherence with a broadcast; on a single host
we can do strictly better and simpler (below).

`Realization::Fuse` and `MountSource::MemoryStore` are already reserved in
`awaken-provisioning-contract`; both local providers currently **fail loud** on a
memory-store mount ("no memory backend wired"). No `fuser` dependency exists yet.

## Decision

Port `awaken-next`'s memoryd FUSE design into this repo, adapted to a single
process and extended for concurrent mounts, in six slices. The FUSE *logic* is
ported almost verbatim (it is proven); the new cost is the **store-model upgrade**
and the **single-process/concurrent wiring**.

### D1 — Upgrade the memory model to path-addressed + CAS (the foundation)

Introduce an in-process **`MemoryFs` port** (a `crates/resources/` trait) whose
method shapes mirror `awaken-next`'s `MemoryClient`, but async and in-process:

```rust
#[async_trait]
pub trait MemoryFs: Send + Sync {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr>;
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr>;
    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr>;
    // CAS: base_sha mismatch -> MemErr::Conflict{current}. Never clobbers.
    async fn update(&self, store: &str, id: &str, content: &str, base_sha: &str) -> Result<Memory, MemErr>;
    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr>;
    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr>;
}
// Memory { id, path, content_sha256, content_size, updated_at, created_at, version, content: Option<String> }
// MemErr::{NotFound, Conflict{current: Box<Memory>}, TooLarge, PathConflict, Storage}
```

A durable `PathAddressedMemoryStore` backs it (inmem/fs/sqlite/pg, reusing the
`awaken.memory_store` scoped-migration bundle with a new `V0002__memories.sql`).
Store invariants required by everything downstream:

- Keyed `(workspace, store_id, path)`, **unique path** per store (blocks two
  concurrent `create`s of the same path).
- Every write carries `content_sha256` and a **monotonic per-path `version`**.
- `create`/`update`/`delete`/`rename` are each **one transaction** (pg tx; fs
  write-temp-then-atomic-rename; sqlite tx).
- `update` is **compare-and-swap on `base_sha`** — mismatch returns
  `Conflict{current}` (carrying the current sha + content for diagnostics), never
  overwrites.
- **`rename` over an existing target is a defined, atomic operation** — the store
  either atomically replaces the target within the transaction or rejects with a
  clear error; it is never left to incidental behaviour.
- `updated_at`/`created_at` are stored and **returned** (see D4 — attribute
  fidelity), 100 KB size cap, path validation (absolute, non-root, no `..`/`//`/
  control chars), and a `splice_bytes` DoS guard that projects final length
  **before** any `Vec::resize`.

The legacy `MemoryBlobStore` and the copy-in/harvest resource path are **left in
place** (they serve non-FUSE callers); the FUSE path is a new, parallel realizer.
The existing `memory_store_api.rs` HTTP facade is repointed at
`PathAddressedMemoryStore` (D-later), replacing its synthesized in-router registry.

### D2 — Port the FUSE server (`awaken-sandbox-memoryd`), swapping the transport

A new crate in the **`server` bucket** (`worker ⊥ resources`, so a crate that calls
a resources-tier store cannot be `worker`). It depends on `fuser 0.15` (feature
`fuse`, default on), `awaken-memory-store` (the `MemoryFs` port), and the
provisioning contract. `fuser` is added to `check_crate_boundaries.py`'s
`ALLOWED_DEPS`.

`fuse.rs` is ported **almost line-for-line** from `awaken-next` — inode/path
interning, `OpenFile` per-fd buffer, `ContentLruCache` (TTL 1s, cap 256, key=path),
the dirty-fd budget (128, fail-closed), `sha256_hex`, `splice_bytes`,
`MemoryMountHandle` (open-fd counting + bounded 5 s unmount drain), mount options
(`FSName("awaken-memory")`, `NoExec`/`NoSuid`/`NoDev`), errno mapping. The **only
substantive change** is that `MemoryClient` (HTTP) becomes `Arc<dyn MemoryFs>`; each
sync FUSE callback drives it via a dedicated tokio runtime + `block_on`, exactly as
before. Directories are synthetic (rolled up from path listings); `mkdir` is
store-free.

### D3 — FUSE conformance floor (ported invariants, non-negotiable)

The port must preserve every `awaken-next` ADR-0057 conformance-floor invariant, or
it becomes a silently-rotting component:

1. Buffer = whole file (≤100 KB); one fd = one buffer + a `base_sha` captured at
   open.
2. Flush = CAS `update(precondition = base_sha)`; on **409, keep the buffer and
   return `EAGAIN`** (writes are never dropped).
3. Dirty-fd budget 128, **fails closed** (never evicts buffered writes).
4. `rename` **remaps the inode in place**; already-open fds keep their id, base_sha,
   and buffer, and still CAS on their original base_sha.
5. Directories are synthetic; `mkdir` touches no store.
6. Content cache TTL 1 s, cap 256, key=path, consulted before every read.
7. Dedicated tokio runtime + `block_on` per callback.
8. errno: `Conflict`/`DirtyLimit` → `EAGAIN`, `NotFound` → `ENOENT`, else `EIO`;
   a daemon crash surfaces `EIO` to in-flight ops (never a hang).
9. Mount options as above; `unmount` drains open fds to a 5 s timeout, warns on
   leftovers, then joins.
10. Mount root `/mnt/memory/{store_id}`; `required_mount_kind() == "fuse"`; the
    provider must advertise fuse capability (fail-closed selection).

### D4 — File-attribute fidelity (one fix, the rest an explicit semantic boundary)

A FUSE memory mount is **not a general-purpose POSIX filesystem**; it serves
"LLM-edited memory text files". We make one fidelity fix and document the rest as a
hard boundary:

- **Fix (worth it): real timestamps.** `awaken-next`'s `attr()` fills every
  timestamp with `SystemTime::now()`, so `mtime` is meaningless and any
  mtime-dependent tool (`grep -newer`, `ls -t`, `make`, incremental scans) breaks.
  We thread the store's `updated_at`/`created_at` into `getattr` so timestamps are
  faithful. This is why D1 stores and returns them.
- **Correct as-is:** `size`, `kind` (regular/synthetic-dir), `nlink`.
- **Accepted lossy (documented boundary):** `perm` fixed (files 0644, dirs 0755),
  `uid`/`gid` fixed, so `chmod`/`chown` do not persist (`setattr` ignores
  mode/uid/gid); `utimes`/`touch -m` are ignored; no `xattr`/ACL; no symlinks or
  special files; empty directories are synthetic and **do not persist** across a
  remount; there is **no shared page cache across fds** (each open fd is an
  independent snapshot — read-your-writes holds only within one fd).
- **No multi-file atomicity:** each memory flushes independently under its own CAS;
  there is no cross-file transaction. Acceptable because memories are independent
  files; documented so callers never assume it.

### D5 — Concurrent mounts: one shared FUSE per `store_id`, refcounted (single-host)

The root cause of cross-mount incoherence is "one cache per mount". On a single
host we eliminate it structurally instead of importing `awaken-next`'s NATS
broadcast: **a `store_id` is mounted exactly once**, and every sandbox that binds it
shares that one mount.

A **`MountCoordinator`**, keyed by `store_id`, reference-counts the shared mount:

```
acquire(store_id, dest):
    lock the store_id entry
    if not mounted -> spawn one MemoryFuse at a neutral host path
                      (e.g. /run/awaken/memory/{store_id})
    expose that mount at dest (bind on the namespace tier; direct mount / bind on
      the workdir tier)
    refcount += 1
release(store_id, dest):
    detach dest; refcount -= 1
    if refcount == 0 -> unmount()  (drains open fds, ≤5 s)
```

- One `store_id` ↔ one `MemoryFuse` ↔ one `ContentLruCache`/inode table. N sandboxes
  are N views of the **same** fs, so **reads are constructively coherent** — there is
  no second cache to go stale, and no invalidation machinery is needed.
- **Writes stay CAS-serialized.** Two sandboxes opening the same file get two fds
  (two buffers, two base_shas) on the one `MemoryFuse`; the first flush wins and
  advances the store sha, the second conflicts → `EAGAIN`, keeps its buffer, and the
  agent reopens to retry. Concurrent mounts give **coherent reads + CAS-safe writes**,
  **not** merged concurrent edits (no CRDT) — stated plainly for callers.
- **Lifecycle safety via refcount:** one sandbox exiting only `release`s; the shared
  mount survives for the others and is unmounted only at refcount 0. A daemon crash
  surfaces `EIO` to all bound sandboxes (no hang); the supervisor re-establishes the
  mount without leaving the mountpoint dangling.

**Cross-host (distributed) is a later evolution**, not this ADR's target: when the
same store is mounted on two hosts a single shared mount is impossible, so each host
keeps a **version-validated cache** (revalidate the cached `(path, version)` against
the store — the in-memory `path → version` index D1 already maintains is the local
oracle; a remote oracle is a cheap version query) plus an **`Invalidator`** seam
(in-process bus now; NATS / pg-notify later). CAS write-safety is identical in both
models. The interface is designed so the distributed model bolts on without
reworking `MemoryFs` or the FUSE.

### D6 — The unit of work is the provisioning-contract mount, not a legacy bolt-on

The FUSE mount is implemented as a **realization of the
`awaken-provisioning-contract` mount** — `MountSource::MemoryStore { store_id }` →
`Realization::Fuse` — through the contract's `SandboxProvider::create` /
`Sandbox::attach(MountRequirement) -> RealizedMount` seam. `MountSource::MemoryStore`
and `Realization::Fuse` were reserved in that contract (ADR-0038/ADR-0041) for
exactly this; the realizer plugs in **behind** the contract, never as a parallel
mechanism and never bolted onto the legacy `Mount::Resource` copy path (which
ADR-0041 supersedes). Both contract providers currently **refuse** the mount
(`LocalProvider` `provider.rs:186`, `NamespaceProvider` `namespace.rs:197`); the work
is to make them realize it, and to fill the currently-stubbed `Sandbox::attach`
(`namespace.rs:434`).

- **Injection (no boundary break):** a provider takes an injected
  `Arc<dyn MemoryMounter>` (dependency-inverted exactly like the existing
  `NamespaceProvider::with_file_store`), calls it on a `MemoryStore`
  `MountRequirement`, and stamps `RealizedMount { realization: Fuse, mount_path:
  /mnt/memory/{store_id} }`. The provider itself takes no `fuser`/resources
  dependency; the realizer (a `server`-bucket `awaken-sandbox-memoryd` crate, D2)
  implements `MemoryMounter` and owns the `MountCoordinator` (D5).
- **Host route migration is part of the slice.** The host drives the *legacy*
  `LocalSandboxProvider` (copy-in `stage_one_resource` / harvest
  `harvest_thread_memory`) for the memory-store family today. Realizing against the
  contract means **routing the memory-store family through the contract provider**
  and retiring the copy/harvest pair for it — the non-throwaway path, aligned to
  ADR-0041's direction.
- **Capability-gated fallback (no FUSE → copy/harvest).** FUSE needs `/dev/fuse`,
  absent on macOS, in CI, and in unprivileged containers. `fuse_available()` gates
  the realization: when true, mount the live write-through FUSE; when false, the same
  path-addressed store is materialized the old way — `materialize` copies every
  memory out to plain files at prepare, and `harvest` folds them back after the turn
  (create new, CAS-update changed, skip unchanged). Both realizations back the one
  durable `MemoryFs`, so a store is portable across FUSE-capable and FUSE-less hosts.
- **Workdir tier first, then the namespace splice.** The Workdir `LocalProvider`
  realizes the FUSE mount at the sandbox path with **no mount-namespace splice**
  (directly visible to rooted tools); this is the first integration target. The
  bwrap `NamespaceProvider` is the harder second step (`awaken-next` left it
  unimplemented): bwrap enters a new mount namespace after `--unshare-user`, so the
  host FUSE mount must be **bind-propagated in** (host mount `rshared`, an extra
  `--bind <fuse_dir> /mnt/memory/{store}` in `bubblewrap_argv`, `/dev/fuse`
  available) — an independent, may-fail slice that does not block the Workdir tier.

### D7 — Slice plan

| Slice | Content | Acceptance |
|---|---|---|
| **P0** | `MemoryFs` port + `PathAddressedMemoryStore` (inmem+fs): path-unique, CAS, monotonic version, stored timestamps, atomic rename-replace | Rust unit: create / update-CAS-conflict / rename-replace / delete / list |
| **P1** | `awaken-sandbox-memoryd`: port `fuse.rs` over `MemoryFs`; conformance floor; real timestamps in `attr` | Ported unit tests: dirty-budget fail-closed, rename-keeps-open-fd, stale-fd-no-clobber |
| **P2** | `MountCoordinator` (refcounted shared mount per `store_id`) + mount handle drain | Unit: refcount acquire/release, open-fd drain |
| **P2.5** | Concurrency tests | (1) mount A writes → mount B reads fresh; (2) concurrent write same file → one wins, one `EAGAIN`, no lost update; (3) create-create race → one wins; (4) two sandboxes share store, release one, other still R/W, unmount only at 0 |
| **P3** | Contract provider wiring: `LocalProvider` realizes `MemoryStore` → `Realization::Fuse` via the injected `MemoryMounter`; route the host memory-store family through the contract provider (retire copy-in/harvest for it) | Kernel-VFS integration test (gated on `/dev/fuse`) + TS e2e: agent writes memory files, readable across turns and after restart |
| **P4** | sqlite/pg backends + repoint `memory_store_api.rs` at the new store | Existing managed-memory e2e green + durability e2e |
| **P5** | Bwrap namespace splice (mount propagation + `--bind`) | Bwrap integration test (gated); failure does not affect P3 |
| **(later)** | Distributed model: `Invalidator` (NATS/pg-notify) + version-validated cache | Two-host coherence test |

## Consequences

- **Net consistency improvement over the status quo.** Today's copy-in/harvest is
  last-writer-wins with zero CAS and full lost-update risk under concurrency. The
  FUSE path makes **write-write no-lost-update a strong guarantee** (CAS) and, via
  the shared single mount, **reads coherent across concurrent mounts** on one host —
  both are guarantees the current path simply does not have.
- **Atomicity is per-file, not cross-file.** Single-file writes are atomic and
  durable at flush (transactional store); there is no multi-file transaction, and
  `rename`-replace is atomic only because D1 makes the store enforce it. Buffered
  writes not yet flushed are lost on crash (POSIX-normal); an in-flight op at a
  daemon crash returns `EIO`.
- **Not a general-purpose filesystem.** Timestamps become faithful, but
  mode/owner/utimes/xattr/ACL/symlink/special-file/empty-dir semantics are
  intentionally absent — acceptable for LLM-edited memory text, and documented as a
  hard boundary so no caller assumes otherwise.
- **The FUSE risk is bought proven.** The load-bearing FUSE logic is ported from
  `awaken-next`'s tested implementation; the genuinely new work — and the new risk —
  is the store-model upgrade (D1) and the single-host concurrent wiring (D5), which
  are ordinary async-Rust and are unit-testable without a kernel.
- **Cost / dependency footprint.** Adds a `fuser` dependency (a `server`-bucket
  crate; boundary allowlist updated), needs `/dev/fuse` for the kernel path (all
  kernel-level tests gated; the copy path remains the fallback where FUSE is
  unavailable, e.g. CI/macOS), and introduces a per-`store_id` background mount whose
  lifecycle is refcounted against sandbox create/dispose.
- **Boundary preserved.** Providers depend on an injected `MountCoordinator` port,
  never on `fuser` or the store directly, keeping `worker ⊥ resources` intact.
- **Delivered:** P0 (`MemoryFs` port + CAS store), P1 (the FUSE port, proven by a
  real kernel-VFS integration test), P2/P2.5 (`MountCoordinator` + concurrency),
  P3 (95% changed-code coverage), and P4 (the durable path-addressed store backs the
  `/memories` HTTP endpoints, with a restart-durable TS e2e). `awaken-sandbox-local`
  was moved to the **worker tier** (it now resolves mount bytes through an injected
  `BlobSource` port and links no durable store), resolving the bucket question below.
- **Open questions carried:** the contract-provider realization of
  `MountSource::MemoryStore → Realization::Fuse` and the host route cutover from the
  legacy copy path (D6) remain a follow-on slice; whether the Workdir tier can expose
  the shared mount unprivileged (bind needs `CAP_SYS_ADMIN`; a symlink into the jail
  may be blocked by rooting) is a P5 spike; the bwrap namespace splice (P5) and the
  distributed `Invalidator`/version-oracle are deferred until they are needed.
