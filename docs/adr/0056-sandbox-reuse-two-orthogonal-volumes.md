# ADR-0056: Sandbox Reuse as Two Orthogonal Volumes — Cache-Volume Warmth Is Application-Owned, Isolation-Instance Reuse Is Runtime-Owned; Share Decisions, Not Orchestration

- Status: Proposed
- Date: 2026-07-15
- Builds on: the `pc::Sandbox`/`SandboxProvider`/`SandboxHandle` port and the
  never-downgrade `select_provider` / fail-closed `prepare_environment` gates
  (`awaken-provisioning-contract::sandbox`); the already-written-but-uncalled
  reuse decision functions `reconcile_adoption` / `LeaseLiveness` / `capped_expiry`
  (`awaken-provisioning-contract::lease`); the deprecated cooperating-tool
  `Environment`/`RootedTool` model pending a `pc::Sandbox` rebase
  (`awaken-sandbox-local::lib`); the brain–hand relay and dynamic placement
  registry (ADR-0044/0045/0046); the two existing mount kinds — content-hashed
  read-only `ResourceMount` and harvested read-write memory mount (ADR-0038).
- Reference: the sibling product `~/Codes/oversight-next` already implements the
  application half of this decision correctly (work-area lock + mtime GC in its
  own `oversight-provisioner`, zero coupling to awaken provisioning); the
  operational shape is validated by DeerFlow's AIO warm-container pool.

## Context

Awaken's sandbox layer has a complete **contract + decision** half and an absent
**live-orchestration** half. The ports exist (`Sandbox::{spawn,attach,process,
renew_lease,dispose,status,artifacts}`, `SandboxProvider::{capabilities,
probe_ready,create,adopt}`), the never-downgrade selection and fail-closed
preparation exist, and the reuse *decisions* are written as pure, tested
functions (`reconcile_adoption(live, referenced) -> AdoptionPlan`,
`LeaseLiveness::{Healthy,DueForRenewal,Dead}`, `capped_expiry`). **None of the
decision functions has a caller.** There is no warm pool, no lease-renewal loop,
no idle reaper. Separately, the resident host still provisions per session
through the *deprecated* `Environment` cooperating-tool model, one fresh
environment per session, no reuse.

Two neighbours have already solved the parts we lack:

- **oversight-next** implements durable *directory* reuse end to end: a stable
  path keyed per work unit, a single-active lock (`create_new` atomic lock file),
  keep-warm-on-release (drop removes only the lock, the directory stays), and
  mtime-based idle GC — all in its own `oversight-provisioner`, with the realized
  path treated as opaque worker state, and **no dependency on awaken's
  provisioning** (its ADR-0012 keeps `oversight-awaken-binding` the only awaken
  seam). It also *models but does not implement* a warm-instance pool
  (`EnvironmentCatalogEntry.max_concurrency` / `persistent_paths`).
- **DeerFlow** implements the operational shape of an isolation-instance pool for
  Docker/k3s containers: deterministic id `sha256(user:thread)`, release-parks /
  destroy-stops, an idle reaper, cross-process discovery, orphan reconciliation,
  and readiness-probe-before-adopt (dead entries dropped; a failed health check
  is treated as *unknown*, not dead).

The load-bearing realization is that "reuse a warm sandbox" is really **two
independent reuses at two lifecycles**, and conflating them into one `pc::Sandbox`
object is why `attach`/`renew_lease`/pooling are half-built. Separating them is
the classic **volume ⊥ container** split — storage lifecycle is not compute
lifecycle. Naming them in volume terms makes the ownership boundary, the access
semantics, and the cross-node decision all legible against a well-known mental
model, and it settles a boundary question: **who owns "warmth"?**

## Decision

### 1. Model reuse as two orthogonal volumes plus a placement endpoint.

Three concepts, three lifecycles, three owners. The middle column is the volume
vocabulary this ADR adopts repo-wide.

