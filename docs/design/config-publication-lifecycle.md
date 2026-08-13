# Configuration Publication And Executable Registration Lifecycle

This document owns the publication lifecycle only. The complete configuration,
Deployment, request, execution, and response sequences live in
[config-to-run-execution-flow.md](config-to-run-execution-flow.md). Service and
data ownership are decided by [ADR-0071](../adr/0071-distributed-service-boundaries-and-executable-agent-registration.md).

## Authority Boundary

Control owns mutable Agent configuration, source revisions, catalog/credential-
backed model-candidate resolution, compilation, and durable `StoredPublication`
history. Coordinator owns the rebuildable catalog used to resolve executable
Agents for new Sessions; it never reopens the model Catalog or Credential
repository.

```text
Control authority                         Coordinator projection

AgentConfig revision
  -> CatalogModelPublicationResolver
       (Catalog + Credential metadata + secret-free ACP/Worker capability facts)
  -> compile_published
  -> StoredPublication (durable)
  -> ExecutableAgentRegistrar.register
                                          -> ExecutableAgentCatalog
                                          -> current/exact/fingerprint reads
```

The operation is registration, not installation:

- `publish` creates durable Control-domain publication truth;
- `register` makes one exact publication available to Coordinator execution;
- `projection` describes the derived Coordinator data, but is not the API verb;
- `upsert` is the idempotent repository operation;
- `resolve` selects a registered exact snapshot for Session creation.

## Lifecycle

The labels below describe observable processing stages. They do not require a
second persisted status enum beside the existing Agent lifecycle and
`StoredPublication` state.

| Stage | Owner | Durable effect | Successor |
|---|---|---|---|
| authored | Control | versioned `AgentConfig` | validated or rejected |
| validated | Control | none | compiled or rejected |
| compiled | Control | immutable `ExecutableAgentSnapshot` candidate | published or rejected |
| published | Control | `StoredPublication` at one source revision and fingerprint | registration requested |
| registration requested | Control boundary adapter | none beyond the publication | registered or temporarily unavailable |
| registered | Coordinator | exact executable projection and monotonic current pointer | resolvable by new Sessions |
| unavailable | neither domain mutates its truth | retryable boundary failure | registration requested |
| superseded | Control and Coordinator retain exact history | a newer revision is current | terminal unless explicitly republished |

## Registration Contract

The target application port is:

```rust
#[async_trait]
pub trait ExecutableAgentRegistrar: Send + Sync {
    async fn register(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError>;

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError>;
}
```

`ExecutableAgentRegistration` is a transport command built from existing values,
not a new Agent domain model. It carries:

```text
workspace_id
agent_id
source_revision
ExecutableAgentSnapshot, including its fingerprint
AgentConfigView frozen from the same revision
```

Execution placement is deliberately absent: ADR-0073 derives it from the frozen
Environment and durable Worker capabilities.

The identity and outcomes are:

| Condition | Required outcome |
|---|---|
| identity is new and revision is current or newer | persist the exact snapshot and advance current |
| same identity and same fingerprint is retried | return the existing success |
| same identity carries a different fingerprint | reject as conflict |
| an older exact revision arrives | retain it as historical; never move current backwards |
| Coordinator is unreachable | keep `StoredPublication`; return availability failure |

Policy reconciliation performs a write-free preview first. If the fingerprint
already exists, it replays the exact publication and does not manufacture a
revision. If a catalog, credential, plugin, or Worker-capability dependency
changes the fingerprint, Control advances the unchanged authoring intent with
revision CAS and only then publishes/registers the new source revision. A
different fingerprint is therefore never persisted at an existing
`(Workspace, Agent, source_revision)` identity.

For retained data created before that fence, an exact replay first uses the
ordinary registrar. Only a semantic same-revision/different-fingerprint
conflict activates the same CAS revision advance; availability and storage
failures remain retries and never manufacture revisions.

The implemented private HTTP adapter posts the complete command to a fixed
registration route. Idempotency belongs to the application identity and
Coordinator state machine, not to an HTTP-method convention. Transport naming
is secondary to the port contract.

## Implemented Scope

### Reused unchanged

- `AgentConfig`, revision checks, publication compilation, and repository ports in
  `awaken-agent-config`, with unchanged `StoredPublication` persistence implemented by
  `awaken-config-store`;
- `ExecutableAgentSnapshot` and its fingerprinted resolved data;
- Catalog, credential-inventory, and Worker-observation ports; the Control
  resolver consumes them without acquiring a SecretStore or execution adapter;
