# ADR-0041: Sandbox Execution Environment — One Process-Level Provider Seam

- Status: Accepted
- Date: 2026-07-03
- Amended: 2026-07-04 (Slice 3/5 mechanism decisions — see Amendment);
  2026-07-24 (credential broker composition — see Amendment 2);
  2026-08-11 (Kubernetes egress-policy evidence — see Amendment 3);
  2026-08-24 (one signed sandbox-image publisher — see Amendment 4);
  2026-08-28 (prove immutable staging before release promotion — see Amendment 5);
  2026-08-28 (independent signature and predicate retry closure — see Amendment 6);
  2026-08-29 (dormant provider-neutral Sandbox control transport — see Amendment 7)
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

5. **Package requirements are neutral provisioning data, not an Environment
   adapter.** The exact Environment snapshot carries manager → package specs;
   the host maps it once into `SandboxSpec.packages`. Admission requires the
   provider's `package_provisioning` capability. Podman realizes the requirement
   as a content-addressed derived OCI image built from the exact inspected base
   image ID. Providers that cannot prove this capability reject before workload
   creation; they never run a host package manager or silently ignore the request.
   Manager names remain open in the neutral contract; a concrete provider owns
   the manager vocabulary it can safely realize.

6. **Deliberately excluded from the neutral contract** (simple design / YAGNI):
   seccomp/cgroup/rootfs/sidecar/mountPropagation (→ `SandboxSpec.extra` +
   provider, declared via `capabilities()`); monitoring methods; ACP piped-stdio
   (provider-specific until a cross-backend consumer proves the need);
   `EnvironmentBuild`/usage-readback. Test for inclusion: *must every backend
   understand it uniformly?*

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
| `PackageRequirements` | Value object (provisioning) | exact manager → package requirements crossing the one Sandbox seam | `SandboxSpec`, provider capability | package-manager execution or host mutation | requirement accepted but absent from workload | capability decision table; Podman cache/build tests; real workload-marker E2E |
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
  the metadata-derived crate boundary checker.

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
   process-as-container (Job/Pod command), not exec-into-idle; orphan disposal uses
   the durable referenced-set reconciliation (ADR-0074), while native GC is valid
   only under a lifecycle-stable owner; artifacts are
   retrieved out-of-band via an object-store/PVC-backed `outputs_path`, not streamed
   through the control plane.

## Consequences

- **One seam, four interchangeable adapters.** The managed `/v1/files` routes,
  monitoring, and the ACP bridge are written once and hold across
  local/bwrap/container/k8s; a new isolation mechanism is a new adapter, not a
  contract change.
- **Local and distributed share one contract.** `SandboxHandle` + `adopt` +
  `poll`/`status` + `renew_lease` make a remote sandbox reconnectable across host
  restarts; only the durable referenced-set reconciliation may dispose an orphan.
  The local backend implements the same ports trivially.
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
  and the context/layer rule in `check_crate_boundaries.py` forbids the shortcut.
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
- *Orphan disposal* — the durable referenced-set reconciliation is authoritative;
  ADR-0074 retires age-based Docker/Podman/Kubernetes reaping because process age
  and lease loss are not destruction evidence.
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

## Amendment 2 (2026-07-24): one credential-file broker

`PinnedCredentialMaterializer` is also the Worker composition's
`SecretBroker`; this reuses the authoritative `CredentialRepo` and `SecretStore`
instead of resolving `MountSource::Secret` through `BlobSource` or an
application-owned plaintext path. The standard Worker installs that one broker
after deployment selects the Session provider, so Local, Namespace, pooled
Container, and direct Container variants cannot drift.

Provider guarantees remain explicit. Namespace materializes read-only secret
files and shreds them on disposal. Container additionally supports durable
writable secret write-back through the same broker. Workdir cannot enforce a
read-only mount and therefore fails the admission check for that declaration;
Workdir and Namespace also fail closed when brokered writable write-back is
requested. A higher layer carries only the opaque reference and may not replace
it with `InlineBytes`.

## Amendment 3 (2026-08-11): Kubernetes egress-policy evidence

