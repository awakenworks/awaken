# ADR-0059: Neutral Core, Leaf Adapters — the Evolution Invariant, Its Fitness Functions, and the God-Hub Dissolution Plan

- Status: Accepted
- Date: 2026-07-18
- Amended by: ADR-0091 (Proposed; §3 where a port trait lives, §6 the `awaken-session-executor` name)
- Builds on: the plane-aligned single workspace and inward dependency rule
  (`Cargo.toml` metadata, `scripts/ci/check_crate_boundaries.py`); the
  `X-contract` extraction convention that factored
  `awaken-dispatch-contract` out of its host (ADR-0039 slice 2.1); the unified
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

Underneath these: `awaken-session-executor` is a **god-hub** — the protocol-neutral session
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

1. **Authority and wrong-direction dependency → contract.** A contract exists only when
   one stable domain authority owns the vocabulary/port and keeping it with an adapter or
   implementation would create an actual wrong-direction dependency. Consumer count is
   evidence to inspect, never an ownership rule: two planes may correctly consume an
   implementation owned by one of them, while a single store/adapter pair may require a
   contract to avoid depending on each other. A data/port-only contract has **no IO, no
   runtime, no backend**.
2. **Survives wire-erasure → neutral.** Erase the wire format and logic remains → it is a
   neutral implementation: protocol-and-backend-free logic in `runtime/` (the kernel), or
   port-composing service logic in the host layer.
3. **A replaceable port impl → `stores/` / `resources/`.** It is *one* implementation of a
   port, swappable without changing system semantics → a backend, beside its siblings,
   under the port's conformance suite.
4. **Deployment role → `server/` / `worker/` / `control/`.** Otherwise place by *which
   process links it*: resident daemon, on-demand isolated execution, or authoring/authz.

A `protocol-*` crate is the residue that passes none of these: only wire mirrors + route
handlers + exactly one neutral→wire projection. It is therefore always thin and always a
leaf. Contract extraction is **demand-driven, never symmetry- or consumer-count-driven**:
a contract crate is born from an actual wrong-direction dependency (as
`run-ingress-contract` and `session-contract` were), not from "each plane should have
one" or "two planes import it." This is why there is no
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

### 4. The remote-attempt seam (amended by ADR-0057 Phase E)

Remote execution is driven through the neutral `RunAttemptExecutor` interface.
Delegation itself remains the neutral `RunDelegationService`, but it always
creates or resumes an ordinary child Run from the target's published snapshot.
The immutable backend reference selects the Native, ACP, or A2A attempt
executor. A2A wire operations, polling and transport authentication live in
`awaken-run-executor-a2a`; the host holds only the attempt port. The former
`RemoteAgent`/`A2aRemoteAgent` path was removed because it duplicated child-Run
admission, transport selection, credentials, recovery and cancellation.

### 5. Fitness functions (the invariant is enforced, not documented)

The invariant is only real if a violating commit fails the build. One manifest walk
builds the workspace model from mandatory `context`, `layer`, and `authority` metadata;
the metadata-derived rules and focused semantic predicates carry cause-effect selftests.

- **Metadata completeness and direction** — every crate declares a bounded context,
  Clean/DDD layer, and authority. Context and layer matrices are defined once by role;
  no per-crate dependency list and no directory bucket participates in the decision.
- **Contract and adapter semantics** — contracts reject database and wire frameworks;
  domain/application code cannot reverse-depend on either interface or infrastructure
  adapters. Live async handles are allowed in an explicit contract because they are part
  of the published port.
- **Ownership fitness** — resource authorization separation, runtime secret isolation,
  migration ownership, Coordinator authority, and Managed route inventory remain focused
  semantic checks over the same workspace graph/source tree.

### 6. The god-hub dissolution — sequence and blockers

The split keeps one protocol-neutral `SharedHost` substrate and assigns role-specific
interfaces to explicit owners. This avoids duplicating Host construction or Session state.
Verified state after the 2026-08-04 cut:

