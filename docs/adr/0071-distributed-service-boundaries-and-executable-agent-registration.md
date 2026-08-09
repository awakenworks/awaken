# ADR-0071: Distributed Service Boundaries And Executable Agent Registration

- Status: Accepted
- Date: 2026-07-30
- Depends on: ADR-0031, ADR-0032, ADR-0038, ADR-0062, ADR-0063,
  ADR-0065, ADR-0066, ADR-0067
- Supersedes: ADR-0031 D1/D4 and ADR-0032 D1/D4 only where they require a
  process-local whole-catalog install as the publication-to-execution handoff

## Context

Before this ADR's first implementation slice, publication persisted a
`StoredPublication` and then wrote the same `ExecutableAgentSnapshot` into a
process-local catalog owned by Config Service. The Deployment adapter still
calls the ordinary Managed Session
creation command in the same process. Remote Workers already use authenticated,
claim-fenced dispatch and commit transports, but publication availability,
Deployment-to-Session launch, exact credential materialization, and per-kind
Resource access do not yet have equally explicit service boundaries.

The earlier documentation described a whole-catalog install protocol that no
longer exists in the workspace. Keeping that vocabulary would create a second,
fictional implementation path beside the actual per-Agent publication and
snapshot path.

Two alternatives were considered:

1. retain a whole-catalog install abstraction and adapt every service around it;
2. register each immutable executable Agent publication with the Coordinator and
   reuse the existing Session, dispatch, commit, credential, and Resource ports.

The second option matches the implemented aggregate boundaries, avoids a second
catalog authority, and supports both AllInOne and distributed deployment.

## Decision

### D1: Control publishes; Coordinator registers executable Agents

Control remains authoritative for Agent authoring, source revisions, compilation,
and durable `StoredPublication` history. After persistence, Control invokes one
`ExecutableAgentRegistrar::register` port with the existing immutable
`ExecutableAgentSnapshot` and its Workspace, Agent, source-revision, and
fingerprint identity.

The Coordinator stores a rebuildable `ExecutableAgentCatalog` projection. A
registration is idempotent by `(workspace_id, agent_id, source_revision)`;
repeating the same fingerprint returns the existing result, while a different
fingerprint for the same identity fails closed. Older exact revisions remain
addressable and never move the current pointer backwards.

`register` is the boundary verb. `publish` remains the Control-domain action,
`projection` describes the derived data architecturally, and `upsert` may name
the repository operation. `install`, `project`, and `coordinate` are not aliases
for this boundary.

### D2: One port, local and remote adapters

AllInOne uses `LocalExecutableAgentRegistrar`. A split deployment uses
`HttpExecutableAgentRegistrar` and the authenticated
`executable_agent_registration_router`. These are transport adapters for the
same port and commands, not a second registration service or state machine.
Startup rehydration calls the same registrar; it does not maintain a second
warm-install path.

The private HTTP boundary requires a bearer token. Its client retries only
idempotent network, availability, and storage failures; validation,
authentication, and semantic conflicts return immediately.

Publication success means the Coordinator acknowledged registration. A durable
publication may exist while registration is temporarily unavailable; that call
returns an availability failure and an idempotent retry registers the same
publication.

### D3: Coordinator owns Deployment and reuses the existing Session authority

`DeploymentState`, its public API, scheduler, repository, and lifecycle outbox are
owned by Coordinator beside Session. `DeploymentSessionLauncher` remains the one
in-process application port and carries the existing stable `deployment_run_id`.
Coordinator Session creation is idempotent by DeploymentRun identity, so recovery
after an interrupted call cannot create a second Session.

The earlier split placed the public Deployment API in Control while Coordinator
constructed a second `DeploymentState` over the same repository to run schedules.
That parallel aggregate and its private HTTP launch client/router/token are retired.
The public gateway routes Deployment APIs to Coordinator; AllInOne merges the same
local component.

### D4: Resource unification stops at the Session manifest

`InputBinding`, `ResolvedSessionResources`, and `SessionResourceManifest` remain
the common, secret-free Session input language. Materialization stays per kind:

- File uses immutable content retrieval and digest validation;
- Memory uses snapshot/read plus CAS write-back;
- Repository uses `RepositoryRealizer` and an exact credential binding;
- Skill uses an immutable versioned bundle source and remains a capability
  domain, not a generic mounted Resource lifecycle.

Distributed Workers use narrow per-kind clients. This decision adds no universal
`ResourceService`, `ResourceMaterializer`, Resource aggregate, or transport
framework. A Resource provider may be deployed separately when its data,
security, or scaling characteristics require it; Memory is the first natural
candidate.

### D5: Credential materialization stays exact and claim-fenced

