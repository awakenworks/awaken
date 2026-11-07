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
- Amended: 2026-10-09 (reconciled with ADR-0045, ADR-0054 and ADR-0056: no
  foundation `awaken-connection`, no `RunEventSink`, `reconcile_adoption` not yet wired)

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
   `wait`/`poll`/`signal`. Waiting is a cancellable observation: it cannot consume
   the only unfinished exit handle or hold a lock needed by concurrent poll/signal.
   Local/Namespace and Docker/Podman/Kubernetes exec adapters share the existing
   `awaken-local-process` waiting policy over provider-owned `poll`; native process
   status, remote completion frames, signals and isolation stay in each adapter.
   A transport error remains unknown, never synthesized process exit or a Dispatch
   physical-quiescence acknowledgement.

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
- **Repository path fidelity.** `ResolvedInput.mount_path` remains the exact
  durable/Agent-visible value; provider adapters may validate but never relocate
  it. The standard Managed wire allows an explicit container path, while the
  current substitutable provider profile admits only canonical
  `/workspace/<child>` Repository paths because Container writable-root and
  continuation semantics do not yet preserve arbitrary roots. The shared
  `validate_repository_mount_path` gate runs during new resolution, Manifest
  preflight, cold runtime admission, and each realizer before credentials or Git;
  unsupported historical values remain loadable but cannot start a workload.
  Enforcer: contract decision table plus Managed/Namespace/Container tests that
  assert `/repo` fails without creating `/workspace/repo`.
  `WorkspaceLayout` in `awaken-provisioning-contract` is the sole owner of
  Runtime-controlled workspace roots (`.mnt`, delivered Skills, ACP/tool config,
  and Awaken state); providers consume those constants rather than maintaining
  a second denylist. A separate shared *final-layout* validator compares the
  exact Repository trees with the provider-effective `SandboxSpec`: projected
  File/Memory mounts, frozen baseline mounts, outputs, inline runtime-directory
  values, and Container extra mounts. Cold projection validates the prospective
  baseline and Environment before publishing either; live replacement validates
  before Skill/File materialization; create validates before CacheVolume prewarm;
  adopt and restore validate before provider I/O. The provider adapters retain a
  final `RepositoryRealizationPlan` check as an independently callable effect-edge
  fence, not as another path policy.
  Agent-authoring defaults are the same Provisioning layout fact:
  `resource_input_default_mounts` owns the closed File/Memory/Repository value,
  derives the Repository member through `WorkspaceLayout::child("repo")`, and is
  projected read-only as `/v1/capabilities.resource_inputs.default_mounts`.
  Assistant and Console adapters consume that projection/value; they never keep a
  local path table. Explicit authored paths remain exact, while the Admin Resource
  PUT applies the shared Repository validator before its existing store port.
- **G13 — the environment authors no runtime state.** Sandbox tools yield
  content/error only; artifacts and telemetry are downstream projections, never a
  commit path. Enforcer: the `HandOutput`/`RawTool` boundary in
  `awaken-sandbox-local`; artifacts do not flow through `CommitCoordinator`.
- Local rooted tools and out-of-process Hand tools share `HandToolContext` for
  logical path projection and directory-capability confinement. Local no longer
  keeps a per-tool JSON path rewriter. Absolute `/workspace`, `/mnt`, and
  `/outputs`, plus the relative `outputs` alias, are trusted provider mappings;
  both output aliases resolve to the configured output directory. Relative
  parents survive normalization until resolution against the actual workdir:
  an explicitly authorized sibling mount is valid, an ungranted escape is not.
  A nested projection does not add authority beyond its already-owned parent.
  The content-only adapter remains solely the G13 output boundary.
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
device/inode ownership are one publication critical section. Host-side bind and
liveness probes use a retained directory descriptor through Linux procfs, so a
long host data directory does not enter the Unix socket address. Before handing
the listener to the publication registry, the adapter verifies that its short
address and the canonical cleanup path name the same inode. The descriptor is
process-local and never enters a durable handle or the sandbox-visible endpoint;
procfs and kernel pathname semantics remain external assumptions. A demanded
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

## Amendment 8 (2026-09-05): explicit TrustedHost Environment selection

Environment authoring now names host trust independently from minimum isolation.
`SandboxOverride.host_access` is a closed value: `isolated` (the default) or
`trusted_host`. `trusted_host` is valid only with exact `isolation=workdir`, no
image/network/limit guarantee, and only on a `self_hosted` Environment.
Scheduler resource requests remain valid because they are capacity demand, not
enforcement. The Awaken Environment create command may
bind one exact sandbox-policy version in revision 1; Control resolves and
validates that version before its Environment transaction. Official Managed
wire remains unchanged, and later policy binding remains the update operation
of the same aggregate rather than a second local-mode registry.

The three provider choices have deliberately different guarantees:

