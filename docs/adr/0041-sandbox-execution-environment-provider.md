# ADR-0041: Sandbox Execution Environment — One Process-Level Provider Seam

- Status: Accepted
- Date: 2026-07-03
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
3. `awaken-protocol-acp` bridge + an agents-plane supervisor (session-runner
   shape) — running an opaque agent as a first-class, protocol-bridged capability.
4. `FsFileStore` + `resourced`-style fan-out + managed `/v1/files`.
5. `DockerProvider` → `K8sProvider` (distributed repo), reusing `adopt`/`lease`.

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