- **Control aggregates and durable adapters** — `awaken-agent-config` owns the Agent
  Config aggregate, compilation and repository ports; `awaken-model-catalog` owns the
  catalog aggregate/ports; `awaken-credential-vault` owns credential materialization.
  `awaken-config-store`, `awaken-model-catalog-store`, and
  `awaken-credential-store` implement those ports. No application crate depends on an
  infrastructure crate, and no compatibility re-export preserves the former mixed path.
- **Outermost host adapters** — `awaken-session-executor` and
  `awaken-acp-application` are classified as infrastructure because their authoritative
  responsibilities compose concrete execution/storage/process adapters. Their names are
  retained to avoid inventing duplicate host or ACP catalogs; metadata records the
  architectural role independently of the physical directory or historical suffix.

- **Session application** — `awaken-session-application` owns the Session repository,
  runtime/environment/credential/resource ports, environment-binding CAS, incarnation,
  lifecycle-supervisor fence, exact Environment selection/runtime validation, the sole
  post-intent creation protocol (insert → finalize → realize → activate → dispatch), and the
  sole Session recovery/WorkQueue projection command.
  Its collaborators are private and reached through explicit application operations/ports;
  `awaken-protocol-managed::ManagedState` has no `Deref` compatibility path and owns only
  wire validation/lowering, projections, event ids, and SSE channels. AI SDK, AG-UI, and A2A
  share one `SessionRunApplication`: it invokes durable `SessionApplication` admission and the
  existing durable activity fence around each effect-capable neutral `RunApplication`
  operation, while control/read operations address committed Thread truth directly.
  Coordinator only assembles these ports; Runtime Host executes the admitted Run and never
  creates, recovers, or owns the lifecycle of a Session.
- **Coordinator runtime interface** — `awaken-dispatch-http` owns durable-operation
  HTTP routing. Neutral durable-control methods remain on `SharedHost` as its application API.
- **Worker runtime interface** — `awaken-coordinator-client` owns registration, heartbeat,
  drain, and claim-fenced Session-control clients. `awaken-session-executor` no longer exports
  those Worker transport adapters.
- **Dispatch execution (2026-10-02)** — `awaken-dispatch-execution` owns the sole
  `DispatchWorker`, process `DispatchPool`, renewal guard, recovery projection,
  process-local capacity, and neutral wake port. `awaken-dispatch` retains
  Coordinator-only SQLite/PostgreSQL dispatch adapters, PG/NATS wake transports,
  and the claimed-commit application; `awaken-dispatch-contract` owns queue
  and claim vocabulary, while its application port has its own bounded contract.
  Runtime Host consumes those contracts and the shared execution owner, with no
  normal Cargo path to the Coordinator Ingress implementation. Test fixtures
  still construct the one Ingress memory backend explicitly at composition.
- **Sandbox realization (`sandbox_source`, `provisioning`)** — the sole ACP
  `sandbox_source` now lives in `awaken-execution-composition` and consumes the
  Session-owned process environment that ADR-0066 consolidated. Runtime Host
  hands it only frozen backend, permission, MCP and continuation facts through
  the existing Session Environment SPI; `provisioning` still reads the run's
  resolved spec in the Host plane.
- **ACP (`acp_backend`, `acp_serve`)** — `SharedHost` holds one injected
  `AcpSessionExecutorFactory` port, selects the frozen root or child backend,
  and owns Run/Session effects and its hub observer. The bound factory, sole
  `sandbox_source`, `PublishedAcpLaunchResolver`, and deployment route assembly
  live in `awaken-execution-composition`; its one source reports the exact
  credential capability and starts the process in the already selected Session
  environment. The static ACP executor implements the same port for trusted
  fixtures. Moving the config-aware resolver into `run-executor-acp` would
  violate that crate's *config-free* invariant. The factory exposes the same
  executor-owned `AcpCli` policy for Host admission. MCP process cleanup uses
  one injected `ProcessReaper` port implemented by the existing ACP Supervisor;
  its TERM/KILL/wait kernel is not copied. Runtime Host has no normal Cargo
  dependency on `awaken-run-executor-acp`; direct fixture references remain
  dev-only.
