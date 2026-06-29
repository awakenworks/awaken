# Packaging Enforcement Matrix

This document turns boundary and licensing rules into package-level checks. The
goal is mechanical enforcement: imports, public APIs, names, and licenses should
make boundary violations visible before code review becomes subjective.

## Package Classes

Crate names, module layout, and per-crate authority are owned by
[D12 in key-design-decisions.md](key-design-decisions.md#d12---contract-names-follow-authority).
This table is the *enforcement* view — the dependency direction and naming rule
each class must satisfy — and does not redefine those crates.

| Package class | May depend on | Must not depend on | Naming rule |
|---|---|---|---|
| runtime core | runtime contracts, agent-domain stores, neutral extension traits | server routes, product adapters, config CRUD, publication coordinators, registry compilers, admin tools, execution drivers | no product hosting vocabulary |
| runtime contract/spec | neutral value objects, catalog install port, snapshot execution ports, capability query ports, schemas, conformance fixtures | product/admin/server implementation, config CRUD, publication coordinators, registry compilers, admin workflow | Apache-2.0 protocol/spec surface |
| runtime extension | runtime contracts and extension seams | server/admin/product internals | no concrete product protocol names |
| builtin tools extension | runtime tool/plugin contracts, environment adapter interfaces | runtime internals, admin registry | concrete tool ids allowed only here |
| config contract/domain | typed config records, config snapshots, publication values, registry compiler contracts | runtime loop internals, live control, product DTOs | config vocabulary, not hosted product vocabulary |
| run ingress/dispatch | runtime ports, stores, durable delivery internals | product DTOs in runtime request data | dispatch vocabulary |
| protocol adapter | server routes, runtime ports, projection stores | runtime internals, config write ports unless explicitly an admin/config API | public protocol vocabulary allowed only here |
| admin assistant tools | admin auth, config validation/publication services, private tool registry | ordinary agent catalogs | admin vocabulary required |
| orchestration layer above | executor/tool/backend adapter contracts, resource realization | runtime domain decisions, public protocol semantics | execution-adapter vocabulary |
| analytics/eval | committed facts/events, runtime ports for scenarios | runtime write internals | analytics vocabulary |

## Required Checks

| Check | Applies to | Failure caught |
|---|---|---|
| dependency direction | all packages | runtime imports server/product/admin/execution internals |
| public API surface | runtime contracts and runtime core | accidental new cross-boundary type |
| forbidden vocabulary | neutral packages | product hosting terms leaking into neutral code |
| concrete tool ownership | neutral runtime and contract packages | builtin tool ids, concrete tool symbols, or concrete `Tool` / `RawTool` implementations entering core |
| preferred tool API naming | neutral runtime and contract packages | `TypedTool` appears instead of `Tool` plus low-level `RawTool` |
| deferred work mechanism naming | neutral runtime and contract packages | `BackgroundTask` umbrella reappears instead of `ScheduledAction`, waiting state, or durable dispatch |
| SPDX/license metadata | runtime spec, examples, conformance | non-Apache terms on runtime protocol material |
| role catalog coverage | design docs | stable authority introduced without owner |
| ownership index | design/wiki docs | source doc added without retrieval owner |
| adapter conformance | protocol adapters | public DTO mapping drift |
| config/admin write denial | protocol runtime adapters | runtime protocol accidentally mutates config/admin state |
| snapshot contract surface | runtime contract/spec and server adapters | `AgentId` treated as complete executable config or runtime imports config CRUD/admin workflow |
| publication/install boundary | config domain and runtime contract/spec | publication coordinator or registry compiler enters runtime core, or runtime compiles config instead of installing complete data |

`scripts/ci/check_crate_boundaries.py` owns the initial arch hook for dependency
direction, neutral vocabulary, concrete builtin tool ownership, `Tool` /
`RawTool` implementation placement, `TypedTool` naming, `BackgroundTask`
umbrella prevention, and config-publication/compiler symbol leaks.

## Import Rules

```text
product/admin/protocol
  -> server/dispatch
  -> runtime ports/contracts
  -> agent-domain store contracts

config application/domain
  -> runtime catalog install port
  -> no runtime implementation internals

orchestration layer above
  -> executor/tool/backend contracts
  -> runtime ports only when acting as execution adapter

analytics/eval
  -> committed facts/events or normal runtime ports
```

The reverse direction is forbidden unless a design document adds a new boundary
and updates the enforcement matrix first.

## License Boundary

Runtime protocol/specification material, SDK-facing schemas, examples, and
conformance tests are Apache-2.0. Product, admin, hosted service, or deployment
packages may choose different package licenses only when they consume the runtime
protocol instead of redefining it.

## First Vertical Slice

1. Add package metadata for runtime spec and one extension package.
2. Add a dependency deny-list for runtime -> server/product/admin imports.
3. Add forbidden-vocabulary checks for neutral packages.
4. Add conformance fixtures for one public protocol adapter.
5. Add a negative test proving a runtime protocol request cannot call config/admin
   write ports.
6. Add inline/by-id snapshot contract tests proving both paths converge before
   execution.
7. Add a publication/install boundary test proving `ConfigPublicationCoordinator`
   and `RegistryCompiler` are not runtime-core dependencies.

## Guardrails

G2, G10, G14, G15, G16, G18, G19, G27, G28, and G29 in
[INVARIANTS](../INVARIANTS.md).
