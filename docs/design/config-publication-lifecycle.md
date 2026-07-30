# Configuration Publication And Executable Registration Lifecycle

This document owns the publication lifecycle only. The complete configuration,
Deployment, request, execution, and response sequences live in
[config-to-run-execution-flow.md](config-to-run-execution-flow.md). Service and
data ownership are decided by [ADR-0071](../adr/0071-distributed-service-boundaries-and-executable-agent-registration.md).

## Authority Boundary

Control owns mutable Agent configuration, source revisions, compilation, and
durable `StoredPublication` history. Coordinator owns the rebuildable catalog
used to resolve executable Agents for new Sessions.

```text
Control authority                         Coordinator projection

AgentConfig revision
  -> compile_published
  -> StoredPublication (durable)
  -> ExecutableAgentRegistrar.register
                                          -> ExecutableAgentCatalog.upsert
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
}
```

`ExecutableAgentRegistration` is a transport command built from existing values,
not a new Agent domain model. It carries:

```text
workspace_id
agent_id
source_revision
ExecutableAgentSnapshot, including its fingerprint
```

The identity and outcomes are:

| Condition | Required outcome |
|---|---|
| identity is new and revision is current or newer | persist the exact snapshot and advance current |
| same identity and same fingerprint is retried | return the existing success |
| same identity carries a different fingerprint | reject as conflict |
| an older exact revision arrives | retain it as historical; never move current backwards |
| Coordinator is unreachable | keep `StoredPublication`; return availability failure |

A natural remote representation is an idempotent `PUT` keyed by Workspace,
Agent, and source revision. Transport naming is secondary to the port contract.

## Existing, Modified, And New Scope

### Reused unchanged

- `AgentConfig`, revision checks, publication compilation, and
  `StoredPublication` persistence in `awaken-config-service` and
  `awaken-config-store`;
- `ExecutableAgentSnapshot` and its fingerprinted resolved data;
- current, exact-revision, and fingerprint lookup semantics already represented
  by `InstalledAgentCatalog`;
- the existing publication reconciler trigger and durable publication history.

### Modified

- `ConfigService::publish` calls `ExecutableAgentRegistrar` after durable
  publication instead of directly mutating a process-local catalog;
- `InstalledAgentCatalog` becomes the Coordinator-owned
  `ExecutableAgentCatalog` backed by an `ExecutableAgentCatalogRepository`;
- startup rehydration uses the same `register` port instead of a separate local
  warm-install mechanism;
- the publish HTTP adapter distinguishes validation/conflict failures from a
  retryable Coordinator availability failure.

### New boundary code

- `ExecutableAgentRegistrar`;
- `CoordinatorExecutableAgentClient`;
- `RegisterExecutableAgentHandler`;
- the persistence adapter behind `ExecutableAgentCatalogRepository`;
- a local registrar adapter for AllInOne composition.

No whole-catalog command, second publication model, generic RPC framework, or
parallel compatibility path is added.

## Recovery And Failure Rules

Control never rolls back a durable publication because a remote registration
attempt failed. A retry submits the same immutable identity. Startup or periodic
reconciliation reads published revisions and calls the same registrar; it does
not bypass the normal handler or write Coordinator storage directly.

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

Test comments must cite the applicable rule and preserve its causes and effects.

## Guardrails

G3, G4, G18, G23, G28, and G29 in [INVARIANTS](../INVARIANTS.md).