| Choice | Working path seen by process | Host files and identity | Network/resource enforcement |
|---|---|---|---|
| Namespace | stable sandbox-absolute path | excluded by OS namespace/Seatbelt capability | only capabilities attested by that provider |
| Container | stable sandbox-absolute path | excluded except declared mounts/broker routes | provider capability admission is mandatory |
| TrustedHost/Workdir | provider-owned host directory; installed Native Hand and ACP prompts use cwd-relative paths; typed Hand tools also accept logical roots | explicitly available to local Native tools and BackendOwned ACP | no isolation claim; read-only is advisory; no network, allowlist, or resource-limit enforcement |

Workdir is therefore a convenience and ownership boundary, not a cloud security
boundary. A network policy, read-only mount flag, or resource request must never
be reported as enforced merely because it was authored for TrustedHost. Cloud
Environment authoring rejects TrustedHost, and a runtime without an installed
local Workdir provider rejects it again. Conversely, an isolated Namespace or
Container never falls back to a BackendOwned local-agent identity.

Static structure remains one path: Environment owns the exact policy reference;
the existing executable Environment projection carries the policy body;
`SessionRuntimeSlot.environment_projection` freezes it; the existing
`SessionEnvironmentProvider` selector chooses Workdir, Namespace, or Container;
Native and ACP reuse the resulting `SessionEnvironment`. No `LocalEnvironment`,
credential store, mount registry, or alternate executor is introduced.

Stdio Hand path projection is explicit: `hand --stdio --mount-root <absolute-path>`
uses the provider's physical managed-resource root. Existing namespace callers
may omit the option and retain `/mnt`; the RuntimeHost passes `/mnt` explicitly
through the existing Namespace renderer, which translates it for Seatbelt.
The Hand no longer guesses a sibling mount from cwd or infers a mount namespace
from Linux. Network/resident Hand deployments retain their `/mnt` contract.
This changes only the projection input, not the Hand protocol, resource ledger,
or TrustedHost isolation guarantees.

TrustedHost now reuses the same Session-owned `SessionHandExecutor` as Namespace
and Container. Startup verifies the release companion's `hand --check` and
installs one lazy Hand factory into the existing trusted Workdir provider,
including a provider previously created by ACP startup. Its command passes both
`--mount-root` and `--workspace-root` as absolute argv values from that provider;
the Hand consumes them through the canonical `HandToolContext`, not a shell
rewrite. Creation/adoption retain configuration without starting a process.
First Native dispatch opens the existing Hand channel; failed launch or unknown
reap never falls back to Worker-local tools. Idle retirement, projection fencing,
retry and terminal close reuse the existing lifecycle. Workdir and Namespace
directory disposal require a Hand stop receipt; only Container disposal itself
owns a physical execution-substrate teardown. Unconfigured weaker Local
embeddings keep their explicit rooted tool path, not a failure fallback.

Installed-binary qualification supplies the exact Cargo JSON artifact to that
same startup owner, because Cargo's intermediate/test directory can differ from
its final binary directory. Release sibling resolution remains unchanged; tests
still require the ordinary Hand capability check and scoped deployment probe.
No alternate startup, build-cache search, or production environment override is
introduced by this test input.

Native prompts are relative to the Hand's initial `/workspace`; the configured
ACP working directory affects only ACP. Bash may subsequently change its own
cwd and must resolve the supplied paths from their stated reference directory.
No physical host prefix is injected into model prompts. This is Native process
wiring, not process-tree supervision: ordinary process reap does not prove tree
absence. Scoped launch, live lease propagation and durable process participants
remain the required activation boundary described below.

Dynamic behavior is fail-closed. Create validates policy existence, enabled
state, cross-field rules, and Environment compatibility before persistence. At
realization, `trusted_host` exact-selects the Workdir provider even when the
model is HostExecutor/Provider; absence fails. BackendOwned Codex/Claude may
reuse only the host identity allowlist documented by ADR-0069. Every other ACP
launch keeps its Session-owned HOME, while Namespace/Container continue to use
Vault/Broker materialization and declared mounts. Suspension, resource
generation, cleanup, and terminal receipts remain those of the same
SessionEnvironment owner.

Worker registration and claim reuse the existing exact backend axis. A Worker
always reports its primary Namespace/Container/Workdir backend and reports the
additional `trusted-host` backend only when the operator selected the Local tier
or set `sandbox_trusted_host_enabled=true`. Coordinator compiles a frozen
TrustedHost Environment into `PlacementRequirements.sandbox_backend =
"trusted-host"`; `can_claim` requires exact membership. It deliberately does not
turn TrustedHost into a minimum isolation requirement, so a cloud Container
Worker cannot substitute itself merely because its isolation is stronger.

```text
Environment create (exact policy version)
  -> executable Environment projection
  -> Session freezes host_access=trusted_host
  -> Run placement requires backend "trusted-host"
  -> Worker manifest exact-match claim
  -> Runtime rechecks local operator opt-in
  -> trusted Workdir realizes resources and launches Native/Codex/Claude
```

Failure at policy lookup, self-hosted compatibility, Worker capability claim,
runtime opt-in, provider availability, resource staging, or Agent launch leaves
the existing durable Run/Session outcome authoritative; none of these stages
falls back to an isolated or untrusted provider.

The API selects the form through the existing versioned policy aggregate, then
binds that exact version atomically when the Environment is created:

