# ADR-0072: Environment Definition And Execution Boundary

- Status: Accepted
- Date: 2026-08-01
- Depends on: ADR-0041, ADR-0066, ADR-0071
- Supersedes: ADR-0071 D6 only where it makes the mutable Environment
  definition and public CRUD Coordinator-owned

## Context

ADR-0071 correctly separated Control authoring from Coordinator execution for
Agents, but retained Environment definition, execution state, public CRUD, and
self-hosted work coordination in one Coordinator-owned `EnvironmentState`.
That choice now conflicts with the implemented definition plane:

- Agent defaults already author an exact Environment id/revision in Control;
- Environment name, metadata, scope, packages, and networking are mutable
  configuration facts requiring Control IAM, audit, revision, and idempotency;
- Session freezing, package-image realization, self-hosted work, and sandbox
  lifecycle are execution facts owned by Coordinator;
- the Control Admin Assistant carries a second `EnvironmentDraft` vocabulary
  and calls a private Coordinator create command, while public update bypasses
  the create application service.

Keeping the combined owner would make package-image build state increase the
amount of authoring policy in Coordinator. Copying the same mutable Environment
row into Control would instead create two authorities. The boundary therefore
has to split the definition from its executable projection, using the already
accepted Agent publication/registration shape without introducing a generic
registrar framework.

## Decision

### D1: Control owns EnvironmentDefinition

Control owns the mutable `EnvironmentDefinition` aggregate, immutable authored
revisions, lifecycle, scope authorization, audit, idempotency, and the
public Managed Environment create/retrieve/list/update/archive/delete routes.
Every successful mutation produces one immutable revision and one durable
registration intent. A no-op replay does not mint another revision.

The existing Environment config, networking, package, id, and revision value
objects become the one published Environment contract. Managed HTTP and the
Admin Assistant translate into that vocabulary; neither owns a parallel draft
model. The existing Environment registry store migrates to the Control store
group and remains the sole mutable definition repository.

### D2: Coordinator owns immutable executable Environment projections

Control invokes one typed `ExecutableEnvironmentRegistrar::register` port with
the exact Environment id, authored revision, normalized executable
definition, lifecycle, and fingerprint. AllInOne uses a local adapter and split
Control uses an authenticated HTTP adapter. Both reach the same Coordinator
registration application.

Coordinator stores a rebuildable `ExecutableEnvironmentCatalog` projection.
Environment ids are installation-global within the Control authority; IAM and
visibility scope are enforced at ingress rather than copied into the executable
identity. Registration identity is therefore `(environment_id, revision)`. The same
fingerprint is an idempotent replay; another fingerprint at the same identity is
a conflict. Older exact revisions remain addressable and registration can never
move the current pointer backwards.

The projection is not a second authoring aggregate: Coordinator cannot edit the
definition, generate an authored revision, or answer public definition CRUD.
It may retain the minimum display fields required by Managed Session projection,
but those fields remain derived from the registered revision.

### D3: A binding is authored by the aggregate that contains the reference

Control owns Agent-default-to-Environment binding because it is part of the
Agent's authored Session defaults. Agent publication freezes the exact
Environment id/revision and carries it to Coordinator.

Coordinator owns Deployment-to-Environment and explicit
Session-to-Environment binding because Deployment and Session are Coordinator
aggregates. It resolves an exact Agent default or the current executable
Environment revision, then freezes one `EnvironmentSnapshot` into the Session
baseline. Runtime and Worker never reopen either mutable definition store or
executable catalog after that boundary.

### D4: Coordinator owns Environment execution state

Self-hosted Work Queue state, sandbox execution policy binding, package-image
build demand/state/lease, immutable Registry digest, Session realization, and
cleanup remain Coordinator-owned. Registering a SelfHosted revision converges
its one healthcheck. Registering a package-bearing Cloud revision converges one
idempotent image-build demand.

Environment definition success does not require a completed image build. It
does require Coordinator acknowledgement that the exact executable revision and
its required execution intent are durable. `sessions.create` remains the
readiness barrier: it waits internally for the exact image and required stdio
MCP realization before returning success.