The existing `CredentialMaterialResolver` contract remains authoritative. A
remote adapter carries the exact credential id, revision, access, target use,
Workspace, and allowed plaintext holder. The Session manifest never contains
plaintext. A Worker that loses its dispatch claim may not materialize, record a
receipt, or continue mutable Resource write-back.

### D6: Data ownership is enforced at composition time

Control owns authoring, publication, IAM, credential mutation, and Data Subject
consent/accountability stores.
Coordinator owns executable Agent registration, Deployment, DeploymentRun,
Environment execution state, Session, subject-tagged captured content, dispatch,
and commit stores. Resource
providers own their content. Worker owns only ephemeral execution state and must
not receive authority database connections.

The process store bundle is split into optional Control and Coordinator groups.
Split Coordinator cannot configure or acquire Catalog, Credential, Config,
Admin, or the Control seal key; split Control cannot configure or acquire
Session/Deployment or Resources content stores. `ResourceComponent` owns the
existing `ResourceCatalog` implementation together with its per-kind ports, so
Coordinator may mount the component without borrowing Control's Admin store.

Coordinator-to-Control calls cross one authenticated application boundary:
`ManagementAuditRepository` preserves the existing audit state machine,
`SessionCredentialSource` returns only secret-free credential access pins, and
`LifecycleFactDelivery` delivers the existing durable lifecycle-outbox fact.
AllInOne injects local implementations of the same ports. The HTTP adapter owns
serialization, authentication, and bounded idempotent retry only; it owns no
business state. Authentication precedes handler dispatch, so a rejected request
cannot mutate an authority before returning 401.

Coordinator model discovery is derived from
`ExecutableAgentInventorySource`, the same current immutable registrations used
for Session resolution. It does not reopen Control's mutable model or credential
catalogs merely to populate `/v1/models`.

Environment is Coordinator-owned. Public Managed HTTP, the authenticated private
Control command, and the AllInOne local adapter all invoke the same
`EnvironmentApplication::create`; the Registry atomically owns command-id /
fingerprint replay and the application converges one healthcheck through the Work
Queue. Control receives only `EnvironmentAuthor`, never `EnvironmentState` or an
Environment database address. `environment_db` remains an external deployment
field but resolves into the Coordinator store group.

Data Subject is Control-owned; captured runtime content is Coordinator-owned.
Control builds the only `RepoDataSubjectResolver`, persists its erasure-process
checkpoints, and owns the public consent/erasure API. Coordinator installs its
own durable captured-content adapter into Runtime and reads consent once per
attributed Run through `DataSubjectConsentSource`. The reverse erasure command
uses the authenticated private Coordinator boundary and invokes the one
Coordinator erasure application, which fans out to captured telemetry and the
configured portable ACP session store. Each adapter persists an erasure fence
and stable receipt, preventing both late-write resurrection and receipt loss on
an ambiguous retry. AllInOne replaces both HTTP adapters with the same local
ports; neither role opens the other's database.

## Implementation Status

The decision is implemented. The canonical component inventory, request flows,
failure semantics, and verification matrix live in
[Configuration-To-Application And Request-To-Response Flows](../design/config-to-run-execution-flow.md).
Keeping that evidence in one owner prevents the ADR and implementation guide
from becoming competing status ledgers.

The enforced boundary is G45 in [INVARIANTS](../INVARIANTS.md). In addition to
the adapter decision tables, `scripts/ci/check_crate_boundaries.py` verifies
that production Worker code cannot link or acquire a Control, Coordinator,
Credential, or Resource authority store.
The same fitness rule also rejects Control/Coordinator cross-owned store fields,
unconditional Resources migration by a Control role, and a Resources component
that loses ownership of `ResourceCatalog`.

## Consequences

- The same application ports support AllInOne and distributed deployment.
- `StoredPublication` remains the single publication authority; Coordinator data
  is explicitly rebuildable.
- No whole-catalog installation track survives beside per-Agent registration.
- Deployment recovery, publication retries, claimed commits, and mutable Resource
  write-back have stable idempotency or fencing identities.
- Resource semantics remain type-specific instead of accumulating optional
  behavior in a generic service.
- Split Coordinator no longer needs Control database credentials or the Control
  seal key; loss of the reverse Control boundary fails closed while durable
  Coordinator outbox/audit identities remain retryable.
- The service split requires boundary adapters, handlers, configuration checks,
  and tests, but no new domain model or general-purpose framework.

## References

- [Architecture overview](../design/architecture-overview.md)
- [Configuration-to-response flows](../design/config-to-run-execution-flow.md)
- [Publication lifecycle](../design/config-publication-lifecycle.md)
- [Managed Deployments](../design/managed-deployments.md)
- [Resources, Memory, Files, and Skills](../design/resources-memory-files-skills.md)
- [Credentials and Vaults](../design/credentials-and-vaults.md)
