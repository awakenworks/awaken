# ADR-0059: Neutral Core, Leaf Adapters — the Evolution Invariant, Its Fitness Functions, and the God-Hub Dissolution Plan

- Status: Proposed
- Date: 2026-07-18
- Builds on: the plane-aligned single workspace and inward dependency rule
  (`Cargo.toml` bucket layout, `scripts/ci/check_crate_boundaries.py`,
  `deny.toml` wrappers); the `X-contract` extraction convention that factored
  `awaken-run-ingress-contract` out of its host (ADR-0039 slice 2.1); the unified
  agent-configuration snapshot as the config→execution data seam (ADR-0057); the one
  neutral event vocabulary + `Transcoder` projection seam (ADR-0058); tenancy as an
  edge aspect with one opaque `ScopeId` (ADR-0051).

## Context

The workspace already declares an inward dependency rule ("dependencies point inward
toward contract/kernel"). Three drifts had accumulated against it, all instances of one
disease — **a neutral concern living in the wrong crate, so the rest of the system
reverse-depends on an adapter or a hub**:

1. **Neutral ports lived inside a protocol adapter.** `awaken-protocol-managed` defined
   `SessionRuntime`, `WorkQueue`, `AgentConfigSource`, `McpProbe`, and the session vocab.
   Seven crates — including the neutral service layer and the control plane — reverse-
   depended on a *protocol adapter* to reach them. A protocol leaf had become the highest
   fan-in node: the stable-dependencies principle run exactly backwards.

2. **A contract leaf carried backends.** `awaken-session-contract` shipped the in-memory
   `WorkQueue` / `EnvRegistry` / session-repository implementations (and a lease
   algorithm) beside the ports, so "contract" meant two contradictory things across the
   tree — pure port (agent/resource/provisioning contracts) vs. port + backend (session).

3. **A protocol wire type sat on host state.** `SharedHost` held
   `Arc<dyn awaken_protocol_a2a::Transport>` for remote-agent delegation — the neutral
   substrate naming a concrete protocol.

Underneath these: `awaken-runtime-host` is a **god-hub** — the protocol-neutral session
substrate accreted roughly six bounded contexts (dispatch/durable, sandbox realization,
ACP driving, per-plane HTTP routers, MCP) and ~36 first-party dependencies, and is
depended on by control, server, and worker planes. It is the single largest structural
coupling in the repo.

The question this ADR settles is not "which crates move where" case by case, but **what
invariant keeps the layout correct as the system evolves, and how is that invariant made
mechanically enforceable** — so that adding a protocol, a backend, a topology, or an
agent capability stays a *leaf* change instead of a hub edit.

## Decision

### 1. The evolution invariant

> **The neutral core never learns a protocol name, a backend name, or a topology name.
> Every such variation is absorbed in a leaf adapter.**

Stated as a single acceptance test for any change: *is it a leaf-add, and can a newcomer
place the code correctly on the first try?* If a change edits a "hub" that already knows
everything, or scatters across crates, the grain is wrong. In DDD terms the core is a
shared kernel that stays conformist to external SDK shapes via an anti-corruption layer;
the core's ubiquitous language never leaks an external vocabulary. In simple-design terms
the core is closed to modification and open to extension — the variation points are all at
the outermost layer.

### 2. Placement criteria (four admission tests, in priority order)

1. **Shared across planes → `contract/`.** Two-plus planes need the type/port → it is
   vocabulary; it lives in a contract leaf with **no IO, no runtime, no backend**.
2. **Survives wire-erasure → neutral.** Erase the wire format and logic remains → it is a
   neutral implementation: protocol-and-backend-free logic in `runtime/` (the kernel), or
   port-composing service logic in the host layer.
3. **A replaceable port impl → `stores/` / `resources/`.** It is *one* implementation of a
   port, swappable without changing system semantics → a backend, beside its siblings,
   under the port's conformance suite.
4. **Deployment role → `server/` / `worker/` / `control/`.** Otherwise place by *which
   process links it*: resident daemon, on-demand isolated execution, or authoring/authz.

A `protocol-*` crate is the residue that passes none of these: only DTO mirrors + route
handlers + exactly one neutral→wire projection. It is therefore always thin and always a
leaf. Contract extraction is **demand-driven, never symmetry-driven**: a contract crate is
born from an actual wrong-direction dependency (as `run-ingress-contract` and
`session-contract` were), not from "each plane should have one." This is why there is no
`config-contract` (the config→execution seam is the ADR-0057 snapshot — a deliberate
*data* boundary, not a port) and no `distributed-contract` (distribution decomposes into
four orthogonal seams that already have homes: the `DispatchQueue` in
`run-ingress-contract`, the commit `Coordinator` in `agent-contract`, the brain-hand wire
in `tool-relay`/`connection-plan`, and sandbox provisioning in `provisioning-contract`).

### 3. The thin-adapter (transcoder) recipe

A protocol adapter is an anti-corruption layer with a fixed shape. The `project()` fold in
`awaken-protocol-managed` (neutral committed messages → public Managed events) is the
canonical instance. The recipe, to copy for protocol #N:

> **A `protocol-*` adapter = ① DTOs (the wire mirror) + ② route handlers + ③ exactly one
> `project(core → wire)` function. There is no fourth kind of thing.**

No port trait is *defined* in an adapter (ports belong in `contract/`); no backend is
constructed in an adapter (backends come from `stores/`, injected by a composition root);
no other crate depends on an adapter except a composition root, the host service layer,
the control plane, or a sibling/executor adapter. A newcomer adds a protocol by copying
the shape, not by studying how the last one reached into the host.

### 4. The remote-Agent seam (the ADR-0058 pattern, applied to delegation)

Remote-agent delegation is driven through the neutral `RemoteAgent` interface
(`awaken-runtime-contract`): `run(agent_id, input, cancellation) → DelegationStep` and
`card(agent_id) → Value`, both neutral types. The A2A wire — `message:send`, task
polling, the discovery card shape — lives entirely in `A2aRemoteAgent`
(`awaken-run-executor-a2a`), the adapter that implements the interface. The host holds
`dyn RemoteAgent` and names no protocol. Native (in-process child Run) and remote Agents
are peer implementations chosen by `agent_id`.

### 5. Fitness functions (the invariant is enforced, not documented)

The invariant is only real if a violating commit fails the build. Four architecture
fitness functions, using the existing enforcement layers, hold it (the first three added
with this ADR, living as pure predicates + cause-effect selftests in
`scripts/ci/_arch_fitness.py`):

- **Contract purity** — a `contract/` crate may not carry a normal dep on an async
  runtime, a DB driver, or an HTTP/wire framework. Catches drift (2) recurring.
- **Protocol leaves** — nothing may depend on a `protocol-*` crate except a composition
  root, the host/control service layer, or a sibling/executor adapter (crate-name prefix
  rule, so it holds for every current and future adapter, including zero-depender ones).
  Duplicated as `deny.toml` wrappers for the adapters that have dependers (cargo-deny
  layer). This is the primary guard against drift (1) — a new reverse edge onto an adapter
  fails the build, forcing the neutral port to move to a contract leaf first.
- **God-hub ratchet** — `awaken-runtime-host`'s first-party dependency count is monotone
  non-increasing (ceiling started at 36 and is now 35). The hub may only shrink; each extraction lowers
  the ceiling in the same commit.
- **Bucket direction + secret-resolution-free runtime** — the pre-existing rules.

### 6. The god-hub dissolution — sequence and blockers

The full crate-split of `awaken-runtime-host` is a **multi-session, port-first** effort,
not a single cut, because every module worth extracting is woven into `SharedHost`: the
modules own its fields, add `impl SharedHost` methods, and the host itself depends on
`awaken-run-ingress`, so dispatch glue cannot move *down* without a cycle. Verified
per-cut state (2026-07-18):

- **Dispatch / durable (`store`, `commit_*`, `dispatch_*`, `durable_ops`)** — 3–16
  `SharedHost` references each; moving down to the ingress family would cycle. Requires a
  `DispatchBackend` port at the host boundary first.
- **Sandbox realization (`sandbox_source`, `provisioning`)** — `sandbox_source` has zero
  host references but *provides* `ThreadEgress` / `ThreadResources` that `SharedHost`
  holds, and uses host-internal modules; `provisioning` reads the run's resolved spec
  (host-plane knowledge). Requires a narrow `SandboxSource` port; provisioning stays.
- **ACP (`acp_backend`, `acp_serve`)** — add `impl SharedHost` methods and are held as the
  `acp` field. `acp_provision` (`EnvLaunchResolver`) reads process-env keys and is
  correctly host-placed: moving it into `run-executor-acp` would violate that crate's
  documented *config-free* invariant.
- **Per-plane routers** — already resolved. `files_router` / `models_router` (public,
  self-contained) moved to `awaken-managed-routers`; `memory_stores_router` /
  `skills_router` deliberately **stay** in the host, because they are the HTTP face of the
  host's private `MemoryStores` / `SkillCatalog` subsystems and moving them would leak the
  subsystems' method surface — worse encapsulation.

The dissolution therefore proceeds by introducing one narrow port at a time, moving its
implementation to the sibling plane, and lowering the ratchet — the fitness functions make
that sequence monotone and safe. The precursor cohesion work (sealing `SkillCatalog`,
`MemoryStores`, `Delegates`, `Compaction` sub-structs) is done; the remaining substrate
fields (`llm`, `sessions`, `hub`, `file_store`, model/provider) are the irreducible core.

### 7. Layout rules recorded (previously implicit)

- `stores/` holds **any** port's durable backend, including domain-specific ones
  (work-queue / session / env registries), not only the generic fs/sqlite/postgres commit
  stores.
- `control → server` is an accepted **anti-corruption exception**: the control plane mounts
  the managed routes and reuses the wire `ErrorResponse`. If it must be severed later, the
  wire error type sinks to `awaken-api-contract`. It is a named edge, not an accident.
- `bin/` holds **composed deployables** — `awaken-cli` (the primary binary), the db-less
  `awaken-worker` daemon, and the in-container `awaken-sandbox` bridge — not "exactly one
  binary." Dev tooling lives in `devtools/` (`publish = false`), off the delivery surface.

## Consequences

- Adding a protocol, backend, topology, or agent capability is a leaf change; the fitness
  functions reject the shapes that would make it a hub edit, so the grain is discoverable
  from the tree and the CI output rather than from tribal knowledge.
- The root disease cannot silently recur: a neutral port re-entering an adapter, a backend
  re-entering a contract leaf, or a new dependency on the god-hub each fail the build with
  a message naming the fix.
- The god-hub shrinks monotonically. The cost is that its dissolution is spread across
  sessions; the benefit is that no step risks a cycle or a broken build, and progress is
  measured by the ratchet ceiling dropping.
- The enforcement is split across two mechanisms (`check_crate_boundaries.py` /
  `_arch_fitness.py` at pre-commit + CI, `deny.toml` at `cargo deny`) — deliberate defense
  in depth, matching the pre-existing overlap between the boundary checker and the wrappers
  allowlist. The duplicated protocol-leaf depender lists must be kept in sync; the
  categorical prefix rule is the source of truth and covers cases the wrappers cannot
  (a zero-depender member leaf).
- This ADR is documentation *plus* code: Phases 0.1 / 0.2 / 3 of the layout work landed the
  three new fitness functions; the neutral-port extractions (session-contract, the InMemory
  backends, the `RemoteAgent` seam) landed the structural changes it describes.