- **Managed public routers** — already resolved. `files_router`, `models_router`,
  `memory_stores_router`, and `skills_router` are modules of the single
  `awaken-protocol-managed` anti-corruption boundary and consume injected application
  services. The former `awaken-protocol-managed-resources` aggregation crate and the
  host-owned router path were deleted; no compatibility re-export preserves either
  parallel owner.

Further extraction still proceeds one port at a time. The remaining substrate fields
(`llm`, Session slots, event hub, file/resource materialization, model/provider routing)
are shared execution behavior; a role adapter may depend on the substrate, but it must not
reimplement that behavior.

### 7. Layout rules recorded (previously implicit)

- Physical `stores/` holds **any** port's durable backend, including domain-specific ones
  (work-queue / session / env registries), not only the generic fs/sqlite/postgres commit
  stores.
- Physical directories are not semantic layers. Moving a crate does not change its
  architecture; changing `context`, `layer`, or `authority` does and is checked.
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
- `awaken-session-executor` is shared host infrastructure rather than a
  Coordinator/Worker facade. Role-specific interfaces can shrink independently without
  producing two Hosts.
- Architecture enforcement has one source of truth: Cargo metadata interpreted by
  `check_crate_boundaries.py`. `deny.toml` retains dependency-hygiene settings only and
  intentionally contains no architecture wrappers or depender allowlists.
- Interface adapters do not publicly re-export another first-party crate. Consumers import
  credential, Dream, and MCP wire values from their authoritative owner directly.
- This ADR is documentation *plus* code: Phases 0.1 / 0.2 / 3 of the layout work landed the
  three new fitness functions; the neutral-port extractions (session-contract, the InMemory
  backends, and the common Run-attempt seam) landed the structural changes it describes.

## Amendment 1 — Public Run protocols share the Session activity fence (2026-08-25)

AI SDK, AG-UI, and A2A retain their protocol-neutral Message and streaming adapters, but
their effect-capable Run and resume calls now cross the same SessionApplication-owned
Running/Idle activity boundary already used by Managed messages and background jobs. This
removes the former admitted-but-unfenced path: an internal MCP server can authorize
`tools/call` from durable running truth while a hosted Worker executes, without a public
callback URL, protocol-specific lifecycle store, or relaxed running-Run guard. Read-only
history, pending, model, and usage queries remain activity-free.

## Amendment 2 — A hosted Run outlives its observation stream (2026-10-06)

AI SDK and AG-UI each had a local copy of the Local/OwnerScoped Run selector
and unconditionally called `RunApplication::interrupt` when their SSE receiver
closed. In a hosted Session, that receiver is only a best-effort observation
of a durably admitted Run. A browser, proxy, or device disconnect could
therefore turn a still-running provider request into a durable cancellation
while its physical effect was unknown. The dispatch/Session stop-evidence
fence must then refuse replay; it cannot infer provider quiescence from the
HTTP stream ending.

The existing `awaken-session-application-contract` owns one shared Run mode
selection for those two adapters. A Local, connection-bound turn still
interrupts when its client disconnects. An OwnerScoped, durable Session Run
does not: the adapter releases its live stream, while the already-admitted
Run continues under the Session/Dispatch claim, bounded inference budget,
commit, and physical-stop rules. A browser Stop previously used the same HTTP
abort as a passive disconnect, so it now sends `DELETE` to that Thread's
existing AI SDK Run path first; AG-UI exposes the same exact-Thread method.
The existing ApplicationAccess guard requires the `thread.run` operation and
the exact bound Session for this command, rejecting foreign or read-only
capabilities. Both methods invoke the one `RunApplication::interrupt` port;
Flow closes its local stream only after the stop command succeeds. Explicit
user cancellation therefore remains available without turning passive
transport loss into an implicit command. Reconnection observes or idempotently rejoins the original Run;
it never creates a second model or tool authority. A2A's Task stream retains
its own Task publication lifecycle and explicit `tasks/cancel` boundary;
this amendment does not recast its Task as an AI SDK/AG-UI Session stream.

The decision changes no Run identity, event store, recovery queue, terminal
projection, or provider retry rule. A client hang-up is no longer evidence of
Hosted Run termination, and an unknown provider effect remains fenced until
its existing physical/effect evidence is resolved.
