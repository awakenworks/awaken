# ADR-0002: Resolver Role Demarcation

- Status: Proposed
- Depends on: ADR-0001
- Supersedes: none

## Context

The corpus names several roles ending in `Resolver`. Three are *distinct
canonical roles* operating in different domains, but the shared `Resolver` root
hides that distinction, and a single concrete type may implement more than one of
them. This repeatedly caused design docs to conflate the roles — e.g.
`architecture-overview.md` listing `AgentResolver` in the gated runtime/server
port (where it does not belong), and `config-to-run-execution-flow.md` naming
both `RunResolver` and `AgentResolver` for the resolution stage without saying
which does what.

(The reference implementation confirms one type can span two roles — its registry
resolver satisfies both the agent-lookup and plan-resolution roles — which is why
the names alone are not enough to keep them apart.)

## Decision

### D1: Three canonical resolver roles, each defined by input → output and layer

| Role | Input → Output | Layer (this corpus) |
|---|---|---|
| `AgentResolver` | `AgentId` → `ResolvedAgent` (spec lookup inside the execution loop) | registry, runtime core |
| `Resolver` | `ResolutionRequest` → `ResolvedRun` (turn a registry target into an executable plan) | resolution, runtime core |
| `RunResolver` | `RunActivation` + scope → `ResolvedRun` (host-layer materialization) | run-ingress / host |

Documents and call sites reference the **role**, not "the resolver".

### D2: A type may implement several roles, but each impl declares which

A concrete type may satisfy more than one role (a registry resolver, for example,
can implement both `AgentResolver` and `Resolver`), but each implementation states
which role it serves; the roles stay conceptually distinct. Auxiliary `*Resolver`
helpers compose one of the three roles — they are not new roles.

### D3 (open — needs consensus): domain-self-evident renaming

The shared root still forces readers to inspect impls to pick a role. A follow-up
ADR may rename to self-evident names after team review; candidates:
`AgentResolver` → `AgentSpecRegistry`, `Resolver` → `RunPlanResolver`,
`RunResolver` → `IngressRunResolver`. Not adopted here to avoid churn before
consensus; the D1 demarcation is usable immediately without renaming.

## Consequences

- The gated-port and seam lists stop inventing an `AgentResolver` server port;
  each resolver mention resolves to one of the three D1 roles.
- A new `*Resolver` helper must declare which canonical role it composes.
- Naming is intentionally left open; if D3 is adopted, this ADR is amended or
  superseded with the rename and a migration note.

## References

- [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md)
  (run resolution stage), [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md)
  (role catalog).
- ADR-0001 D3 (internal vocabulary consistency).