The Kubernetes adapter remains the sole K8s realization of the process-level
provider seam. It labels each Session Pod with the canonical
`app=awaken-sandbox` and `awaken-egress=open|restricted` posture, but a label is
not enforcement. A composition may therefore advertise K8s network isolation
only when it supplies the versioned `awaken-restricted-egress-v1` evidence that
its cluster policy selects those exact labels, denies ingress for every Session
Pod, permits unrestricted egress only for `open`, and denies all egress for
`restricted`.

Static structure: `SandboxSettings` owns the optional operator evidence;
`DeploymentConfig::sandbox_support` projects it into the Worker manifest; the
existing `K8sRuntime` receives the same evidence and remains the only creator of
Session Pods. The neutral provisioning contract and its `NetworkPolicy` are
unchanged. Platform composition owns the matching Kubernetes `NetworkPolicy`
objects and must not claim evidence when they are disabled or use another label
contract.

Dynamic behavior: without evidence, unrestricted Pods remain admissible while a
restricted request fails before Pod creation and the Worker does not advertise
network isolation. With exact evidence, the Worker advertises deny-all support,
the adapter admits `NetworkPolicy::None`, emits `awaken-egress=restricted`, and
the platform policy denies its egress. Host allowlists remain unsupported and
fail closed because this binary posture contract cannot enforce arbitrary host
sets. Removing or changing the cluster policy requires removing the evidence
before Workers become ready; otherwise the operator has violated the advertised
capability contract.

## Amendment 4 (2026-08-24): one signed sandbox-image publisher

The production Sandbox image is an Awaken product artifact, so this repository
owns its build, publication, and exact-revision evidence. The existing
`deploy/images/sandbox/build.sh` remains the only build-and-acceptance entry;
`stage-binary.sh`, the Rust-generated ACP runtime contract, and the existing
Dockerfile remain below it. Release automation may invoke that owner but may not
recreate its staging, Docker build, or runtime acceptance logic.

The sole publisher is `.github/workflows/release.yml`. It admits only an exact
protected `vX.Y.Z` tag in `awakenworks/awaken`, builds the tagged commit through
the existing script, pushes only
`ghcr.io/awakenworks/awaken-sandbox:<tag>`, and immediately converts that mutable
tag coordinate to the registry's immutable `repository@sha256:digest`
coordinate. Every later release effect uses only that digest. There is no
`latest`, branch, pull-request, manual, private-key, local-script, or second
workflow publisher. Repository ruleset protection for the release tag family is
an operational prerequisite, and the workflow also rejects a ref for which
GitHub does not report protection.

`scripts/release/awaken_sandbox_image_provenance.py` owns one strict, canonical
predicate. It binds the fixed source repository, exact 40-hex revision, exact
semantic tag ref, exact workflow identity and run coordinates, and the fixed
immutable image coordinate. The workflow keyless-signs the image and attests
that predicate with GitHub Actions OIDC, then verifies the exact certificate
identity and issuer and re-runs the same validator over Cosign-verified DSSE.
Unknown fields, wrong subjects, missing or duplicate predicates, format drift,
and unbounded input fail closed.

Static structure: ADR-0041 remains the artifact owner; `build.sh` remains the
build/acceptance owner; the one workflow owns the remote push and Sigstore
effects; the Open-owned predicate validator owns evidence interpretation; and
`check_sandbox_image_release.py` rejects competing publishers or weakened
release controls. A composing platform may invoke the exact validator and bind
its canonical predicate digest into a signed product BOM, but it may neither
build or republish this image nor parse a parallel Open-provenance schema.

Dynamic behavior: a protected semantic tag selects one commit; the workflow
checks tag, revision, repository, protection, and workflow identity before the
build; the canonical script builds and accepts one local image; one push yields
one immutable digest; keyless signature plus exact predicate are attached to
that digest; exact identity/issuer verification and canonical revalidation must
match the emitted bytes. Any failed precondition or verification terminates
without producing downstream release evidence. A consumer starts from the
immutable digest and exact Open revision, verifies through this owner, and only
then records the returned predicate digest.