The public self-hosted `/v1/environments/{id}/work...` routes remain on
Coordinator. Process composition splits those runtime routes from Control's
definition CRUD; AllInOne merges the same two routers locally.

The Coordinator process starts the build loop as an internal application
worker; it is not an authored resource, public API object, or Awaken Design
concern. SQLite/PostgreSQL build rows are the one durable Pending/Building/
Ready/Failed authority. On Kubernetes, the injected package provisioner creates
a bounded rootless BuildKit Job and pushes the result to the configured shared
OCI Registry. The Kubernetes Job owns no durable lifecycle state, and neither
Runtime Host nor the Session Pod installs Environment packages after a prepared
digest has been frozen.

The existing Kubernetes objects also expose one secret-free, read-only release
proof. A package Build Job and its Pod template carry the exact deterministic
recipe fingerprint in `awaken.dev/package-recipe-fingerprint` and the mutable
push target in `awaken.dev/package-image-destination`. The package-destination
Registry probe reuses the same two annotations on its existing image-check Job
and Pod, so a retry after Build completion can recover the correlation from the
kubelet `imageID` even when the short-lived Build Job is gone. Base-image and
general availability probes do not claim package provenance. The existing
termination message remains the sole BuildKit result and contains the immutable
pushed digest. At the one Sandbox Pod creation seam, the Pod carries the original
`SandboxSpec.scope` in `awaken.dev/sandbox-scope` and the exact resolved
`ContainerPlan.image` in `awaken.dev/resolved-image`; the adapter-local,
possibly hashed Kubernetes runtime id is not a Session identity. The scope key
is deliberately neutral: a warm-pool Pod carries its physical `warmpool-*`
scope, not the later Session bound to its in-process handle. Cloud may use this
field as mission join evidence only when warm capacity is disabled and its value
equals the exact mission Session id. Because neutral `SandboxSpec.scope` is
intentionally unbounded, the pair is emitted only within a small additive
annotation budget. An overlong scope still creates the same Sandbox but yields
no release proof; its hash is never substituted as a Cloud or Session identity.

These annotations are correlation evidence, not a build record, Session map,
admission decision, or cleanup owner. A bounded deployment observer may prove
that a BuildKit termination digest, or the same destination observed by the
annotated retry probe, equals the exact mission Session Pod image and that
kubelet reports the same immutable `imageID`. It must not infer package
provenance from the neutral resolved-image annotation alone. Recipe bodies,
proxy values, registry authentication, image-pull Secret names, and any other
secret material never enter these annotations.

The existing Ready-image availability port receives the exact durable build
demand together with its stored immutable image. Its claimed build completion,
blocking Session readiness, and periodic non-blocking capacity reconciliation
all use that same port. On Kubernetes the package adapter reuses the canonical
recipe projection to derive the destination and the same two annotations, then
runs only the existing destination image-check and accepts it only when the
kubelet digest exactly equals the stored image. A missing or drifted destination
invalidates Ready so the existing claimed build worker can converge it; the
availability path never invokes BuildKit, claims work, or creates another retry
owner. Docker and Podman retain their existing stored-image probes.

### D5: Delivery is durable and fail-closed

The Control definition revision and registration intent commit atomically. A
network or Coordinator failure leaves the intent retryable and does not create
a second revision. Public mutation success means Coordinator acknowledged the
exact registration. An ambiguous retry replays the same command/fingerprint.

Archive or delete registers a lifecycle tombstone. Coordinator rejects the
Environment for new current bindings but retains exact projections and image
facts needed by already-frozen Sessions until their normal reclamation boundary.
No request falls back from Coordinator to the mutable Control store.

### D6: One vocabulary and no generic resource framework

The existing Environment value objects move rather than being copied. The
Admin Assistant's duplicate Environment draft/network/package types are removed
after all callers migrate. The existing Coordinator mutable `EnvRegistry` path
is removed after registration and Session resolution use the executable catalog.

