# ADR-0071: Distributed Service Boundaries And Executable Agent Registration

- Status: Accepted
- Date: 2026-07-30
- Depends on: ADR-0031, ADR-0032, ADR-0038, ADR-0062, ADR-0063,
  ADR-0065, ADR-0066, ADR-0067
- Supersedes: ADR-0031 D1/D4 and ADR-0032 D1/D4 only where they require a
  process-local whole-catalog install as the publication-to-execution handoff

## Context

The implemented publication path persists a `StoredPublication` and then writes
the same `ExecutableAgentSnapshot` into a process-local `InstalledAgentCatalog`.
The implemented Deployment adapter likewise calls the ordinary Managed Session
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

AllInOne uses a local `ExecutableAgentRegistrar` adapter. A split deployment uses
`CoordinatorExecutableAgentClient` and `RegisterExecutableAgentHandler`. Startup
rehydration calls the same registrar; it does not maintain a second warm-install
path.

Publication success means the Coordinator acknowledged registration. A durable
publication may exist while registration is temporarily unavailable; that call
returns an availability failure and an idempotent retry registers the same
publication.

### D3: Deployment launch reuses the existing Session authority

`DeploymentSessionLauncher` remains the only Deployment-to-Session port. Its
request carries the existing stable `deployment_run_id`. AllInOne keeps a local
adapter; split deployment adds a Coordinator client and handler. Coordinator
Session creation is idempotent by DeploymentRun identity, so a transport retry
cannot create a second Session.

Deployment owns Deployment and DeploymentRun truth. Coordinator owns Session
creation, the frozen baseline, execution, and history after creation.

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

Control owns authoring and publication stores. Coordinator owns executable Agent
registration, Deployment, Session, dispatch, and commit stores. Resource
providers own their content. Worker owns only ephemeral execution state and must
not receive Control or Coordinator database connections.

## Consequences

- The same application ports support AllInOne and distributed deployment.
- `StoredPublication` remains the single publication authority; Coordinator data
  is explicitly rebuildable.
- No whole-catalog installation track survives beside per-Agent registration.
- Deployment retries, publication retries, claimed commits, and mutable Resource
  write-back have stable idempotency or fencing identities.
- Resource semantics remain type-specific instead of accumulating optional
  behavior in a generic service.
- The service split requires boundary adapters, handlers, configuration checks,
  and tests, but no new domain model or general-purpose framework.

## References

- [Architecture overview](../design/architecture-overview.md)
- [Configuration-to-response flows](../design/config-to-run-execution-flow.md)
- [Publication lifecycle](../design/config-publication-lifecycle.md)
- [Managed Deployments](../design/managed-deployments.md)
- [Resources, Memory, Files, and Skills](../design/resources-memory-files-skills.md)
- [Credentials and Vaults](../design/credentials-and-vaults.md)