## Amendment 5 (2026-08-28): prove immutable staging before release promotion

This amendment supersedes only Amendment 4's publication order. Building or
pushing the semantic release tag before proof is unsafe: a crash after that
push but before attestation leaves a release coordinate whose bytes can change
on retry, while trusting image labels would let another package writer preseed
spoof-labelled bytes for the release workflow to launder. Labels are therefore
consistency evidence, never authorization, and an existing release tag without
trusted proof is an error rather than unfinished work for this workflow.

Static structure: the domain owners remain unchanged and no publisher, ledger,
schema, or compatibility path is added. `deploy/images/sandbox/build.sh` projects the
standard `org.opencontainers.image.source`, `.revision`, and `.version` labels
only when release automation supplies the complete set. The sole release
workflow owns a run-scoped staging tag, immutable-digest proof, and final tag
promotion. `resolve_awaken_sandbox_release_image.sh` is the workflow's sole
read-only registry adapter: it resolves a tag once, reads configuration only by
the resulting immutable digest, and applies the existing validator. The
existing provenance validator remains the only predicate and
consumer interpretation owner; it also validates release labels, the one
registry-resolved manifest digest, and promotion metadata. The static checker
locks this dependency direction and rejects release-tag builds or pushes,
pre-proof promotion, parallel registry writers, unbounded tag promotion, and
weakened exact-workflow claims.

Dynamic behavior has two closed paths. If the semantic tag exists, the workflow
resolves it exactly once before any build and thereafter uses only that
immutable digest. Reuse requires all of the following: exact OCI labels; a
keyless image signature whose certificate has the exact repository, workflow
identity, protected tag ref, source SHA, and `push` trigger; and exactly one
verified predicate of the existing type whose canonical bytes bind that same
digest, revision, tag, and workflow. Missing, wrong, conflicting, or multiple
evidence fails without building, signing, attesting, or mutating the tag. In
particular, this workflow never signs or completes a pre-existing unsigned
semantic tag.

If the semantic tag is absent, the canonical script builds and accepts only a
run-scoped staging tag carrying the complete labels, pushes it, and resolves
the pushed image through the same adapter to an immutable digest. The workflow validates labels on
that digest, either reuses its one exact proof or keyless-signs and attests it,
and then performs the same exact signature, attestation, exactly-one predicate,
and canonical-byte checks. Only after those checks succeed may it promote the
same manifest to the semantic tag. Immediately before the write, the same
adapter requires that the tag is still absent or already names the proven
digest; immediately after, it must name that digest with exact labels.
Digest-preserving promotion and its metadata must name the already-proven
digest exactly. A crash before promotion leaves no
release tag, so retry can rebuild safely or reuse proof for an identical staged
digest. A crash after promotion follows the existing-tag path and reuses the
verified immutable digest. Consumers retain their strict exactly-one verified
predicate rule throughout; publisher retry logic does not weaken it.

## Amendment 6 (2026-08-28): independent signature and predicate retry closure

This amendment closes the remaining crash window inside Amendment 5's immutable
proof step. An image signature and the Open provenance predicate are independent
registry effects. Predicate absence does not imply signature absence: a runner
may stop after `cosign sign` commits but before `cosign attest` commits. Repeating
both effects on retry would create ambiguous signature evidence even though the
immutable image bytes did not change.

Static structure remains single-owner. The existing provenance module now owns
both strict predicate interpretation and the bounded publisher-side image
signature classification; it reuses the same strict JSON, DSSE statement,
subject-digest, and input-bound primitives. The sole workflow remains the only
signature and attestation writer. The static release checker locks the one
query/create/requery sequence and its repository-wide writer inventory. No
ledger, signature schema, publisher, registry adapter, or compatibility path is
added.