| Concept | Volume-vocabulary name | What is reused | Lifecycle | Owner |
|---|---|---|---|---|
| Warm directory (checkout, build cache, `node_modules`) | **Cache Volume** | persistent bytes on disk | outlives many runs | **application / product plane** |
| OS-isolated spawn environment (`pc::Sandbox`) | **Sandbox instance** (pooled only when expensive) | an isolation boundary | per run, or parked warm for Container tier | **awaken worker plane** |
| Execution endpoint that runs tools/spawns | **Hand** (`HandRegistry`, ≈ today's `WorkerRegistry`) | a stateless executor + its capabilities | fleet-lived | awaken worker/server plane |

The composition at run time is a mount, not an inheritance:

```
Hand (chosen by placement, HandRegistry)
  └─ Sandbox instance (isolation, from SandboxPool or freshly created)
       └─ mounts ── Cache Volume (warm bytes, provisioned by the application)
```

A **Cache Volume is not a Hand and not a Sandbox instance.** It is storage. A
Hand is by definition stateless (ADR-0044 G33: the hand links no store/model/
commit); therefore warmth *cannot* live in the hand — it must live in a volume.
This is not a naming nicety; it is the reason the split is correct.

### 2. Cache Volume is a ReadWriteOnce, Retain-reuse, node-local *cache* volume — and it is application-owned, not runtime-owned.

The Cache Volume carries exactly the semantics its volume name implies, and they
are load-bearing constraints, not documentation:

- **Access mode ReadWriteOnce (RWO).** One active writer at a time, enforced by a
  single-active lock (oversight's `create_new` lock file is the reference). A
  concurrent claim for the same key gets a fresh throwaway volume, never a shared
  writer. The oversight invariant "writable base + `max_concurrency > 1` is
  forbidden (lost-update)" is precisely the RWO-vs-RWX rule; concurrency means N
  independent volumes or an explicit RWX volume, never a shared RWO.
- **Reclaim policy Retain-then-reuse.** Release keeps the bytes; only an idle
  horizon (mtime GC, optionally lease-cross-checked) reclaims them.
- **Cache, not durable data.** Losing a Cache Volume costs a cold rebuild, never
  data loss. Nothing whose only copy matters may live here; the reaper may delete
  it at any idle moment.
- **Node-affine by default.** The warm bytes are on one node's disk. See §6 for
  the cross-node fork this forces.

**Ownership ruling.** The Cache Volume — its key policy (what to reuse by), what
subpaths persist, its size/reclaim thresholds — is **application/product domain
knowledge**, because it assumes the workload is "a code project with a checkout
and a build cache worth keeping warm." A general agent runtime must not assume
that. Therefore:

- The **runtime core** (`awaken-runtime`) owns none of it and never learns of it
  (placement- and substrate-agnostic, G2).
- The **awaken worker plane** does **not** implement a Cache-Volume provisioner.
  It exposes a **seam**: `SandboxSpec` accepts a caller-supplied opaque
  persistent volume path (the existing `IsolatedRoot` / `AWAKEN_PROJECT_DIR`
  mechanism generalized), mounts it, and treats its warmth/GC as **opaque —
  awaken neither keeps it warm nor reclaims it.**
- The **application** owns the Cache-Volume lifecycle. oversight already does
  this in `oversight-provisioner` and must keep owning it; awaken's own hosted
  product, if it wants directory warmth, owns a Cache-Volume provisioner in its
  **server/product plane** (`awaken-runtime-host` or above), never in the runtime
  or the neutral worker contract.

So: **awaken does not build a WorkAreaPool / VolumePool.** That was the wrong
instinct. Awaken builds the substrate a volume mounts into, and stops.

### 3. Cache Volume is a third, named mount kind — reconcile the ubiquitous language now.

Introducing "volume" must not silently overload the existing `Mount` vocabulary.
The three mount kinds are distinct and mutually exclusive; the ADR fixes the
language so "volume" never drifts into a synonym for "mount":

| Mount kind | Direction | Truth authority | Reuse model |
|---|---|---|---|
| `ResourceMount` (ADR-0038) | read-only, content-hashed | the content hash | immutable, shared |
| memory mount (ADR-0038) | read-write | the memory store (harvested back) | value carried in/out |
| **Cache Volume (this ADR)** | read-write | **none — disposable cache** | **reused in place, node-local, never harvested** |

A Cache Volume is the only mount whose bytes have no authority and are never
carried back; that is exactly what makes it a *cache*.

### 4. Awaken owns Sandbox-instance reuse: wire the decision functions, pool only the expensive tier.

The awaken worker plane owns everything about the **isolation** lifecycle, because
isolation is awaken's declared job (the provisioning-contract header). A
`SandboxManager` in the worker plane:

- **Wires the existing pure decisions** — the reap/renew loop *calls*
  `reconcile_adoption` for orphan/idle decisions and `LeaseLiveness` for
  renew-vs-reap, with `capped_expiry` aligning any injected secret's TTL to the
  lease. The manager contributes control flow only; every judgement stays in the
  already-tested pure functions.
- **Pools only when creation is expensive.** Workdir and Namespace tiers create
  in milliseconds and are **not pooled** — they are created per run and disposed.
  Only the **Container tier** gets a warm pool (release-parks / dispose-stops),
  mirroring DeerFlow's AIO container pool. This keeps the pool where it earns its
  cost and nowhere else.
- **Adopts operational hardening from DeerFlow**: deterministic instance id,
  readiness-probe-before-adopt (dead → drop and recreate; failed health check →
  *unknown*, not dead), startup orphan reconciliation via platform labels /
  Kubernetes `ownerReference`, and a graceful `shutdown()` drain.
- **Completes the ports it needs**: `adopt(handle)` + `process(pid)` reattach on
  the Container tier so a host restart reconnects a still-running instance; local
  tiers keep honest `Unsupported` (a local sandbox dies with its owner) and say so
  in `capabilities()`.

### 5. Isolation floor is a policy input, not a hardcoded default — one mechanism, two trust models.

The only real difference between the local-dev trust model and the multi-tenant
hosting trust model is the *floor*, so make the floor a parameter:

```
IsolationPolicy { require: IsolationClass, prefer: IsolationClass, on_unmet: FailClosed | DegradeWithConsent }
```

- Local single-user (oversight-shaped): `require = Workdir`, soft default.
- Multi-tenant hosting (awaken-shaped): `require = Namespace|Container`,
  `on_unmet = FailClosed`, preserving the never-downgrade rule.
- `DegradeWithConsent` never degrades silently: it emits a structured audit
  event, a metric, and a run-level `isolation_degraded` marker. Degradation
  becomes *representable and logged*, never invisible.

### 6. Name the cross-node fork the volume model exposes; do not hide it.

Because a Cache Volume is node-affine, cross-node reuse is a real decision, not an
emergent property of adding a pool:

- **Node-affine (local-PV / hostPath shape).** Warm bytes stay on one node;
  reusing them means the placement layer (`HandRegistry`) prefers the node that
  holds the volume. This is the default and the cheap path.
- **Networked RWX (portable volume shape).** Warm bytes on a shared/networked
  volume, mountable anywhere, at the cost of network storage latency.

The Cache Volume's key and the Hand placement policy must agree: **placement
prefers the hand co-located with a live Cache Volume for that key.** This is an
input to `HandRegistry` scheduling, recorded here so it is not rediscovered as a
performance bug later.

## Handling the two design tensions

This decision is only DDD- and simple-design-sound if two tensions are managed
explicitly. They are part of the decision, not an afterthought.

### Tension A — YAGNI vs completeness (simple design, rule 4: fewest elements)

The full shape (Container pool, `DegradeWithConsent` audit path, `process`
reattach) serves scenarios that are **not yet real** in-repo (container hosting is
daemon-gated and unwired; multi-tenant degradation is meaningless for a
single-user host). Building all of it now is gold-plating.

**Resolution — scenario-gated, slice-first delivery.**

1. **Ship the minimal vertical slice first** (§ First vertical slice): the
   Cache-Volume *seam* + `SandboxManager` over the Workdir tier + host
   `session_artifacts` on `pc::Sandbox`. Zero new isolation risk, proves the
   split end to end.
2. **Every deferred element requires a driving scenario test before it may
   merge.** `SandboxPool` (Container reuse) and `IsolationPolicy::
   DegradeWithConsent` land only when a failing test for real container hosting /
   real multi-tenant degradation exists. This is enforced (see guardrail G-Y).
3. The two-axis *separation* itself is not deferred and not gold-plating — it
   removes an existing conflation that already produces the `attach`/`renew_lease`
   gaps. Splitting genuinely different lifecycles is simplification, not addition.

### Tension B — DRY vs bounded-context autonomy (DDD integration mode)

The instinct to "extract shared reuse primitives so awaken and oversight don't
each build them" is a **Shared Kernel** — DDD's most coupling-heavy integration
mode, demanding co-versioned lockstep evolution. Across two bounded contexts,
duplication is frequently healthier than that coupling (Separate Ways).

**Resolution — share decisions, not orchestration.**

- **Shared (a thin, pure, product-free kernel):** the *decision* functions and
  value types only — `reconcile_adoption`, `LeaseLiveness`, `LeaseGrant`,
  `capped_expiry`, `SandboxHandle`, `IsolationClass`. These are nearly immutable,
  carry no DTOs, and do no I/O. Awaken **owns** this kernel; oversight may depend
  on it **one-way** (consistent with oversight ADR-0012: awaken is upstream).
  Awaken must **never** depend on oversight.
- **Not shared (each context owns its own):** the pool *orchestration*, the
  Cache-Volume provisioner, the lock/GC loop. oversight keeps its working
  `work_area_lock`/`work_area_gc`; awaken builds its own `SandboxManager`. A small
  amount of parallel orchestration is accepted deliberately, because it buys each
  context the freedom to evolve its lifecycle independently.
- The boundary is mechanically enforced (guardrail G-K): the shared crate may
  export pure functions and value types only — no `async`, no I/O, no product
  types — so it cannot grow into a de-facto shared runtime.

The rule in one line: **share the judgement, not the machinery.**

## Guardrails (fitness rules)

- **G-Own (Cache-Volume ownership).** No crate in `awaken-runtime`,
  `awaken-provisioning-contract`, or the neutral worker `*sandbox*` crates
  implements a Cache-Volume provisioner, warm-directory reuse, or its GC. The
  worker layer only mounts a caller-supplied opaque volume path. Enforcer:
  `check_crate_boundaries.py` rule forbidding `mtime`/idle-GC/warm-directory
  symbols in those crates.
- **G-Dir (dependency direction).** `awaken-*` must not import any `oversight-*`
  crate. oversight depends on awaken only through the shared pure kernel and
  `oversight-awaken-binding`. Enforcer: boundary check + `deny.toml`.
- **G-K (shared kernel purity).** The shared reuse-decision crate exports only
  pure functions and `Copy`/plain value types — no `async fn`, no `std::io`, no
  `tokio`, no product DTO. Enforcer: a crate-lint that rejects `async`/IO deps in
  that crate's `Cargo.toml` and a symbol check.
- **G-Y (YAGNI scenario gate).** `SandboxPool` (Container reuse) and
  `IsolationPolicy::DegradeWithConsent` may not merge without an accompanying
  failing driving-scenario test (container hosting / multi-tenant degradation).
  Enforcer: CI checks the PR touches a scenario test under `crates/bin/
  awaken-scenario-host` when it touches those symbols.
- **G-Lang (mount vocabulary).** "Cache Volume" is the only name for the RW,
  node-local, non-harvested cache mount; it is never called a `ResourceMount` or
  a memory mount, and "volume" is never used as a synonym for `Mount`. Enforcer:
  wiki/doc lint mirroring `check-wiki-no-invariant-copy`.
- **G-Down (never-downgrade preserved).** `select_provider` keeps its
  never-downgrade contract; the only path to a weaker tier than `require` is
  `IsolationPolicy::DegradeWithConsent`, which must emit the audit event + metric
  + run marker. Enforcer: existing `select_provider` test + a degradation-emits-
  audit test.
- **G-Pure (decision/IO split).** Reuse judgements stay in pure functions
  (`reconcile_adoption`/`LeaseLiveness`); `SandboxManager` contributes control
  flow only. Enforcer: the pure functions carry no `async`/provider deps (already
  true); a manager test drives them with a fake clock + fake provider.

## Consequences

**Positive.**

- The split matches a universally understood model (volume ⊥ container), so
  intent is legible and the container tier can delegate to native volume
  primitives (Kubernetes PVC + StorageClass, Docker named volumes) instead of
  reinventing storage.
- Ownership is unambiguous and DDD-clean: runtime core owns nothing here; the
  worker plane owns isolation + the mount seam; the application owns warmth. The
  dependency graph stays acyclic and one-way.
- Awaken stops carrying application domain knowledge ("this is a code project");
  oversight stops being tempted to reach into awaken provisioning; neither
  rebuilds the other's working half.
- The uncalled decision functions finally get a caller, closing the most glaring
  gap (`reconcile_adoption` with no orchestrator) without re-deriving any
  judgement.

**Negative / accepted costs.**

- A deliberate, bounded amount of parallel orchestration between oversight and
  awaken (Tension B's price for context autonomy).
- The Container tier's pool and reattach are real work deferred behind scenario
  gates; until then, container hosting on a bare host degrades to the Workdir
  floor unless a daemon is present (honest, `probe_ready`-gated, never silent).
- "Volume" enters the ubiquitous language and must be policed against overloading
  `Mount` (guardrail G-Lang).

## First vertical slice

The smallest end-to-end proof of the split, zero new isolation risk:

1. Generalize `SandboxSpec` to accept a caller-supplied **opaque Cache-Volume
   path** (extend the existing `IsolatedRoot`/`AWAKEN_PROJECT_DIR` mechanism);
   awaken mounts it and treats warmth as opaque.
2. Introduce `SandboxManager` in the worker plane over the **Workdir tier only**,
   with a reaper loop that *calls* `reconcile_adoption`/`LeaseLiveness` (fake
   clock + `LocalProvider` in tests).
3. Migrate host `session_artifacts` from the deprecated `Environment` to
   `pc::Sandbox::artifacts()`, leaving the rest of the legacy path under
   `#[cfg(test)]` until later slices.
4. Provide the shared pure-kernel crate (decision fns + `SandboxHandle`) and point
   the manager at it; add guardrails G-Own, G-Dir, G-K.

No Container pool, no `DegradeWithConsent`, no reattach in this slice — those
arrive only with their driving scenarios (G-Y).