```http
POST /v1/awaken/sandbox-execution-policies
{"id":"developer-host","config":{"isolation":"workdir","host_access":"trusted_host"}}

POST /v1/awaken/environments
{"command_id":"create-dev","name":"Developer host","config":{"type":"self_hosted"},"sandbox_policy":{"policy_id":"developer-host","version":1}}
```

Namespace and Container use the same two commands with `isolation` set to
`namespace` or `container` and `host_access` omitted. There is no run-level
string that can bypass the Environment revision or Worker claim fence.

The same policy body now accepts `process_tree_supervision: true` as an
independent hard requirement. Omission retains the legacy wire/fingerprint;
an overlay cannot weaken an already-required guarantee. The requirement is
frozen into the effective SandboxSpec, realization/capacity identity, Worker
placement, and terminal cleanup including final Dispose. Consent to weaker
isolation cannot waive this or any other non-isolation requirement: policy
selection uses the same canonical capability predicate as direct selection.

An additional TrustedHost backend has its own optional
`WorkerManifest.trusted_host_sandbox` capability profile. The existing
`sandbox` field continues to describe only the primary backend. Legacy absence
means unenforced Workdir capabilities, never permission to borrow the primary
Namespace/Container guarantees. The backend identity set still owns operator
opt-in; a profile alone cannot select or install a backend.

This requirement is currently fail-closed, not an enabled production mode:
all built-in providers still report supervision as false, and Worker assembly
rejects an explicit manifest claiming it. The systemd-free Linux namespace and trusted
bootstrap adapters are qualified separately (ADR-0038). Full activation still
requires current Session lease propagation through launch, Native execution
under that same boundary, and process identity/stop evidence integrated into
the existing realization marker before adoption, checkpoint and resource
cleanup. No process database, second lifecycle or manual per-Session cgroup/PID
configuration is introduced.

The optional Linux TrustedHost provider binding now routes its existing
`spawn` and `spawn_agent` through the same `ScopedProcess`, including Native
Hand and sandbox-stdio consumers. It must be selected before the first launch;
a weaker launch permanently forbids upgrading that live object. Current Session
authorization is supplied through one synchronous callback, not retained as a
provider lease. The callback records boot, observer PID namespace and exact init
evidence in the existing realization marker before GO. The marker's one lock
serializes launch against adoption, replay, checkpoint and terminal effects.
Multiple live participants share that marker; only positively absent entries
may be reclaimed. Rebuild source evidence retains the same participants.

Operator selection is carried by the existing standalone Worker/all-in-one
configuration key `sandbox_trusted_host_scoped_execution = true` (default
false). It requires Linux plus the Local tier or the existing explicit
TrustedHost opt-in. The one Deployment validator rejects unsupported combinations
for file-based, typed Worker and direct Host construction. Host startup uses the
existing release-companion locator and resolves bubblewrap once from absolute
operator PATH entries, never Agent environment. A disposable real scoped command
must complete private handoff and exact-tree exit before installation. Missing,
incompatible or stalled binaries fail startup without direct-execution fallback.
Repeated initialization preserves the exact installed deployment; configuration
replacement is rejected, and provider readiness reuses the same probe.
The existing `SessionExecutionBinding` model now distinguishes startup
readiness from current business authority: selecting a scope requires a
completed probe, but that fact alone can never authorize an effect. Its
probe-bypass mutant and reachable candidate/resident effects guard both safety
and non-vacuity. The probe's actual OS evidence remains a model assumption,
exercised separately by real Linux tests rather than claimed as refinement.

The existing Session preparation owner then binds current Worker/Session
authority before its first effect; the probe grants no business authority.
The startup and lease-refresh integration test follows this actual configuration
path rather than manually configuring a test-only provider. This opt-in exposes
the installed Local hardening path for qualification, not the full
`process_tree_supervision` capability: that API/manifest guarantee remains
false pending complete selected-provider and failure qualification.

The same optional boundary wraps Local Repository realization/publication as
one internal `repository-effect` helper operation. The existing Git transport
remains the only clone/CAS/credential authority; its synchronous workflow and
all Git descendants run inside the selected scope. This is post-Environment
resource activation, not a second creation-lock protocol. Requests reuse the
secret-free Repository plan and exact root identity; command credentials use
opaque materialized environment values, never request JSON. Internal effects
use Worker environment, not Agent-configured environment or broker references.
Temporary Git state stays under the existing runtime-owned workspace root.
The existing pre-GO marker and renewal sweep cover the operation; cancellation
retains durable participant evidence until exact process-tree absence.

The parent closes request stdin before awaiting the response, drains bounded
stdio and joins complete scope exit. Valid stdout alone cannot yield a result;
publication additionally checks the exact existing receipt/rejection contract.
`LocalRepositoryEffect` composes `LocalProcessLifecycle`, checking reply,
exit, cancellation and Worker-crash schedules. Its six mutants and three
reachability witnesses are bounded abstract evidence, not Rust refinement,
Git server correctness or a hard kernel teardown deadline. Real Linux tests
exercise clone, publication replay/CAS, private environment, EOF ordering and
early-reply rejection. Container host Git bundles and Namespace Git are not
covered by this optional Local binding; production capability activation remains
disabled until all selected effect paths and deployment wiring are qualified.