Agent and Environment registrations may share authentication, retry, CAS, and
test helpers, but retain distinct typed ports and repositories. This decision
adds no `ResourceRegistrar`, universal definition service, generic background
task, or second Sandbox provisioning seam.

## Environment Role Catalog

| Fact | Authority | Execution consumer |
|---|---|---|
| Environment definition, revisions, lifecycle, IAM, audit | Control | exact registered projection |
| Agent default Environment reference | Control | frozen exact reference in Agent publication |
| executable Environment revision/current index | Coordinator | Session/Deployment application |
| Session/Deployment Environment binding | Coordinator | frozen Session baseline |
| self-hosted work and healthcheck | Coordinator | self-hosted Worker |
| image build state, lease, and Registry digest | Coordinator | Build Worker and Session Worker |
| live sandbox and stdio MCP process | Worker/Sandbox, ephemeral | Session realization receipts |

## Dynamic Outcomes

| Cause | Durable truth | Outcome |
|---|---|---|
| exact registration replay | one Control revision and one Coordinator projection | existing success |
| same identity, different fingerprint | original projection | conflict; no overwrite |
| Coordinator unavailable after Control commit | definition revision plus pending registration intent | retryable unavailable response |
| older revision arrives after newer | both exact revisions; current remains newer | idempotent acknowledgement |
| Environment update | a new immutable definition and executable revision | old Agent bindings remain resolvable |
| archive | Control tombstone plus Coordinator lifecycle projection | no new binding; frozen Sessions continue |
| image build pending at Session create | frozen Session baseline plus build demand | wait internally or fail without idle success |
| bounded Kubernetes release observation | Build Job/Pod recipe and destination annotations, BuildKit termination digest, cold-created Sandbox scope/resolved image annotations, and kubelet `imageID` | with warm capacity disabled, accept proof only when the scope equals the exact mission Session id and the immutable image identities agree; absence or mismatch is no proof and changes no runtime state |

## Consequences

- Control consistently owns what an Environment is; Coordinator owns how one
  exact version is run.
- Agent and Environment authoring share the same service boundary without
  coupling Coordinator to Control databases.
- Historical exact Environment bindings survive later updates.
- Public Environment CRUD and public self-hosted work share a URL family but
  have different application owners and router composition.
- Distributed registration adds an outbox, transport, projection store, retry,
  and recovery tests; that cost replaces rather than supplements the current
  Control-to-Coordinator create adapter and mutable Coordinator registry.
- Package image build remains outside the neutral provisioning contract. The
  existing `PackageImageProvisioner` and `SandboxProvider::create` mechanisms
  remain authoritative.

## First Vertical Slice

1. Move the existing Environment value objects into one Environment contract and
   migrate every Admin/Managed/Session caller; delete the duplicate draft types.
2. Move the existing registry authority and CRUD application into Control,
   retaining SQLite/PostgreSQL/in-memory parity and idempotency.
3. Add typed local/HTTP executable-Environment registration and the Coordinator
   catalog; switch Session snapshot resolution to that catalog.
4. Split definition and self-hosted-work routers, role stores, and migrations;
   remove the former reverse `EnvironmentAuthor` boundary and Coordinator mutable
   registry.
5. Add Coordinator-owned package-image build state and a store-free Build Worker,
   then make Session creation the readiness barrier.

## Guardrails

- G3: only immutable, serializable revisions cross Control to Coordinator and
  Coordinator to Runtime.
- G13: definition revision and registration intent commit together; execution
  state has one Coordinator authority.
- G45: process roles acquire only their owned authorities.
- G46: Environment definition and executable projection are distinct facts with
  typed registration and exact revision fences.

## References

- [ADR-0041](0041-sandbox-execution-environment-provider.md)
- [ADR-0066](0066-session-service-binding-and-realization.md)
- [ADR-0071](0071-distributed-service-boundaries-and-executable-agent-registration.md)
- [Configuration-to-application flow](../design/config-to-run-execution-flow.md)
