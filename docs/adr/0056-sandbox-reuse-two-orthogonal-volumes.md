# ADR-0056: Sandbox Reuse as Two Orthogonal Volumes — Cache-Volume Warmth Is Product-Plane-Owned, Isolation-Instance Reuse Is Worker-Plane-Owned; Keep Decisions Pure, Orchestration Separate

- Status: Accepted
- Date: 2026-07-15
- Implemented: 2026-07-22 — `CacheVolume` remains the product-owned opaque
  storage seam; `SessionEnvironmentProvider` is the production worker-plane
  owner for Workdir, namespace, Docker, Podman, and Kubernetes environments;
  `WarmContainerPool` supplies unused, shape-fenced container capacity; durable
  environment bindings support restart adoption and competing-adopter fencing;
  Docker/Podman crash GC and Kubernetes owner references close substrate-specific
  reclamation. Native, ACP, and delegated child attempts use the same
  Session-owned environment. The unused generic `SandboxManager` was removed:
  its private in-memory registry duplicated the Session Environment owner without
  participating in any production call path.
- Builds on: the `pc::Sandbox`/`SandboxProvider`/`SandboxHandle` contract and the
  never-downgrade `select_provider` / fail-closed `prepare_environment` gates
  (`awaken-provisioning-contract::sandbox`); the already-written-but-uncalled
  reuse decision functions `reconcile_adoption` / `LeaseLiveness`
  (`awaken-provisioning-contract::lease`); the deprecated cooperating-tool
  `Environment`/`RootedTool` model pending a `pc::Sandbox` rebase
  (`awaken-sandbox-local::lib`); the brain–hand relay and dynamic placement
  registry (ADR-0044/0045/0046); the two existing mount kinds — content-hashed
  read-only `ResourceMount` and harvested read-write memory mount (ADR-0038).
- Reference: the operational shape of an isolation-instance pool is validated by
  DeerFlow's AIO warm-container pool (deterministic id, release-awaits, idle reaper,
  orphan reconciliation, readiness-probe-before-adopt) — a reference architecture,
  not a code dependency.

## Context

Awaken's sandbox layer has a complete **contract + decision** half and an absent
**live-orchestration** half. The contracts exist (`Sandbox::{spawn,attach,process,
renew_lease,dispose,status,artifacts}`, `SandboxProvider::{capabilities,
probe_ready,create,adopt}`), the never-downgrade selection and fail-closed
preparation exist, and the reuse *decisions* are written as pure, tested
functions (`reconcile_adoption(live, referenced) -> AdoptionPlan`,
`LeaseLiveness::{Healthy,DueForRenewal,Dead}`). **None of the
decision functions has a caller.** There is no warm pool, no lease-renewal loop,
no idle reaper. Separately, the resident host still provisions per session
through the *deprecated* `Environment` cooperating-tool model, one fresh
environment per session, no reuse.

The two reuses have well-understood reference shapes:

