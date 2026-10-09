# ADR-0053: Memory Store as a Write-Through FUSE Mount

- Status: Accepted
- Date: 2026-07-12
- Implemented: P0-P5, including the bwrap splice (`crates/worker/awaken-sandbox-local/src/namespace.rs:407-412`,
  test `bwrap_splices_a_live_fuse_memory_mount_into_the_namespace` in
  `crates/worker/awaken-sandbox-local/tests/namespace_provider.rs:1114`); see Amendment 2026-10-09
- Amended by: ADR-0063 D7 (`MemoryBlobStore` and one-file copy/harvest removed; one Memory truth)
- Amended: 2026-10-09 (reconciled with ADR-0063: legacy blob path removed, crate tier, splice delivered)
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
`.mnt/<logical>`; after each Run the host scans `.mnt/` and `put`s the bytes back
(`harvest_thread_memory`). This is **last-writer-wins with no CAS**: two sessions
bound to the same store silently lose each other's updates, the agent sees a stale
whole-file copy for a whole Run, and there is no per-file granularity.

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

Introduce an in-process **`MemoryRepository` port** (a `crates/resources/` trait) whose
method shapes mirror `awaken-next`'s `MemoryClient`, but async and in-process:

```rust
#[async_trait]
pub trait MemoryRepository: Send + Sync {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr>;
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr>;
    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr>;
    // CAS: base_sha mismatch -> MemErr::Conflict{current}. Never clobbers.
    async fn update_head(&self, store: &str, id: &str, content: &str, base_sha: &str,
                         target_path: Option<&str>) -> Result<Memory, MemErr>;
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
- `create`/`update_head`/`delete`/`rename` are each **one transaction** (pg tx; fs
  write-temp-then-atomic-rename; sqlite tx).
- `update_head` is the aggregate operation used by the public API: content, an
  optional path change, replacement of an occupied destination, and all history
  rows commit or roll back together. `update` is its content-only convenience.
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
a resources-tier store cannot be `worker`). It depends on `fuser 0.16` (feature
`fuse`, default on), `awaken-memory-store` (the `MemoryRepository` port), and the
provisioning contract. `fuser` is added to `check_crate_boundaries.py`'s
the metadata-derived context/layer rules.

`fuse.rs` is ported **almost line-for-line** from `awaken-next` — inode/path
interning, `OpenFile` per-fd buffer, `ContentLruCache` (TTL 1s, cap 256, key=path),
the dirty-fd budget (128, fail-closed), `sha256_hex`, `splice_bytes`,
`MemoryMountHandle` (open-fd counting + bounded 5 s unmount drain), mount options
(`FSName("awaken-memory")`, `NoExec`/`NoSuid`/`NoDev`), errno mapping. The **only
substantive change** is that `MemoryClient` (HTTP) becomes `Arc<dyn MemoryRepository>`; each
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

### D5 — Concurrent mounts: independent projections over one repository

Each sandbox owns its mount path and teardown guard. `MemoryStoreMounter`, constructed
once per host, wraps the configured `MemoryRepository` with one `LocalInvalidator`;
every FUSE projection subscribes to that bus. A successful mutation publishes
`(store_id, path)`, peer projections evict the matching cache entry, and their next
read fetches the durable head. This avoids a privileged bind/namespace splice and
keeps one production realization path across Workdir and Namespace providers.

- **Writes stay CAS-serialized.** Concurrent file descriptors carry independent base
  SHAs; the first flush advances the durable head and a stale second flush fails with
  `EAGAIN`. Edits are not merged and there is no CRDT.
- **Lifecycle is projection-local.** Disposing one sandbox tears down only its own
  mount. Other projections remain mounted and coherent through the shared repository
  and invalidation bus.
- **The listener owns its thread before kernel setup.** `InvalidationListener`
  now stops and joins in its own destructor. Failed `spawn_mount2`, setup unwind,
  explicit unmount and `MemoryMountHandle::drop` use this single cleanup owner;
  dropping a bare `JoinHandle` previously detached the subscriber on setup error.
  The existing bus, cache loop, Memory port and kernel session remain unchanged;
  only listener ownership changes, with no new runtime registry or cleanup service.
  Dynamic order is create owned listener -> attempt mount -> transfer to the mount
  handle on success, or stop/join on error. Unmount/Drop stop and join the listener
  before releasing the kernel session. Repeated unmount sees an empty listener.
  Adjacent test comments own the cause/effect table: real invalid-mount failures
  preserve a peer subscriber, 18 disposal/exit/queue combinations release the
  subscriber, a channel barrier requires exit rather than stop acknowledgement,
  and setup unwind retains cleanup. `MemoryInvalidationLifecycle` separately
  checks two projection owners, join-before-return, peer isolation and conditional
  completion, with five semantic mutants and four reachability witnesses. The
  production destructor's single generic cleanup action is also Kani-checked for
  every `u64` handle identity, present/absent ownership and repeated cleanup: stop
  precedes the exact once-only join callback, and return follows that callback.
  Real `JoinHandle` blocking remains qualified by the channel-barrier test and the
  Rust standard-library/runtime assumption. The existing Memory CAS model remains
  the write-conflict authority, not a listener lifecycle model. The TLA+ model is
  abstract, not universal Rust refinement: scheduler progress and terminating
  cache critical sections are assumptions. This does not solve abrupt process
  death, kernel unmount behavior, Provider replay cancellation or failed ownerless
  compensation retention.
- **Cross-host transport remains deferred.** `Invalidator` is the transport port, but
  only the in-process `LocalInvalidator` is implemented. A distributed transport must
  reuse that port and the same repository authority; it must not add another store or
  mount coordinator.

#### Live Provider ownership across cancellation

Local and Namespace now retain physical Memory operations in one shared private
`memory_mount_owner` implementation. Reused unchanged: the MemoryMounter/MemoryMount
ports, Memoryd Copy CAS and FUSE realization, marker/Session authorization, and
Host reconciliation acknowledgement. Modified: existing create, Ready replay,
adoption, Namespace attach and disposal borrow the same participant. Added: a
Provider-owned physical-root index containing the pending mount future, exact
requirement/path key, root identity and returned guards. It replaces temporary
guard vectors and Local/Namespace's all-or-leak compensation; it is not a durable
registry, new aggregate, lease, background cleanup service or source of permission.
Container staging owns a different physical incarnation and is not merged into it.

Dynamic order: register owner -> admit through existing marker/current proof ->
retain and poll the mount future -> retain returned guard -> revalidate -> publish
receipt. Cancellation drops only the caller's borrow. An admitted retry resumes
that same future, then compensates unpublished mounts before resetting an incomplete
root. Failed/denied cleanup preserves the complete selected set; another wrapper
can retry while the Provider lives. Empty, unreferenced owners alone are pruned.
Root replacement denies resumption and teardown; it does not authorize deleting
the foreign root. Namespace attachment locks this owner before checking the
destination, so competing callers cannot each publish a different Memory guard.

Exact confirmed FUSE requirements reuse their guard. Changed references retire the
old FUSE projection under the same admission before reacquisition. Confirmed Copy
participants retain Agent edits and original heads through Ready replay/adoption;
only ordinary disposal or the Host's existing successful reconciliation ACK retires
them. ACK rejects unfinished participants. Replay compensation never harvests a
confirmed Copy just because publication failed. Unfenced Copy adoption remains
zero-effect and rejects unfinished work rather than opportunistically cleaning it.

Host Resource replay uses the completed installed Memory pin, not the original
transition's starting manifest, to admit continuity. If Control loses the
acknowledgement of an empty-to-Memory transition, replay retains the existing
Memory bytes and original heads; it still executes the other aggregate-authored
Resource effects before publishing completion. A different installed Memory pin
rejects both a mutation command and an incoming self-equal command. Desired-only
requirements staging is not completed-install evidence. The existing lifecycle
and Resource locks serialize this decision, and the Provider remains the physical
participant owner; no second receipt or recovery registry is added. The live
manifest decision table and eight four-generation Linux scoped cancellation
chains exercise this composition with Copy Memory. Their bounded TLA+ companion
checks non-rewinding edits and successor identity, not FUSE, partial writes,
terminal writeback or actual Worker death.

Adjacent test comments own the decision tables: pending-before/after-guard cuts,
failed compensation, wrapper Drop, root replacement, concurrent cleanup, exact
FUSE reuse/reference change, both Provider entrypoints and Namespace attachment.
Real Memoryd Copy tests verify original bytes/heads, recovered CAS and no second
harvest after ACK. Host replay tests distinguish wrapper loss (reuse) from actual
Provider replacement (cold reacquisition). Its first/last mount-response table
checks twenty revoke/replace/expire/renew/cancel schedules: denial cannot authorize
compensation, and a fresh retry resumes the original cancelled mount future.
`MemoryParticipantOwnership` bounds one root/Copy-or-FUSE participant and two
callers; twelve semantic mutants and six witnesses cover its ownership/admission
cuts, including no-effect failure, unknown result and failed recovery. The
`MemoryMounter` port now returns one of `Mounted`, `NoEffect` or
`RecoveryRequired`; both live local ownership and container staging retain the
same operation identity until a retryable recovery succeeds. This removes the
former assumption that every ordinary error proved mounter-owned cleanup. It is
independent bounded cross-evidence, not a universal Rust refinement or an
unconditional liveness proof. The live Provider index has one production pruning
predicate shared by Local and Namespace: another wrapper, a currently borrowed
owner, a pending/unknown operation or any returned guard retains the owner; only
an observed empty unreferenced owner is removable. The direct Kani harness
exhausts those four independent facts. Async decision tables bind unknown recovery
and failed teardown to the same pending/guard fields and reject a mutation that
drops pending responsibility. Arc and Tokio behavior, a non-returning mounter,
process/Provider death, a dishonest adapter recovery result, hostile-host identity
races and real multi-platform/cloud credential qualification remain outside this
process-local closure. Actual kernel FUSE tests are separate from fake-guard
timing fixtures; cross-compilation is not platform execution.

#### Provider process death and cold FUSE adoption qualification

The strict G7 substrate gate now composes the existing Linux scoped owner-crash
fixture with the real SQLite repository and Memoryd FUSE mounter. Production
owners are unchanged: the filesystem marker admits adoption, the shared
`prepare_adoption_memory_replay` retains the Ready operation across remount,
and Memoryd clears the disconnected projection before mounting the same store.
No new recovery registry, daemon, Session lease or standard Worker FUSE policy
is introduced; the registered default Worker composition remains Copy-backed.

The adjacent F1–F4 test comments own the cause/effect table. An actual scoped
child writes and fsyncs before its Provider owner is killed with SIGKILL. Three
detached process-tree sockets and the recorded identity establish exit. Live
trees and unknown boot identity reject adoption without replacing the mount or
changing the independent SQLite head. Exact retry uses the original handle,
detaches the busy dead mount, and lets a fresh scoped child read those committed
bytes and commit a new head. The old open descriptor still fails while the new
path works; recycled Linux mount IDs are not a connection-identity oracle.
Terminal disposal unmounts and removes only the Session root, preserving the
external Store and sentinel. The case is explicitly selected by the existing
strict substrate runner, never silently satisfied by Copy or a missing FUSE SDK.

The existing `LocalProcessRecovery` model additionally checks exclusive Ready
ownership for resource recovery. Its combined suite kills 25 semantic mutants
and reaches ten witnesses, including post-crash resource recovery and rejection
with unknown identity. Resource effects remain abstract: kernel behavior,
partial/unflushed I/O, backend commit atomicity and Rust refinement are not
proved by TLC. This real Linux test closes Provider death plus durable FUSE
reacquisition, not the complete distributed Worker/Control restart, cross-host
invalidation, macOS/Windows execution or real credential qualification.

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
  implements `MemoryMounter` and owns the shared invalidation bus (D5).
- **The Host has no Memory write-back registry.** The memory-store family routes
  through the neutral provider contract. A `MemoryMount` guard owns the complete
  realization lifetime; FUSE writes through and copy-mode teardown reconciles its
  own captured snapshot. There is no `StagedResources.memory_mounts`, polling-route
  harvest, or `harvest_thread_memory` second writer.
- **Capability-gated fallback (no FUSE → copy/harvest).** FUSE needs `/dev/fuse`,
  absent on macOS, in CI, and in unprivileged containers. `fuse_available()` gates
  the realization: when true, mount the live write-through FUSE; when false, the same
  path-addressed store is materialized the old way — `materialize` copies every
  memory out to plain files at prepare while capturing `(path, id, sha)` heads.
  Teardown creates new paths, updates changed paths against the captured head, and
  deletes only captured paths through atomic `delete_if_match`. Concurrent durable
  heads win and are reported; paths created after materialization are never deleted
  as locally absent. Both realizations back the one durable `MemoryRepository`, so a store is
  portable across FUSE-capable and FUSE-less hosts.
- **Complete-scan prerequisite for copy write-back.** The one `copy::harvest`
  first enumerates and reads the whole quiescent projection through
  `awaken-sandbox-fs::read_tree_nofollow_with_limits`, reusing its descriptor walker
  on Linux/macOS and its explicitly quiescent-path adapter on Windows. Memory
  owns logical-path validation and Repository mutations, not a second walker.
  Missing roots, directory/read failures, links or Windows reparse points, special files,
  invalid names and over-limit content fail before Repository mutations. A
  readable empty root is distinct: explicit unlink may conditionally delete
  captured heads. Binary files within the size limit retain presence but are
  not published as text. File reads are bounded by the existing Memory content
  limit, and the total captured bytes cannot exceed the existing maximum heads
  per store multiplied by that content limit. The physical copy profile also
  bounds all entries (including empty directories/files) to 4,000 and relative
  depth to 128; ingress and recovered-file admission preflight the same profile,
  counting shared ancestors once and rejecting duplicate/file-ancestor paths
  in either order. These physical limits do not change logical
  Repository quotas. Successful preflight is not a
  multi-file transaction: a later CAS conflict can leave earlier independent
  files committed; exact retry and
  recovery retain those versions and the original bases. The caller must stop
  writers before scanning. A live copy mount retains its original root handle
  and rejects missing, file, or different-directory replacements before harvest;
  restoring that same directory permits retry without refreshing CAS bases.
  Windows rejects static reparse points and compares retained file identifiers
  via `same-file`, relying on stable filesystem IDs and quiescence rather than
  Unix descriptor-relative confinement. The live root handle is not persisted
  authority and does not prove process ownership after Worker death.
  The dependency uses 64-bit Windows file indices, not ReFS 128-bit IDs; this
  check is not a universal Windows-filesystem identity guarantee.
  This is not an atomic snapshot or
  confinement of a malicious TrustedHost process.
- **Portable copy-path admission.** The logical Repository keeps its existing
  POSIX path contract. Its physical copy adapter validates every component
  before materialization and recovery: backslashes, drive/ADS syntax, device
  names, trailing dots/spaces, control bytes and other nonportable components
  reject on every OS. Device-name rules reuse the sandbox directory-name owner
  in `awaken-sandbox-fs`; no second Memory path authority is introduced. Files
  and distinct logical parents use exclusive creation, so aliases such as
  `A/x.md` and `a/y.md` cannot silently merge directories. Admission reads the
  completed projection through the canonical bounded reader and requires exact
  paths and content hashes before returning the mount. Alias/round-trip errors
  can leave a partial unpublished copy; the mounter prepares a fresh projection
  on retry. Unicode/interior spaces are supported when the volume preserves
  their exact spelling; lossy name normalization fails closed. Filesystem
  semantics are not approximated by lowercasing logical identities.
- **Recovery owns no second plaintext projection.** The mounter validates
  persisted evidence and passes the complete captured file set directly to the
  same per-file reconciliation function used by live harvest. There is no
  recovery temp directory, rematerialization, second scan, or best-effort
  cleanup path. Cancellation therefore cannot leave that redundant plaintext
  copy behind, and recovery does not depend on TMPDIR permissions or the
  recovering host's case/normalization semantics. This does not erase the
  original Agent projection or protect bytes already in process memory.
- **Failure-window evidence.** `tests/writeback_faults.rs` exercises actual
  copy teardown against SQLite for create/update/delete, failure/cancellation
  before or after commit, and with/without a preceding independent commit
  (24 decision-table rows). Same-handle retry and durable reopen must preserve
  exact heads/history. Six additional rows pause an actual SQLite transaction
  inside an AFTER trigger, abort its caller while `spawn_blocking` continues,
  and retry before/after the late commit. An independent reader sees only old
  committed truth while paused; retry and reopened original evidence must add
  exactly one version. A subprocess with unusable TMPDIR/TMP/TEMP verifies
  byte-only recovery across success and cancellation on both sides of commit.
  `tests/copy_lifecycle.rs` covers permission-denied reads
  and enumeration, root substitution, unsafe names and metadata overflow;
  permission cases require an unprivileged Unix runner. The shared reader also
  tests an I/O error after partial bytes. Parent aliases and no-edit exact
  path/ID/history round trips use the executing volume's measured semantics.
  These do not prove disk power-loss durability, abrupt Worker-death process
  fencing, or execution on an OS other than the actual test runner.
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
| **P0** | `MemoryRepository` port + `PathAddressedMemoryStore` (inmem+fs): path-unique, CAS, monotonic version, stored timestamps, atomic rename-replace | Rust unit: create / update-CAS-conflict / rename-replace / delete / list |
| **P1** | `awaken-sandbox-memoryd`: port `fuse.rs` over `MemoryRepository`; conformance floor; real timestamps in `attr` | Ported unit tests: dirty-budget fail-closed, rename-keeps-open-fd, stale-fd-no-clobber |
| **P2** | Per-projection mount guards + one host-local invalidation bus | Unit: independent teardown, publish/subscribe; kernel: warmed peer cache observes write |
| **P2.5** | Concurrency tests | (1) mount A writes → warmed mount B invalidates and reads fresh; (2) concurrent write same file → one wins, one `EAGAIN`, no lost update; (3) create-create race → one wins; (4) tear down A while B remains mounted and readable |
| **P3** | Contract provider wiring: `LocalProvider` realizes `MemoryStore` → `Realization::Fuse` via the injected `MemoryMounter`; route the host memory-store family through the contract provider (retire copy-in/harvest for it) | Kernel-VFS integration test (gated on `/dev/fuse`) + TS e2e: agent writes memory files, readable across Runs and after restart |
| **P4** | sqlite/pg backends + repoint `memory_store_api.rs` at the new store | Existing managed-memory e2e green + durability e2e |
| **P5** | Bwrap namespace splice (mount propagation + `--bind`) | Bwrap integration test (gated); failure does not affect P3 |
| **(later)** | Distributed model: `Invalidator` (NATS/pg-notify) + version-validated cache | Two-host coherence test |

## Consequences

- **Net consistency improvement over the retired implementation.** The former
  Host copy-in/harvest path was last-writer-wins with full lost-update risk. The
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
  is the store-model upgrade (D1) and the concurrent invalidation wiring (D5), which
  are ordinary async-Rust and are unit-testable without a kernel.
- **Cost / dependency footprint.** Adds a `fuser` dependency (in the worker-tier
  `awaken-sandbox-memoryd`; boundary allowlist updated), needs `/dev/fuse` for the
  kernel path (all kernel-level tests gated; the copy path is the fallback where FUSE
  is unavailable, e.g. CI/macOS/bwrap), and introduces a per-`store_id` background
  mount whose lifecycle is tied to sandbox create/dispose.
- **Boundary preserved.** The sandbox providers depend on injected ports —
  `BlobSource` for file bytes and `MemoryMounter` for memory stores — never on `fuser`
  or the store directly. The FUSE/copy realizer (`MemoryStoreMounter`) lives in
  `awaken-sandbox-memoryd` (worker) and is the only crate that links `fuser`; the
  composition root injects it.
- **Delivered:** P0 (`MemoryRepository` port + CAS store, including conditional delete), P1 (the FUSE port, proven by a
  real kernel-VFS integration test), P2/P2.5 (per-projection lifecycle + invalidation concurrency),
  P3 (95% changed-code coverage), P4 (the durable path-addressed store backs the
  `/memories` HTTP endpoints, with a restart-durable TS e2e), and P5 minus the bwrap
  splice (see below). `awaken-sandbox-local` **and** `awaken-sandbox-memoryd` were
  moved to the **worker tier**; the providers resolve mount bytes through an injected
  `BlobSource` port and realize memory stores through an injected `MemoryMounter` port,
  linking no durable **commit** store (`worker → resources` is allowed: resources is
  the common foundation config defines and worker materializes; the G17 exclusion is
  the commit-log tier only).
- **Contract-provider realization — delivered (item 1).** `LocalProvider` (Workdir)
  and `NamespaceProvider` (bwrap) realize `MountSource::MemoryStore` through the
  `MemoryMounter` port (`awaken-sandbox-memoryd::MemoryStoreMounter`): **live FUSE**
  where `/dev/fuse` is present, else a **materialized copy harvested back on dispose**
  (D6). A provider with no mounter fails a memory mount loud. Proven end-to-end by an
  integration test that exercises the real FUSE path on a `/dev/fuse` host (seed →
  read-through → edit → write-through → durable) and a deterministic copy-harvest test
  on the bwrap tier. The host's `/memories` HTTP route was already cut over to the
  path-addressed store in P4.
- **Workdir unprivileged exposure — resolved (item 3), no privileged bind needed.**
  The Workdir tier runs in the host mount namespace, so the mounter FUSE-mounts each
  store **directly at the sandbox's resolved path** — no bind (`CAP_SYS_ADMIN`) and no
  jail-crossing symlink. Cross-sandbox coherence for the same store comes from the
  per-mount invalidation listener (item 4) rather than a single shared mount, so the
  D5 guarantee holds without the privileged splice the shared-mount model would need.
- **Distributed coherence — delivered (item 4).** `Invalidator` + `LocalInvalidator`
  (in-process broadcast) with an `InvalidatingMemoryRepository` write-side decorator and a
  FUSE-side listener that drops invalidated paths from the content cache; a
  NATS/pg-notify transport swaps in behind the same trait. Unit-tested (publish→drop,
  reads-don't-invalidate, matching-store-only).
- **Backends — delivered (item 5).** `SqliteMemoryRepository` (tested, restart-durable) and
  `PostgresMemoryRepository` (cross-node CAS via `SELECT … FOR UPDATE`) over one scoped-migration
  bundle.
- **Deferred — bwrap namespace splice (item 2).** Exposing a *live* FUSE mount inside
  the bwrap namespace still needs host mount propagation (`rshared`), an extra
  `--bind <fuse_dir>` in `bubblewrap_argv`, and `/dev/fuse` in the guest — an
  integration that `awaken-next` also left unimplemented and that cannot be faked
  without a real bwrap+kernel test. Until it lands, the bwrap tier uses the copy
  fallback above (a complete realization, not a stub). The distributed
  `Invalidator` **transport** (NATS/pg-notify) and a pull-based version oracle remain
  future work behind the delivered in-process seam.

## 2026-08-14 amendment: one Memory realizer and no Pod-local store authority

The authoritative realization is the injected `MemoryMounter` backed by the
configured `MemoryRepository`. Kubernetes copy mode stages one bounded snapshot
through the provider-owned `memory-projector` into a Pod-local volume and
reconciles that exact surviving tree through the same mounter on disposal or
Worker recovery. The projector owns no Repository client, database, credential
or durable Memory truth.

The standalone `awaken-sandbox memoryd` process that opens a separate SQLite
database under `--store-dir` is not a valid realization of this architecture. It
duplicates the configured Repository and can make a Pod-local database compete
with the authoritative store. The executable role, its dedicated image and its
sidecar-only tests are retired. The `awaken-sandbox-memoryd` crate name remains
for compatibility, but its authoritative production responsibility is the
injected in-process `MemoryStoreMounter`, including FUSE and copy/CAS behavior.

Worker loss while the Kubernetes Pod survives does not lose the projected tree:
the replacement adopts the same Sandbox and later reconciles it through the
existing mounter. Pod/node loss destroys unharvested `emptyDir` changes; the last
durable Repository version remains authoritative. A deployment requiring
write-through durability must provide it behind the existing `MemoryMounter` /
`MemoryRepository` ports with claim-scoped, short-lived authority. It must not
introduce another SQLite store, lifecycle aggregate or provider credential in a
sidecar.

## Amendment 2026-10-09: reconciled with ADR-0063

- D1's "the legacy `MemoryBlobStore` and the copy-in/harvest resource path are left in
  place" is no longer true. [ADR-0063](0063-resource-input-identity-configuration-pinning-and-lifecycle.md) D7 makes `MemoryRepository` the one Memory
  truth; `MemoryBlobStore` and `harvest_thread_memory` no longer exist in `crates/`. The
  copy fallback that remains reconciles per path through the same `MemoryRepository`
  (`crates/worker/awaken-sandbox-memoryd/src/mounter.rs:30`).
- D2's "`server` bucket": the crate is `crates/worker/awaken-sandbox-memoryd`
  (`context = "worker"` in its `Cargo.toml`), as the Consequences already say.
- The "Deferred — bwrap namespace splice (item 2)" bullet is stale: the namespace
  provider binds the live FUSE mount into bwrap
  (`crates/worker/awaken-sandbox-local/src/namespace.rs:407-412`) and a gated test proves
  it (`crates/worker/awaken-sandbox-local/tests/namespace_provider.rs:1114`). With every
  slice delivered, the status is Accepted.
