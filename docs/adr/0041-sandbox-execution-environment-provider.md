# ADR-0041: Sandbox Execution Environment — One Process-Level Provider Seam

- Status: Accepted
- Date: 2026-07-03
- Amended: 2026-07-04 (Slice 3/5 mechanism decisions — see Amendment)
- Builds on: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (kernel is
  sandbox-agnostic; a rooted tool is just a `RawTool`, D6),
  [ADR-0035](0035-environment-provisioning-tools-skills-resources.md)
  (`SandboxProvider::create` is the one provisioning seam; `Environment` exposes
  tools/resources/receipt), [ADR-0038](0038-managed-resource-injection-and-store-organization.md)
  (data-factored `MountRequirement`; `awaken-provisioning-contract` holds shared
  vocabulary), [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md)
  (one port per aggregate; content-addressed pins)
- Relates to: G2, G3, G13

## Context

We must eventually run **opaque agent processes** — Claude Code, Codex, an
arbitrary CLI — *inside* the sandbox, not only the runtime's own in-process
tools. Such a process issues its own `open()`/`exec()`/network syscalls and never
routes through our tool layer, so isolation **cannot** be done by rewriting
tool-call arguments (the current `awaken-sandbox-local` lexical `IsolatedRoot`
jail). Isolation must be enforced at the **OS/process boundary** and be
*transparent* to whatever runs inside.

Two reference implementations were surveyed:

- **awaken-next** — a mature execution mechanism: a contract/impl split
  (`awaken-management-contract`), an `EnvProvider` seam with Local/Bwrap/Docker
  realizers, out-of-process daemons (`resourced` read-only fan-out with a
  workspace-scoped LRU and all-or-nothing realization; `memoryd` FUSE
  write-through; `session-runner` with a `PodSupervisor`/fault-domain model), and
  a pure `prepare_environment`.
- **oversight** (`feature-sandbox-config`) — a clean declarative control plane: an
  `EnvironmentKind` config schema (Scope / Sandbox / IsolatedRoot / Image /
  LocalDir), an `EnvironmentSoundness` admission invariant, and pure launcher
  argv renderers (`bubblewrap_argv` / `sandbox_exec_argv` / container).

Neither can be copied wholesale: both leak host paths and use closed vocabularies
that our neutral core forbids (G3, and the "distributed provider in another repo"
rule of ADR-0035). We need one design that reuses their proven pieces while
keeping the neutral contract minimal.

## Decision

Model the sandbox as **one bounded context, two planes, one seam, and tiered
adapters**. The neutral seam is realized as the crate
`awaken-provisioning-contract` (committed) and is **process-level**, centered on
`spawn`, not tools.

1. **Two planes** (reusing the config-publication ↔ runtime-install split):
   - *Control plane* (`crates/agents/*`) owns the **declared** environment
     (`EnvironmentKind` + build) and admission; it compiles a declaration to a
     content-addressed pin and hands the seam a data-only spec.
   - *Provisioning contract* (`crates/runtime/awaken-provisioning-contract`) owns
     the neutral realize surface. *Providers* are adapters below it.
   - `runtime`/`agent-contract` do **not** depend on this contract; running an
     opaque agent is an agents-plane concern (supervise + protocol-bridge), never
     a runtime-core loop.