Cold observation never signals a persisted numeric PID. A live participant
blocks effects, and missing, masked, foreign-boot or foreign-observer evidence
remains unknown. Destructive preparation, Memory guard retirement/unmount,
secret shredding and deletion cannot turn that uncertainty into a terminal
receipt. Runtime-local weak process handles only deliver explicit stop/cutoff
refresh requests; they are neither a durable registry nor a renewal scheduler.
Interrupted zero-timeout pidfd polling retains a pending observation; the
existing waiter retries rather than spinning or creating an exit receipt.
Invalid descriptor/error flags still fail closed, even alongside readiness.
Refresh delivery errors re-poll the same exact process handle: an observed
complete tree exit makes that renewal unnecessary, whereas a live tree or
unknown poll remains an error regardless of transport error kind. This does
not create a Session stop receipt or retire the durable participant. Linux
provider barriers force exit between preflight and delivery, including denied
authority and broken-channel negative controls. `LocalProcessRefresh` composes
the existing kernel boundary to check error classification under exit races;
mutants reject unconditional success, unconditional failure and leader-only
evidence, with both result branches required to be reachable.
The same `ExecutionDeadline` conversion now samples BOOTTIME before entering
current authority, then derives a fixed cutoff from that earlier anchor and
the freshly returned remaining budget. Startup preflight and cutoff refresh
share this operation and retain the original authority guard through their
effect. Lock waits or thread suspension between budget sampling and conversion
can shorten availability, but cannot extend authority. Expired conversion
invokes no effect; it is not a tree-stop receipt. The ordinary `after` constructor
reuses the same bounded arithmetic for non-authority transport/test deadlines.
`LocalProcessDeadline` composes the existing OS clock/renewal model with this
conversion window; two guard mutants and accepted/rejected reachability
witnesses check both time-minting and stale-effect counterexamples. Adjacent
Rust cases cover bounds, rounding, overflow, delay, denial and effect errors;
real provider barriers cross launch and refresh with expired callback budgets
while retaining exact participant and File evidence. This does not prove OS
clock correctness, arbitrary business-lease callbacks or production Session
scope installation.
Provider integration tests cover two live trees and actual owner SIGKILL;
`LocalProcessRecovery` composes the existing bounded launch model with marker
ordering and unknown-evidence rules. The Session Environment's existing stop
edge now joins Hand closure with the optional provider's exact execution-tree
stop. Revocation, terminal harvest, checkpoint quiescence and filesystem disposal
reuse that edge; Hand uncertainty cannot short-circuit the independent tree stop,
and neither participant alone produces a successful receipt. An unconfigured
provider does not report an execution-tree receipt. Linux lifecycle tests drive
two real scopes through all three stop/quiesce/dispose entrances, retain resource
bytes under Hand or observer uncertainty, and require exact retry plus typed
terminal authorization before deletion. A poll barrier verifies that cancelling
the Hand wait does not prevent tree-stop progress or consume the retained Hand;
concurrent retries converge through the existing marker lock. The same recovery model checks the
Hand/tree receipt conjunction and its mutation oracles. Neither these tests nor
the model establish the still-pending production Session binding, reboot recovery, macOS/Windows,
or real cloud/vendor-credential closure. Worker capability activation remains
fail-closed until that complete composition is installed and verified.

The Session slot now distinguishes uninstalled, live and closed realization
authority in one projection. Closing hides the lease and cancels Resource
preparation under the synchronous slot mutex before either asynchronous owner
lock is awaited. Cleanup retains an exact closed-lease CAS, so a replacement
installed during that wait cannot be interrupted or retired by the old caller.
Cancellation does not reopen the projection. A retained physical owner preserves
closed authority through revocation reset; MCP cannot substitute an old direct
assertion when the root is explicitly closed. Both renewal delivery entrances
use the same slot-local response application method. A delayed response cannot
recreate absence, overwrite a later snapshot or reopen the closed generation.
The existing phase model still owns scheduler fairness; the bounded
`SessionRealizationAuthority` model separately checks exact response, closure and
cleanup interleavings with two callers. Its mutation checks include positive
grant/cleanup reachability. Executable Host barriers cover both owner locks,
single/Worker-wide revocation, cancellation and generation replacement.
Worker authority loss now atomically closes the currently installed cohort and
captures its existing owner-lock Arcs before awaiting pool drain or interruption.
The subsequent cleanup uses those captured locks and rechecks both allocation
identity and closed lease, so removal/recreation with identical lease bytes
cannot redirect an old cleanup into new owner locks or effects. Independent
Session cleanups are joined without short-circuiting on another error or lock
wait; cancellation retains the closed pending owners for explicit retry. This
is one ephemeral cleanup selection, not another registry, timer or lease store.
The authority model includes two slot allocations and one ready peer, checking
cohort closure and peer progress under weak scheduling fairness. Host tests
cover both owner locks, same-lease/lease-free ABA, blocked peers, failed cleanup,
and the Worker handler's split close/interruption/retirement sequence. Actual
late admission is now fenced by the Worker root composition described below;
the production provider callback and its cutoff refresh remain explicit
composition work and are not implied by the selected-cohort proof.