- the existing publication reconciler trigger and durable publication history.

### Modified

- `ConfigService::publish` calls `ExecutableAgentRegistrar` after durable
  publication instead of directly mutating a process-local catalog;
- `CatalogModelPublicationResolver` lives only in `awaken-control`; ACP launch
  catalog rows are projected once into secret-free publication capabilities, so
  Control does not depend on Worker launch commands and Coordinator has no
  parallel resolver;
- the execution projection is now the Coordinator-owned
  `ExecutableAgentCatalog`; Runtime, Session, and Hand reads use that one
  catalog, while deletion safety uses the Resources-owned atomic reference
  index populated by the same registrar command path;
- startup rehydration uses the same `register` port instead of a separate local
  warm-install mechanism;
- the publish HTTP adapter distinguishes validation/conflict failures from a
  retryable Coordinator availability failure.

### New boundary code

- `ExecutableAgentRegistrar` with registration and withdrawal commands;
- `ExecutableAgentCatalog` as the one rebuildable execution projection;
- `LocalExecutableAgentRegistrar` for AllInOne composition;
- `HttpExecutableAgentRegistrar` and the authenticated
  `executable_agent_registration_router` for split deployment;
- `PostgresExecutableAgentRegistrar` and one scoped command-log migration; the
  log stores the existing commands and replays the canonical catalog state
  machine rather than defining a parallel projection model;
- role-aware composition and configuration: Control loads
  `coordinator_internal_url` plus an operator-projected registration token file,
  Coordinator mounts the authenticated durable router, and Worker rejects both
  registration credentials and authority database bindings;
- split Control and Coordinator return distinct public/private Routers and bind
  the private Router to the required `internal_bind`; the public Router contains
  no registration, Worker-observation, captured-content erasure, or reverse
  Control-service route, and no merged compatibility surface remains;
- `ReferenceIndexedExecutableAgentRegistrar`, projecting current Agent bindings
  into `ResourceReferenceIndex` before catalog exposure. Reclamation therefore
  uses the same atomic fence as Session and Extraction references and no longer
  performs a check-then-delete catalog scan.

There is no remaining registration boundary code. Periodic reconciliation is an
operational optimization; startup recovery and an explicit retry already reuse
the same authoritative registrar path.

No whole-catalog command, second publication model, generic RPC framework, or
parallel compatibility path is added.

Deployment and Session are both Coordinator-owned. Their local application seam
uses the stable `deployment_run_id`; no private launch endpoint, token, or second
Deployment aggregate remains.

## Recovery And Failure Rules

Control never rolls back a durable publication because a remote registration
attempt failed. A retry submits the same immutable identity. Startup or periodic
reconciliation reads published revisions and calls the same registrar; it does
not bypass the normal handler or write Coordinator storage directly.

Recovery isolates malformed historical rows per publication. Valid rows still
reach the registrar; each invalid row remains immutable and contributes to the
projection-degraded signal and bounded retry. Projection degradation does not
remove an otherwise healthy HTTP process from Service endpoints. Coordinator's
existing exact/current projection remains last-known-good until a valid newer
registration succeeds.

Coordinator must acknowledge persistence before Control reports the publication
as executable. Existing Sessions keep their frozen snapshot. A newer
registration affects only future resolution unless an explicit Session command
changes a frozen baseline.

## Verification

The cause/effect design for implementation tests is:

| Rule | Causes | Effects |
|---|---|---|
| R1 | valid new publication; Coordinator available | Control persists once; Coordinator registers once; API reports executable |
| R2 | identical retry | no duplicate row; same registration outcome |
| R3 | same identity; different fingerprint | conflict; current remains unchanged |
| R4 | older revision arrives after newer revision | exact revision remains readable; current does not move backwards |
| R5 | Coordinator unavailable after Control persistence | publication remains durable; API reports retryable unavailability |
| R6 | reconciliation after R5 | the same registrar is reused, no parallel path is created, and the R1 outcome is reached |
| R7 | one malformed historical row plus valid rows | valid rows register; invalid history is retained and reported degraded; serving readiness remains true |
| R8 | policy preview changes fingerprint at revision N | CAS creates revision N+1 before persistence and registration; an exact replay creates no revision |

Test comments must cite the applicable rule and preserve its causes and effects.

## Guardrails

G3, G4, G18, G23, G28, and G29 in [INVARIANTS](../INVARIANTS.md).