- **Warm directory reuse** is the classic build-cache / checkout pattern: a stable
  path keyed per work unit, a single-active lock (an atomic `create_new` lock
  file), keep-warm-on-release (drop removes only the lock, the directory stays),
  and mtime-based idle GC — with the realized path treated as **opaque worker
  state**. This is application/product-domain machinery (it assumes "a code
  project worth keeping warm"), so it lives in the product plane, not the runtime.
- **DeerFlow** implements the operational shape of an isolation-instance pool for
  Docker/k3s containers: deterministic id `sha256(user:thread)`, release-awaits /
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
| OS-isolated spawn environment (`pc::Sandbox`) | **Sandbox instance** (pooled only when expensive) | an isolation boundary | per run, or awaiting warm for Container tier | **awaken worker plane** |
| Execution endpoint that runs tools/spawns | **Hand** (`HandRegistry`, ≈ today's `WorkerRegistry`) | a stateless executor + its capabilities | fleet-lived | awaken worker/server plane |

The run-time relationship is a mount, not an inheritance:

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
  single-active lock (an atomic `create_new` lock file). A concurrent claim for
  the same key gets a fresh throwaway volume, never a shared writer. The invariant
  "a writable base with `max_concurrency > 1` is forbidden (lost-update)" is
  precisely the RWO-vs-RWX rule; concurrency means N independent volumes or an
  explicit RWX volume, never a shared RWO.
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
- The **application / product plane** owns the Cache-Volume lifecycle. Awaken's own
  hosted product, if it wants directory warmth, owns a Cache-Volume provisioner in
  its **server/product plane** (`awaken-runtime-host` or above), never in the
  runtime or the neutral worker contract. Any external consumer that wants directory
  warmth likewise owns its own provisioner and reaches awaken only through the mount
  seam below — awaken exposes the substrate, never the warmth policy.

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
isolation is awaken's declared job (the provisioning-contract header). It has one
owner per lifecycle instead of a generic manager beside the production path:

- `SessionEnvironmentProvider` creates/adopts the one active environment retained
  by a Session slot and disposes it at the terminal Session boundary.
- `WarmContainerPool` owns only never-used, mount-less, exact-shape container
  capacity. A container leaves the pool permanently when bound to a Session; used
  containers are never returned.
- `SandboxReaper` owns only cross-restart garbage collection for prior runtime
  owners. It never competes with the current process's Session or warm capacity.
- `awaken-provisioning-contract::lease` remains the pure decision vocabulary. It
  does not gain an I/O orchestrator merely to create a nominal caller.

Workdir and Namespace remain unpooled. The Container tier probes every eagerly
created candidate for `Ready`, exposes capacity through the separate
`ContainerEnvironmentCapacity` lifecycle, and drains unused capacity after Worker
claim drain and before deregistration.

### 4.1 Three-stage warmup is one pipeline with three different owners.

| Stage | Authoritative owner | Key / boundary | Production trigger | Failure result |
|---|---|---|---|---|
| Immutable Environment image | `awaken-environment-image-build` | Environment build digest | Environment registration enqueues one build demand; execution readiness waits for its receipt | Environment remains unready; no alternate image builder |
| Never-used container environment | `WarmContainerPool` through `ContainerEnvironmentCapacity` | normalized mount-less `SandboxSpec` shape | Worker startup, before the first Ready heartbeat; adaptive replenish after a hit | log and retain cold create; selected isolation never downgrades |
| Cache Volume bytes/location | product-side `CacheVolumePrewarmer` in `awaken-runtime-host` with an injected `CacheVolumeInitializer` | caller `(key, CacheVolumeLocation)`; `HostPath` and Kubernetes PVC are equally valid identities | explicit eager call or automatically before Session provider creation | Session creation fails; failed preparation is removed and may retry |

Static dependency direction:

```text
Environment registration -> image-build coordinator -> immutable image receipt
Worker startup -> SessionEnvironmentProvider -> WarmContainerPool capacity
Session creation -> product CacheVolumePrewarmer -> opaque CacheVolume mount -> provider
```

Dynamic startup/use/shutdown sequence:

```text
register Worker as Starting
  -> select provider + capacity from the same backend
  -> prewarm canonical empty shape to configured target
  -> publish Ready -> claim Session
  -> prepare each CacheVolume (single-flight) -> create/adopt Session environment
  -> drain claims/in-flight work -> shutdown unused capacity -> deregister
```

Environment configuration changes use the same path instead of a fourth refresh
mechanism:

```text
commit new Environment revision
  -> enqueue/build its immutable image digest
  -> publish the new exact EnvironmentSnapshot as current warmup demand
  -> Worker compares the desired shape with retained unused capacity
  -> discard obsolete unused capacity, then warm the new exact shape
  -> leave existing Sessions pinned to their frozen revision and environment
```

A CacheVolume key or location change likewise creates a new preparation identity;
it never mutates or aliases the previous prepared volume. Kubernetes PVC and
host-path locations pass through the same `CacheVolumeLocation` identity and the
selected backend-specific initializer.

The three stages deliberately do not share a "warm manager": their keys,
consistency boundaries, failure meaning, and owners are different. They share only
the ordered lifecycle above.

### 5. Isolation floor is a policy input, not a hardcoded default — one mechanism, two trust models.

The only real difference between the local-dev trust model and the multi-tenant
hosting trust model is the *floor*, so make the floor a parameter:

```
IsolationPolicy { require: IsolationClass, prefer: IsolationClass, on_unmet: FailClosed | DegradeWithConsent }
```

- Local single-user (dev / CI): `require = Workdir`, soft default.
- Multi-tenant hosting: `require = Namespace|Container`, `on_unmet = FailClosed`,
  preserving the never-downgrade rule.
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

1. The Cache-Volume seam, Session-owned environment, exact-shape Container warm
   capacity, restart reaper, and three-stage wiring are implemented and scenario
   tested.
2. `IsolationPolicy::DegradeWithConsent`, cross-node CacheVolume scheduling, and
   idle cache GC remain scenario-gated. None is implied by warmup.
3. The two-axis separation is retained: storage warmth and isolation capacity do
   not acquire a shared manager or synchronized duplicate state.

### Tension B — pure decision core vs orchestration (simple design: decision/IO split)

The reuse *judgements* (`reconcile_adoption`, `LeaseLiveness`) and
the reuse *orchestration* (the reaper loop, the pool, the Cache-Volume provisioner)
are different kinds of thing, and mixing them is why `attach`/`renew_lease`/pooling
are half-built. Keep the two cleanly separated.

**Resolution — decisions are a pure kernel; orchestration is the impure shell.**

- **The pure decision kernel:** the *decision* functions and value types only —
  `reconcile_adoption`, `LeaseLiveness`, `LeaseGrant`,
  `SandboxHandle`, `IsolationClass`. These are nearly immutable, carry no DTOs, and
  do no I/O, so they are exhaustively testable with a fake clock and plain values.
  They live in a thin awaken crate (`awaken-provisioning-contract` today; a
  dedicated `awaken-provisioning-kernel` if it needs to be depended on without the
  rest of the contract) and are **self-contained to awaken** — no external product
  is a design input.
- **The impure orchestration:** Session slots, the never-used container pool, the
  cross-restart reaper, and the product Cache-Volume initializer. Each has a
  bounded lifecycle; no generic `SandboxManager` mirrors their state.
- The boundary is mechanically enforced (guardrail G-K): the kernel crate may
  export pure functions and value types only — no `async`, no I/O, no product
  types — so orchestration can never leak into the decision core.

The rule in one line: **keep the judgement pure, the machinery separate.**

## Guardrails (fitness rules)

- **G-Own (Cache-Volume ownership).** No crate in `awaken-runtime`,
  `awaken-provisioning-contract`, or the neutral worker `*sandbox*` crates
  implements a Cache-Volume provisioner, warm-directory reuse, or its GC. The
  worker layer only mounts a caller-supplied opaque volume path. Enforcer:
  `check_crate_boundaries.py` rule forbidding `mtime`/idle-GC/warm-directory
  symbols in those crates.
- **G-K (decision-kernel purity).** The reuse-decision kernel exports only pure
  functions and `Copy`/plain value types — no `async fn`, no `std::io`, no
  `tokio`, no product DTO — so orchestration cannot leak into the decision core.
  Enforcer: a crate-lint that rejects `async`/IO deps in that crate's `Cargo.toml`
  and a symbol check.
- **G-Y (YAGNI scenario gate).** New reuse beyond never-used exact-shape Container
  capacity and `IsolationPolicy::DegradeWithConsent` may not merge without an
  accompanying failing driving-scenario test.
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
  (`reconcile_adoption`/`LeaseLiveness`); Session/pool/reaper orchestration owns
  I/O without copying those decisions into another state registry.

## Consequences

**Positive.**

- The split matches a universally understood model (volume ⊥ container), so
  intent is legible and the container tier can delegate to native volume
  primitives (Kubernetes PVC + StorageClass, Docker named volumes) instead of
  reinventing storage.
- Ownership is unambiguous and DDD-clean: runtime core owns nothing here; the
  worker plane owns isolation + the mount seam; the product plane owns warmth. The
  dependency graph stays acyclic and one-way.
- Awaken stops carrying application domain knowledge ("this is a code project"):
  the worker contract stays a neutral isolation substrate, and warmth is a
  product-plane concern reached only through the opaque mount seam.
- The unused generic lifecycle registry is gone; production call paths expose one
  active Session owner, one unused-capacity owner, and one crash-GC owner.

**Negative / accepted costs.**

- Warm capacity consumes resources before demand and supports only mount-less
  exact shapes; mounted or different-shape Sessions retain the cold path.
- "Volume" enters the ubiquitous language and must be policed against overloading
  `Mount` (guardrail G-Lang).

## Implemented vertical slice

1. `SandboxSpec::CacheVolume` is the opaque storage seam; local namespace and
   container adapters mount it without harvesting.
2. `CacheVolumePrewarmer` single-flights the caller key, retries failures, and is
   invoked by the one Session environment creation helper.
3. `WarmContainerPool` eagerly reaches a ready target, verifies readiness, never
   pools mounted specs, and fences in-flight creates during shutdown.
4. Worker lifecycle warms before Ready and drains unused capacity after claims.
5. Environment image registration/build/readiness continues through the one
   `awaken-environment-image-build` demand path.