2. **One seam, primary primitive `spawn`.** `SandboxProvider::{capabilities,
   create, adopt}` realizes a spec; `Sandbox::spawn(Command)` launches any process
   under OS-enforced isolation (the runtime's `RawTool` model is a *separate
   crate's adapter* over `spawn`, not a method on the seam). `Sandbox` also carries
   `attach` (mount), `artifacts`/`read_artifact` (retrieve), and the distributed
   primitives `handle`/`process`/`status`/`renew_lease`. `ProcessHandle` is
   `wait`/`poll`/`signal`.

3. **Tiered adapters behind the one seam**, selected by `SandboxCapabilities` and
   gated fail-closed by `prepare_environment`:
   `Lexical` (Workdir; **not** tool-transparent — trusted in-process tools only),
   `Bwrap` (Namespace; the minimum tier that may host Claude Code — takes
   oversight's pure argv renderers), `Container` (bollard), `K8s` (kube).

4. **awaken-next's subsystems are provider-internal or consumer-side, never the
   contract**: `resourced` fan-out realizes `attach`/`artifacts`; `memoryd` FUSE
   realizes a `MemoryStore` mount (`Realization::Fuse`); `session-runner` /
   `PodSupervisor` is a *consumer* policy over the `poll`/`status`/`signal`/
   `renew_lease` primitives. Monitoring likewise is a consumer (or a sidecar / an
   entrypoint baked into the custom environment), not a contract method.

5. **Deliberately excluded from the neutral contract** (simple design / YAGNI):
   seccomp/cgroup/rootfs/sidecar/mountPropagation (→ `SandboxSpec.extra` +
   provider, declared via `capabilities()`); monitoring methods; ACP piped-stdio
   (provider-specific until a cross-backend consumer proves the need);
   `EnvironmentBuild`/usage-readback. Toolchain stays an open string list, not a
   closed enum. Test for inclusion: *must every backend understand it uniformly?*

## Role Catalog

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `SandboxProvider` | Boundary port | realize/`adopt` a spec → live `Sandbox`; declare `capabilities` | `SandboxSpec`, `SandboxCapabilities` | OS-mechanism choice or host paths crossing upward (G3) | wrong-tier accepted silently | `check_crate_boundaries.py`; provider tests |
| `Sandbox` | Boundary port | the realized-env consistency boundary: `spawn`/`attach`/`artifacts`/reattach/lease | `Command`, `MountRequirement`, `Artifact`, `SandboxHandle` | tool-call semantics; runtime state (G13) | opaque agent left un-jailed | `tool_transparent` capability; object-safety test |
| `ProcessHandle` | Boundary port | one launched process's lifecycle: `wait`/`poll`/`signal` | `ExitStatus`, `Signal` | the agent loop; commit truth | indeterminate outcome after a dropped connection | `poll` idempotency (lifecycle test) |
| `prepare_environment` | Domain service (pure) | validate spec vs `capabilities` → `EnvironmentPlan`, fail-closed | `SandboxSpec`, `SandboxCapabilities` | I/O; provider selection beyond capability match | a guarantee silently downgraded | `prepare::tests` |
| `EnvironmentSoundness` (admission) | Policy | reject a malformed declared environment at publish time | `EnvironmentDecl` | realize-time concerns | broken config reaches provision | `admission::tests` |
| `SandboxHandle` | Value object | a durable, serializable reference to reconnect across restart/host | — | live connection state | orphaned remote sandbox | lifecycle test (serialize → `adopt`) |
| `EnvironmentKind` | Value object (declared) | the declared userland shape (Scope/Sandbox/IsolatedRoot/Image/LocalDir) | `RootfsSource` | the realized live env; host paths (G3) | unreproducible environment | admission (digest rule) |
| Provider tiers (Lexical/Bwrap/Container/K8s) | Adapters | realize the seam at one `IsolationClass` | `SandboxProvider` | the neutral contract's shape | untrusted/opaque code on a non-transparent tier | `SandboxCapabilities.tool_transparent` + `prepare_environment` gate |
| `AgentChannel` | Boundary port (capability) | hand a consumer one duplex to a spawned agent process; only `tool_transparent` tiers implement it | `awaken-connection` Channel, `ProcessId` | raw stdio on the neutral data contract; protocol semantics (G3) | a non-transparent tier forced to expose a stream it cannot back | ISP: a segregated port, never `ProcessHandle::streams`; `tool_transparent` + object-safety test |
| `RunEventSink` (binding seam) | Boundary port | the sole crate permitted to depend on runtime-core; commit projected neutral events (seq + lease nonce) | `StepOutcome`, event sequence | ACP vocabulary; provider mechanism; commit truth beyond append (G13) | agents-plane tangled directly into runtime-core | `check_crate_boundaries.py` (only this crate → runtime); monotonic-seq test |
| `MountSource::Secret` | Value object (declared) | a file-materialized credential with `MountLifetime` + optional write-back | `MountAccess`, `MountLifetime` | inline secret bytes crossing the seam (reference only, G3) | an agent auth file left unrefreshed or leaked upward | admission (secret-ref only); write-back-plan test |

## Guardrails

- **Tool-transparency (the load-bearing rule).** A provider that hosts an opaque
  agent process must report `SandboxCapabilities.tool_transparent = true`;
  `prepare_environment` (and the agents-plane consumer that runs Claude Code)
  rejects a non-transparent tier for that use. Enforcer: capability probe +
  `prepare_environment` fail-closed + provider conformance tests.
- **G3 — no host path crosses the seam.** Every path in the contract is
  sandbox-absolute or a logical/content-addressed reference; host paths live only
  inside a provider. Enforcer: `check_crate_boundaries.py` (contract depends on no
  OS/driver crate) + the crate's public-API snapshot (`public-api/`).
- **G13 — the environment authors no runtime state.** Sandbox tools yield
  content/error only; artifacts and telemetry are downstream projections, never a
  commit path. Enforcer: the `HandOutput`/`RawTool` boundary in
  `awaken-sandbox-local`; artifacts do not flow through `CommitCoordinator`.
- **G2 — one-way dependency.** `agents` → contract ← providers; `runtime`-core
  does not depend on the contract. Enforcer: `check_crate_boundaries.py`,
  `deny.toml`.

## First Slice

Ship the smallest vertical that proves the seam end to end, then add tiers behind
it without changing the contract:

1. **`LocalSandboxProvider` on the committed contract** (`tool_transparent =
   false`): `prepare_environment` → `create` → `spawn` → `artifacts` green, with
   the existing lexical jail for trusted in-process tools.
2. **`BwrapProvider`** (`tool_transparent = true`) + oversight's pure argv
   renderers — the first tier that can host Claude Code.
3. `awaken-protocol-acp` bridge + agents-plane supervisor (session-runner shape),
   an ACL over the official `agent-client-protocol`. The spawned agent's duplex is
   obtained through a segregated `AgentChannel` capability port (not
   `ProcessHandle::streams`), realized by one transport concept —
   `awaken-connection` (core-only; foundation rev already pinned via
   `awaken-scoped-migration`), with local/bwrap backing it by pipes. Events are
   projected and committed only through the `RunEventSink` binding seam (the sole
   runtime-core dependant). File-materialized credentials use `MountSource::Secret`
   with a write-back plan.
4. `FsFileStore` + `resourced`-style fan-out + managed `/v1/files`.
5. `DockerProvider` → `K8sProvider`, reusing `adopt`/`lease`. In-repo as below-seam
   provider crates until a distributed deployment is real, then extracted to a
   distributed repo; never depended on by the agents plane. K8s maps `spawn` to a
   process-as-container (Job/Pod command), not exec-into-idle; orphan reaping uses
   native GC (ownerReferences / TTL), a custom reaper only for Docker; artifacts are
   retrieved out-of-band via an object-store/PVC-backed `outputs_path`, not streamed
   through the control plane.

## Consequences

- **One seam, four interchangeable adapters.** The managed `/v1/files` routes,
  monitoring, and the ACP bridge are written once and hold across
  local/bwrap/container/k8s; a new isolation mechanism is a new adapter, not a
  contract change.
- **Local and distributed share one contract.** `SandboxHandle` + `adopt` +
  `poll`/`status` + `renew_lease` make a remote sandbox reconnectable across host
  restarts and self-reaping when orphaned; the local backend implements them
  trivially, so the addition is non-breaking.
- **The neutral surface stays minimal.** Container knobs, monitoring policy, and
  build/pin vocabulary are pushed to `extra`/provider/consumer/control-plane; they
  are added to the contract only when a concrete cross-backend consumer exists.
- **Cost accepted.** ACP piped-stdio is provider-specific for now (a cross-backend
  bridge may later need a neutral stream abstraction); the custom-environment
  build→digest step lives in the control plane and is not yet ratified as contract
  vocabulary; large-artifact streaming (`read_artifact` returns `Vec<u8>`) is a
  provider concern deferred until needed.
- **The lexical jail is demoted, not deleted.** It remains a trusted-caller
  convenience for in-process tools and is explicitly barred (by
  `tool_transparent = false`) from hosting an opaque agent — closing the original
  correctness gap.
- **Slice 3/5 are opposite sides of one seam (solidified).** Slice 3 (supervise +
  protocol-bridge) is an agents-plane *consumer*; Slice 5 (Docker/K8s) is a
  below-seam *adapter*. They never depend on each other — only on the contract —
  and `check_crate_boundaries.py` ALLOWED_DEPS forbids the shortcut.
- **Transport is segregated, not widened onto every process.** Exposing the agent
  duplex as a separate `AgentChannel` capability (ISP) keeps the provisioning
  contract data-only and spares tiers that never host a protocol; one transport
  concept (`awaken-connection`) covers pipe/ws/attach, so the bridge is written once.
- **Known gaps made explicit, not silently deferred.** File-based credentials +
  write-back are modelled as `MountSource::Secret` (env-only `EnvValue::Secret` was
  insufficient for CLI agents); the runtime-core dependency is confined to one
  `RunEventSink` binding crate; the K8s process/GC/artifact choices avoid
  re-implementing platform mechanisms.

## Amendment (2026-07-04): Slice 3 & 5 Mechanism Decisions

A design review against simple-design/DDD confirmed the structural decisions above
and refined the Slice 3/5 mechanisms. This amendment records the decisions and the
trade-offs accepted; the Role Catalog, First Slice, and Consequences are updated in
place accordingly.

**Solidified (unchanged, now load-bearing):**

- One process-level seam; Slice 3 above it (agents plane), Slice 5 below it
  (provider adapters). No cross-dependency; boundary-checked.
- ACL over the official ACP; no protocol re-implementation. Process-group reaping,
  no-ambient env. Truth commits in runtime-core, fed by projected neutral events.

**Revised (better alternative adopted):**

- *Agent duplex* — a segregated `AgentChannel` capability port, not
  `ProcessHandle::streams`; a single transport concept (`awaken-connection`,
  core-only) instead of a parallel `futures-io` duplex on the neutral contract.
- *K8s process model* — process-as-container (Job/Pod command), not exec-into-idle.
- *Orphan reaping* — native GC (ownerReferences / TTL); a custom reaper only for
  Docker, which lacks equivalents.
- *Artifacts* — out-of-band via an object-store/PVC-backed `outputs_path`; the
  control plane is not an artifact conduit (`read_artifact` direct-read is a
  local-tier convenience).

**Gaps closed:**

- File-materialized credentials + post-run refresh → `MountSource::Secret` with a
  write-back plan.
- Runtime-core coupling confined to one `RunEventSink` binding crate — the only
  crate permitted to depend on runtime-core (G2 boundary).

**Trade-offs accepted:**

- A second capability port (`AgentChannel`) enlarges the surface, justified by ISP:
  tiers that never host a protocol stay unburdened.
- Process-as-container complicates multi-`spawn` per sandbox; accepted because the
  common case is one agent per environment and it buys native
  restart/observability/GC.