Signature state is derived independently from predicate state. `S0` means the
exact pinned Cosign v3.0.6 zero-signature response—exit 1, empty stdout, and the
exact two-line `no signatures associated` stderr for the requested immutable
image—or a successful, non-empty, raw-and-verified bundle stream containing no
image-signature predicate. An empty successful response or an unverified bundle
stream is contract drift and fails. `S1` requires exactly one raw
image-signature record and exactly one
cryptographically verified record under the existing exact repository,
workflow, ref, SHA, trigger, and issuer constraints, both bound to the requested
digest. A Sigstore bundle for another predicate is ignored as non-signature
evidence and never substitutes for `S1`. Extra, malformed, foreign, mismatched,
or query-error evidence is `SX` and fails closed. Changing the pinned Cosign
version requires changing this classification contract and its causal tests in
the same commit.

Dynamic behavior follows one decision table. For a fresh immutable staging
digest, `S0/P0` signs once, re-queries to require `S1`, then attests once and
requires the existing exactly-one predicate proof. `S1/P0`, including a crash
after signing, performs no signature write and creates only the missing
predicate. `S1/P1` reuses both facts without a registry write. `S0/P1`, `SX`,
or conflicting predicate state is an invalid publication order and terminates
without repair-by-duplication. An existing semantic release tag is reusable
only in `S1/P1`, and the workflow resolves that tag again immediately before
returning success. Thus retry converges across the sign/attest boundary without
weakening consumer cardinality or treating one evidence type as authority for
the other.

## Amendment 7 (2026-08-29): dormant provider-neutral Sandbox control transport

The process-level provider seam now has one segregated, typed control transport
for capabilities that an opaque process may call at a sandbox-absolute
coordinate. `awaken-sandbox-control` owns the bounded request/response codec,
the `SandboxControlServicePublisher` port, and the single logical Repository Git
credential endpoint
`/run/awaken/control/repository-git-credential.sock`.
`SandboxSpec.control_services` remains the sole demand authority. Durable
Namespace and Container handles record only the exact control topology that was
actually realized; a Container handle also records its provider-owned
incarnation fence. Generic adoption restores that evidence, while spec-aware
adoption requires exact requested/realized equality and then repeats ordinary
capability admission. It never unions, narrows, or infers demand from ambient
provider capability.

Static provider realization stays below that one port. A Linux Namespace keeps
the host rendezvous in its provider-private external directory and projects the
fixed control directory read-only into the sandbox. Socket generation and
device/inode ownership are one publication critical section. A demanded
Kubernetes Pod uses one private `emptyDir`: the Agent mount is read-only, the
digest-pinned forwarder mount is writable, and its TCP listener is loopback-only.
The same trusted binary atomically publishes a PID/start-time generation marker
only after both Unix and TCP listeners bind; the Pod's exec readiness probe
validates that current marker and never connects to the one-exchange business
port. Pod UID, the exact control forwarder/volume/mount projection, forbidden
host-network/host-PID/host-IPC/shared-process/ephemeral-container shapes,
ServiceAccount-token denial, and runtime-owner transfer fence create, adoption,
and every authenticated port-forward. This is not a claim of exact equality for
every admission-defaulted Pod field. Unsupported OS/provider cells fail
capability admission rather than silently dropping the requested service.

Dynamic publication is generation-owned. The first provider channel must open
within a bounded admission interval before a lease is returned. A successfully
published generation may then remain idle for the Session lifetime; only a
started credential exchange has a total deadline. EOF, one failed channel, or a
failed exchange causes cancel-aware bounded-backoff reconstruction under the
same active generation. Close/disposal cancels and joins that generation before
provider runtime removal, cleans only the socket or marker inode it owns, and
cannot retry or reopen after disposal. The Git helper accepts the standard Git
credential fields and line endings, discards unknown attributes, treats unknown
operations and `capability` as successful no-ops, and bounds/zeroizes every
credential-bearing frame.

This amendment installs **dormant transport capability only**. It does not
select a Repository, authorize a holder, resolve Vault material, persist a
credential, expose anything to a model, or generate `GIT_CONFIG*`,
`credential.helper`, or `GIT_ASKPASS`. In particular, the fixed endpoint is
known before sandbox creation so a later Session-owned environment projection
can refer to it without a publish-time path lookup, but no such projection or
helper activation is implemented by this amendment. The current Repository pin
continues to require `Forbidden` model exposure; holder/exposure/claim/verifier
and activation authority remain with the credential and Session owners.