Worker heartbeat supervision now has one revocation exit for ordinary proof
loss and dependency unwind. The heartbeat future is consumed before invoking
the existing cohort-close/drain/retire sequence; the task cannot report normal
completion while that sequence is pending. This remains in the same heartbeat
task during graceful drain, without another monitor or lease projection.
`WorkerAuthorityExit` composes the existing cohort model; barrier tests check
normal return, unwind, pending cleanup and explicit abort, and a real Worker
HTTP fixture injects a post-Ready provider-probe panic. Explicit task abort,
process death and panic-abort builds are not unwind recovery; physical cutoff
and final shutdown ownership remain separate requirements.

Worker heartbeat timing and Session installation now consume one guarded
`WorkerAuthorityProof` per registration, replacing the heartbeat loop's private
receipt variables rather than copying their authority into a Host cache. The
registry and existing `AuthorityLeaseTiming` remain the durable owner and timing
policy. `CurrentExecutionAuthority` moved unchanged to the neutral provisioning
contract; the Local provider and the Worker implement/consume this same port.
The Worker injects its proof before enabling dispatch, including the AIO build
path. Direct unregistered Hosts retain their existing Session authority owner;
absence of a Worker proof does not qualify process-tree supervision.

Static ownership is Worker receipt -> neutral synchronous port -> Host Session
slot mutation, with physical providers depending only on the port. Dynamic
order is request-start anchor -> current Applied receipt -> guarded Resource
preparation or complete projection publication. Root close precedes cohort
selection and all asynchronous drain waits. Both Session renewal delivery
entrances also check that root. An expired initial response cannot start claim
polling; a fresh renewal cannot revive an expired/closed old Worker proof.
Graceful drain keeps the proof while existing runs finish, then closes it before
the heartbeat task is aborted. No timer, lease store, mode selector or shadow
Session registry is added. Linux uses the same suspend-inclusive BOOTTIME source
as the execution cutoff, retaining submillisecond precision; other platforms
retain their existing Instant timing and do not gain a supervision claim.

Request-start observations and conservative remaining-time arithmetic now live
in the existing neutral `authority_lease` value objects. The Local process adapter
is the single platform sampler and reuses the existing physical cutoff clock on
Linux; Worker startup and heartbeat both consume it. Observations contain no
wall-clock value, are not serializable, and confer no ownership independently
of the existing guarded receipt. This extraction removes the Worker-private
clock implementation before Session receipt integration; it does not yet fix
the Session projection's absolute-expiry clock correspondence or qualify a
direct all-in-one Host's execution authority.

The Session response-relative evidence contract, shared renewal lane and final
projection publication are owned by the
[ADR-0066 amendment](0066-session-service-binding-and-realization.md#2026-09-07-amendment-request-relative-realization-evidence).
That amendment also owns terminal Provider clock migration: preparation,
reconstruction and physical removal consume current proof without repeating
Worker UTC checks. Ordinary and terminal checkpoint WAL now use that same proof.
Creation/adoption clock correspondence and complete
platform qualification remain open; this ADR defines no second Session clock.

Compound Kubernetes terminal and restored-target removal forward that same
current-effect authority through the existing immutable-incarnation port. The
Pod/PVC UID/resourceVersion protocol and cleanup finalizer remain the sole
physical interlock. A borrowed admission callback rechecks authority after
asynchronous responses, before subsequent mutations and during deletion waits,
including projected ConfigMap/Secret deletion and finalizer conflict retries.
It owns no new lease, registry or cleanup job. Lost authority returns failure
without issuing the next request; an already accepted remote mutation is not
rolled back. The existing aggregate retry supplies a new successor over the
original preparation and exact handle, preserving the durable finalizer until
the existing cleanup protocol can complete. Rebuild's separate exact-identity
deletion retains its existing admission owner; it does not infer a terminal
disposal grant.

The adjacent Kubernetes cleanup test owns the response-cut decision table:
normal and finalizer-conflict paths cross every actual kube HTTP response with
renewal, revocation, replacement, expiry and cancellation, assert the committed
mutation prefix, and retry under a fresh successor to exact total absence.
The existing `SessionExecutionBinding` two-effect wait/check model owns the
corresponding admission obligation. This is deterministic adapter evidence,
not real API-server/controller, credential, unbounded-liveness or Rust-refinement
proof; in-flight remote commits remain outside the local authority lock.

The existing `SessionExecutionBinding` model now checks both fixed deployment
topologies. A registered Worker requires its live root proof; a direct
unregistered Host requires its actual Session owner without synthesizing a
Worker lease. Both retain the same scope readiness, Session generation, slot
allocation, Environment identity and held-lock constraints. The bounded check
explores 1,909,674 distinct states at depth 30; eleven guard/lock mutants fail
their independent oracle, and candidate/resident plus local/registered effect
witnesses are reachable. Deployment classification and OS truth remain trusted
inputs. The abstract Session-expiry bit does not prove the new receipt clock
adapter, complete-projection field atomicity, or unbounded cleanup liveness.

Adjacent cause/effect tests cover the initial/renewal latency boundary,
irreversible close, clock regression, Linux suspend, callback error/unwind,
root-lock contention, late complete projection with/without Resources, both
renewal entrances, and cancellation during a real gated Resource read. A real
HTTP Worker fixture rejects an aged first receipt before dispatch. The bounded
`WorkerExecutionAuthority` model composes the existing Session slot/cleanup
kernel with one root receipt, one outstanding response and a finite boot clock.
It checks late responses, fresh admission, generation replacement and renewal.
It also admits an invalid response observation before its request start;
removing that lower-bound guard must violate the independent receipt oracle.
Seven guard mutants plus admission/renewal/rejected-response reachability
witnesses run in CI. Adjacent Rust rows W10/W11 cover initial and renewal
responses and verify that later valid receipts cannot reopen a closed proof.
One outstanding request and boot time 0..3 exclude response reordering and
general clock rollback detection from the model's claim.
Its atomic callback abstraction does not prove arbitrary callback duration,
physical GO/cutoff refresh, OS clock correctness, Rust refinement or cleanup
liveness.

The optional Linux Local execution deployment now selects one scope on fenced
create, Ready replay and exact-spec adoption. It is immutable deployment data,
not a capability proof: `process_tree_supervision` remains false, including for
a configured provider. Unfenced creation, spec-less adoption and unqualified
restoration reject instead of silently falling back. Selected-but-unbound
scopes reject both process and Agent launch; a weak first launch cannot upgrade.

The existing Session Environment candidate owner binds that scope once to a
view of the current Worker proof and Session slot. Both create and adopt use
one enrollment implementation. The view retains no renewable lease: it checks
the asserted generation against the live lease, existing owner-lock allocation
identity and exact Environment Arc under the original locks. Candidate and
Resident may execute; Retiring, missing/replaced owners, expired authority and
unknown binding cannot. Weak references to the map and Environment avoid an
ownership cycle. Terminal-only candidates without live authority remain
unbound and retain their cleanup identity without gaining execution permission.

The existing Session renewal sweep concurrently drives due Control lease
writes and bounded physical cutoff refresh. A long Session lease no longer
short-circuits refresh of a shorter Worker proof. Both lanes finish their peer
work before reporting errors; current authority is reread at each physical
effect. No second timer, process registry or lease store is added. Native Hand,
ACP and sandbox-stdio continue to share the provider spawn boundary. Local
Repository preparation/publication uses the internal helper described above;
Namespace/Container host-side Git is not covered by that Local boundary.

`SessionExecutionBinding` composes the Worker/Session kernel with one selected
scope, two physical owner allocations and candidate/resident/retiring phases.
It checks one-time binding, no late weak upgrade, exact guarded effects and
same-lease allocation ABA. Independent receipt oracles, ten guard mutants and
candidate/resident reachability witnesses qualify the model. Adjacent Rust
decision-table cases check every authority coordinate, both budget minima,
original mutex retention, weak ownership and callback errors. A real Linux
candidate test crosses unbound rejection, enrollment/retry, launch, no-due
refresh, Worker closure and exact tree stop while preserving File bytes.
Callback elapsed time and OS enforcement remain separate deadline/boundary
obligations; this is not Rust refinement or unbounded distributed liveness.
The opt-in deployment/probe installation described above is now production-wired.
Host composition now returns expected configuration/probe errors through its
existing API. The Worker registration owner consumes startup (including dependency
unwind or cooperative shutdown), then owns one bounded deregistration on every
returned lifecycle path. Initial provider readiness has a five-second bound;
claim polling starts only after successful local acceptance of the first Ready
receipt. Cancellation may follow remote Ready delivery but cannot revive the
dropped local startup future. Startup creates no heartbeat/probe/warmup tasks.
The withdrawal result distinguishes applied/absent/replaced exact identity from
rejection or unknown response; unknown withdrawal cannot turn an otherwise
successful local stop into reported completion. The original startup failure
remains primary when withdrawal also fails. There is no detached cleanup task
or second registry owner; caller abort, process death and panic-abort builds
still require lease expiry and physical process-cutoff qualification.

`WorkerDrain` extends its existing admission/registry ordering with startup,
separate initial Ready delivery/acceptance and one withdrawal. Its finite model
checks failed-startup admission, single withdrawal, receipt-qualified remote
death and eventual local return under owner/timeout polling fairness. Unknown
responses are not proof of remote cleanup. Guard/liveness mutants and four
reachability witnesses run in the existing lifecycle mutation gate. Real HTTP
decision tables cover factory errors/unwinds, probe errors/unwinds/timeouts,
shutdown success/error, initial receipt cancellation/expiry and withdrawal
receipt/error/timeout outcomes. This is not Rust or transport refinement.

Session lease wall-clock/monotonic correspondence, terminal Repository
crash/retry integration, reboot recovery and real cross-platform/cloud
qualification remain required before advertising full supervision.

Native Linux readiness now executes the same `bubblewrap_argv` profile as a
real Namespace launch, including PID/IPC/UTS/network namespaces and parent-death
linkage. A user-namespace-only probe is insufficient. Each observation is fresh,
bounded, and retains only operator PATH for launcher lookup, not ambient
credentials or loader hooks; a previously successful
probe cannot mask policy revocation, and a failed probe cannot permanently mask
subsequent installation. Probe failure is unavailable, not a fallback launch or
a quiescence receipt. This improves the existing Namespace readiness path; it
does not yet install TrustedHost process-tree supervision.

The systemd-free integration must reuse this launch authority. Its TrustedHost
host-view qualification must separately check host identity and access: an
unprivileged user/PID namespace can retain absolute filesystem paths while
changing supplementary-group visibility and host PID semantics. A successful
Namespace readiness probe is therefore not a TrustedHost compatibility result.
## Amendment: container reservation precedes activation (2026-09-08)

Container runtime realization has one two-phase port. The shared
`ContainerRuntime::create_for_effect` method is only a thin direct-call
composition of `prepare_for_effect` and `activate_for_effect`; Docker, Podman,
and Kubernetes no longer implement a second combined path. Docker reserves a
created, stopped container, Podman uses `create` rather than `run`, and
Kubernetes reserves the UID-fenced Pod before projected participants converge.
Activation re-observes the exact fingerprinted locator before Docker/Podman
start or Kubernetes convergence and returns success only for that locator.

The existing stable-scope discovery, immutable labels/annotations, Pod UID,
PVC binding, and `existing_realization_decision` remain the physical authority.
Only the existing runtime port and the three adapters are modified; the Podman
reservation argv is projected from the same plan renderer. The backend object
remains the reservation record; the later Memory amendment adds one recovery
receipt under its retained staging lifecycle rather than a second realization
state machine or recovery service.
The direct composition preserves effect-free runtime callers but is not a crash
boundary. Provider ordering is still `Memory -> combined create` at this
amendment point, so this split alone does not claim cross-process Memory
responsibility. That requires the provider to call the two phases explicitly
and recover the backend-owned staging locator before mounting.

### Memory publication amendment (2026-09-08)

The provider now completes that ordering through the same physical authority:

1. Docker/Podman reserve an exact stopped container; Kubernetes reserves its
   UID-fenced Pod. Existing backend labels and annotations remain authoritative.
2. The provider resolves the reservation's staging root, recovers an unfinished
   attempt when necessary, and materializes a copy through the one injected
   `MemoryMounter`.
3. Secret-free original-head evidence is atomically recorded as a
   pre-activation recovery receipt inside that exact backend-bound staging
   lifecycle. This is new persisted recovery data, but not another physical
   realization authority: the stopped container or UID-fenced Pod still decides
   whether the receipt may be consumed. Only then may the runtime start the
   container or release the Pod projection gate.
4. If activation succeeds but its response is lost, a later Worker observes the
   same active object and reads that receipt without remounting the live copy.
   A prepared object instead compensates the exact old path before remounting.

The existing `MemoryMountOutcome`, backend reservation state, retained staging
guard and materialization evidence remain the only lifecycle owners. No
provider-neutral shadow lifecycle is added beside the actual Local
pending/confirmed owner and Container backend state. Process-local Memory
handles remain transient participants in the existing `StagedMounts` lifecycle
until activation publishes them to `ContainerSandbox`.
Staging path allocation and reopening stay in the existing `StagingGuard`,
`staging_dir` and restoration-target authority; materialization ordering uses
`MemoryMaterializationEvidence::canonicalize_all`.

Production realization already holds the Session root realization lock across
one directive. Across Workers, the root-owned realization lease and exact
Worker/Session effect fence admit only the current owner; Docker/Podman stable
names and Kubernetes scope labels/UIDs additionally serialize the physical
create conflict. The provider adds no process mutex. Direct unfenced adapter
calls are deliberately outside this cross-process exclusion guarantee.

Container providers accept only restart-recoverable copy materialization.
`WriteThroughRequired` is rejected before backend or Memory effects because a
live FUSE participant cannot currently be transferred after Worker death. This
is an explicit capability boundary, not a write-through-to-copy fallback. The
recovery guarantee covers Worker process loss while the backend-bound staging
directory remains available; host loss and cross-node staging recovery remain
outside this contract and fail closed.

## Amendment 9 (2026-09-11): one documented Managed Agents base contract

The production Sandbox image follows the stable, publicly observable foundation
of Anthropic's Cloud Sandbox reference: Ubuntu 24.04 LTS on amd64, Node 22, Go
1.24, and Ruby 3.3 as the documented defaults, non-root execution, and
`/workspace` as the working tree. This is behavioral compatibility with the
published reference, not a claim that Awaken can reproduce Anthropic's
unpublished image bytes or private hardening.

`deploy/images/sandbox/Dockerfile` remains the image recipe and
`deploy/images/sandbox/build.sh` remains its only build and acceptance owner.
Every external base is digest-pinned, the build fixes `linux/amd64`, and the
accepted image carries a versioned Managed Agents base label plus a versioned
Sandbox role-surface label. Acceptance executes the OS/architecture/user/path/
default-runtime checks and the complete closed `awaken-sandbox` role vocabulary.
Both fresh build and local reuse pass the same checks; `--ensure-hand` is not
permission to reuse an image missing another Pod-local helper.

The signed release-label verifier requires those contract labels in addition to
the exact source revision. Therefore a previous Debian image, a moving base, or
an image missing `control-forwarder` cannot be promoted or silently reused after
the contract changes. Environment package layers continue to derive only from
the admitted base image identity; they do not own another base-image recipe.

## Amendment 10 (2026-09-29): workload hardening and one allowlist wire owner

`awaken-local-process` owns the Linux workload syscall profile and trusted
operator executable resolution. Namespace launch, its readiness probe and the
Workdir networkless shell consume that same profile mechanism and descriptor
mapping. Namespace retains Unix sockets for declared Sandbox control services,
but a denied-network launch does not project host resolver runtime directories.
The host-view Workdir shell instead denies creating Unix sockets and masks host
runtime state; that shell does not upgrade the Workdir provider's capabilities.
An unresolved isolation launcher or filter preparation failure rejects launch.

The existing `awaken-container-contract` owns the allowlist capability payload
and DNS-host normalization consumed by the open issuer and closed Gateway.
The Gateway verifies the existing signed wire format and owns public-address
connection admission; the runtime separately proves that direct traffic cannot
bypass it. Neither proxy variables nor a token alone establish isolation. No
second provider, egress service or realization authority is introduced.

Container execution uses the existing backend security projections: capabilities
are dropped and privilege escalation is disabled; Docker and Podman retain an
init owner. Kubernetes agents and helpers share the production image's non-root
identity and volume group, RuntimeDefault seccomp, and disabled service links.
The backend realization configuration records this security-floor revision, so
recovery does not reinterpret a pre-floor object as an equivalent realization.
Kubernetes deny-all and allowlist postures consume their own live policy evidence.

## Amendment 11 (2026-09-30): bind orphan reaping to process identity

The existing `exec_attempts` owner supplies one wrapper prelude and one orphan
reaper to Docker, Podman and Kubernetes. A PID alone is not an identity: a
completed exec can leave its PID record behind, and a later exec can reuse that
number. The wrapper now records the Linux `/proc` start time beside the
existing PID file. The signal APIs retain their exact runtime process status
checks and PID-file address; no second process registry or lifecycle is added.

At disposal, the existing attempt ledger seals new work before asking the
container-local reaper to inspect each record. A missing start-time record or
unreadable live `/proc` identity makes quiescence indeterminate, so container
removal proceeds without in-container harvest. An absent or different process
identity removes only the stale record. Only a matching live identity may be
signalled; the reaper excludes its own record, observes exit after TERM or KILL
within a bounded deadline, and refuses quiescence if that observation remains
unknown. This protects a replacement Pod and its retained workspace from stale
PID reuse without treating a signal request, CRI timeout or missing observation
as an exit receipt.

## Amendment 2026-10-09: reconciled with ADR-0045, ADR-0054 and ADR-0056

- **Transport.** The Role Catalog `AgentChannel` row, First Slice item 3, the
  2026-07-04 amendment, and Consequences name foundation `awaken-connection` as
  the one transport. Per the 2026-09-06 amendment of
  [ADR-0045](0045-connection-plan-and-network-topology.md), that adoption did not
  happen: the duplex is `AgentChannel` (`crates/platform/environment/awaken-agent-channel/src/lib.rs:22`),
  dialed through `awaken-connection-plan::TokioChannelFactory`
  (`crates/platform/sandbox/awaken-connection-plan/src/factory.rs:40`). The workspace
  `Cargo.toml` has no `awaken-connection` dependency.
- **`RunEventSink`.** No such type or crate exists in `crates/`. ACP runs as a
  `RunExecutor` (`crates/sdk/contracts/awaken-runtime-contract/src/execution.rs:198`)
  implemented by `awaken-run-executor-acp`, which depends on
  `awaken-runtime-contract`, not on `awaken-runtime`
  (`crates/platform/execution/awaken-run-executor-acp/Cargo.toml:22`); see
  [ADR-0054](0054-safe-loop-boundary-shared-seam-and-pause-as-durable-await.md).
  The "gap closed" by one binding crate is closed by that contract boundary instead.
- **Orphan disposal.** First Slice item 5 and the 2026-07-04 amendment cite the
  durable referenced-set reconciliation of ADR-0074. Per the 2026-10-06
  amendment of [ADR-0056](0056-sandbox-reuse-two-orthogonal-volumes.md),
  `reconcile_adoption` has no production caller (only tests in
  `crates/platform/environment/awaken-provisioning-contract/src/lease.rs`); it is the agreed
  design, wired when the disposal decision becomes shared. Today each provider
  owns disposal, with the process-identity reaper of Amendment 11.


## Amendment 2026-10-18: one provider port

The container projection is merged into the one `SandboxProvider` port; its
readiness role, live type, restore ladder and provider-owned handle evidence
are specified in [the provider contract](../design/sandbox-provider-contract.md).
