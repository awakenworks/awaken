---
type: Fact Index
title: Runtime Interface Boundary Facts
description: Cross-document facts for runtime role traits, executable snapshots, plugin contributions, tool decisions, and simple design.
tags: [runtime, interfaces, boundaries, snapshots, plugins, simple-design]
timestamp: 2026-06-27T00:00:00+08:00
---

# Runtime Interface Boundary Facts

Owner: [runtime-interface-boundaries.md](../design/runtime-interface-boundaries.md).

## FACT-BOUNDARY-001: Runtime seam is split by authority

- Status: active
- Owner: [Role split](../design/runtime-interface-boundaries.md#role-split)
- Fact: runtime execution, live control, resolution, commit exposure, and delivery are separate roles with distinct authority.
- Links: guardrails G2, G5, and G14
- Verification: public API surface tests, `RunIngressCapabilities` tests, and docs review checklist.

## FACT-BOUNDARY-002: Durable execution requests are data-only

- Status: active
- Owner: [Boundary matrix](../design/runtime-interface-boundaries.md#boundary-matrix)
- Fact: durable-ingress-to-runtime execution data crosses as `RunExecutionRequest`; live handles and commit wiring travel separately through execution context and binding roles.
- Links: guardrails G1, G3, G5, and G6
- Verification: dependency checks and durable ingress tests.

## FACT-BOUNDARY-003: Plugins contribute through resolved contributions

- Status: active
- Owner: [Plugin contribution matrix](../design/runtime-interface-boundaries.md#plugin-contribution-matrix)
- Fact: a plugin is a factory that declares a manifest and a `CapabilityBound`, then resolves to `Contributions`; all plugin behavior enters through those resolved state, hook, gate, tool, transform, action, effect, or lifecycle contributions, which must stay within the declared bound.
- Links: guardrails G8, G9, G14, and G30
- Verification: duplicate-owner tests, hook filter tests, and no-bypass permission tests.

## FACT-BOUNDARY-004: Tool decisions are a ladder

- Status: active
- Owner: [Tool decision ladder](../design/runtime-interface-boundaries.md#tool-decision-ladder)
- Fact: descriptor visibility, invocation authorization, execution location, and committed side effects are separate decisions with explicit intersections.
- Links: guardrails G8 and G9; [tool facts](tool-and-capability-facts.md)
- Verification: visibility policy tests, permission tests, and tool execution lifecycle tests.

## FACT-BOUNDARY-005: Simple design requires executable boundaries

- Status: active
- Owner: [Simple design evaluation](../design/runtime-interface-boundaries.md#simple-design-evaluation)
- Fact: a runtime interface remains simple only when each new role owns distinct authority, has tests, reveals intention, avoids duplicated policy, and is smaller than the abstraction it replaces.
- Links: guardrail G14; [key design decisions](../design/key-design-decisions.md)
- Verification: docs review checklist and public API/dependency checks.

## FACT-BOUNDARY-006: Role catalogs are source-owned maintenance surfaces

- Status: active
- Owner: [Role catalog](../design/runtime-interface-boundaries.md#role-catalog)
- Fact: stable runtime roles are documented in the design document that owns their authority; the wiki indexes the fact and Rustdoc owns API signatures.
- Links: [design corpus guide](../README.md#role-catalog-and-state-machine-rule); guardrail G14
- Verification: docs review checklist and ownership index checks.

## FACT-BOUNDARY-007: Contract names follow authority

- Status: active
- Owner: [D12 - Contract Names Follow Authority](../design/key-design-decisions.md#d12---contract-names-follow-authority)
- Fact: crates express layer and dependency direction, modules express domain context, and type names stay short; ambiguity is resolved by paths such as `agent::stream::Event` and aliases such as `StreamEvent`.
- Links: guardrails G1, G2, G5, G10, and G13
- Verification: dependency checks, public API checks, store conformance tests, event capture tests, and protocol replay tests.

## FACT-BOUNDARY-016: Runtime axes point to an owning lifecycle type

- Status: active
- Owner: [Axis lifecycle catalog](../design/runtime-interface-boundaries.md#axis-lifecycle-catalog)
- Fact: each runtime axis names the workspace type that owns its lifecycle and the canonical ADR; states, transitions, and enforcing tests live in that owner, not restated in the catalog.
- Links: guardrails G1, G5, G13, and G14
- Verification: lifecycle state tests in the owning type, dependency checks, and idempotency tests.

## FACT-BOUNDARY-008: Builtin tools are extension contributions

- Status: active
- Owner: [Tool decision ladder](../design/runtime-interface-boundaries.md#tool-decision-ladder)
- Fact: official first-party tools join the tool ladder as `awaken-ext-builtin-tools` plugin contributions; runtime core starts with no concrete model-callable tool ids.
- Links: [tool facts](tool-and-capability-facts.md); [D14](../design/key-design-decisions.md#d14---concrete-tool-ids-live-outside-runtime-core)
- Verification: plugin registration tests, runtime dependency checks, and catalog visibility tests.

## FACT-BOUNDARY-009: Snapshot contract exposes execution and inspection

- Status: active
- Owner: [Snapshot execution and inspection contract](../design/runtime-interface-boundaries.md#snapshot-execution-and-inspection-contract)
- Fact: runtime-facing snapshot APIs execute inline snapshots, resolve snapshot ids, list executable snapshots, expose capability facts, and validate plugin config without owning config CRUD.
- Links: [D19](../design/key-design-decisions.md#d19---executable-snapshot-is-the-run-configuration-identity); guardrail G28
- Verification: inline snapshot tests, by-id snapshot resolution tests, capability snapshot tests, and dependency checks.

## FACT-BOUNDARY-010: Agent id is not enough to identify executable configuration

- Status: active
- Owner: [Snapshot execution and inspection contract](../design/runtime-interface-boundaries.md#snapshot-execution-and-inspection-contract)
- Fact: the same agent id may be backed by different executable snapshots across runs or threads, so execution validates snapshot data and catalog fingerprint before starting.
- Links: [config flow facts](config-to-run-execution-flow-facts.md); guardrails G3, G4, and G28
- Verification: snapshot fingerprint tests, resolver negative tests, and run activation contract tests.

## FACT-BOUNDARY-011: Runtime receives catalog installs, not publication services

- Status: active
- Owner: [Publication roles outside runtime](../design/runtime-interface-boundaries.md#publication-roles-outside-runtime)
- Fact: publication coordination, registry compilation, and durable publication identity remain outside the runtime role catalog; runtime validates and swaps only `RuntimeCatalogInstall`.
- Links: [D20](../design/key-design-decisions.md#d20---publication-coordination-is-outside-runtime); [config flow facts](config-to-run-execution-flow-facts.md); guardrails G18, G23, and G29
- Verification: install rollback tests, dependency-direction checks, and runtime public API surface tests.

## FACT-BOUNDARY-012: Runtime configuration surface is minimal

- Status: active
- Owner: [Minimal runtime configuration surface](../design/runtime-interface-boundaries.md#minimal-runtime-configuration-surface)
- Fact: runtime configuration operations are limited to catalog install, snapshot execution, snapshot lookup/listing, capability reporting, plugin config validation, and ordinary activation execution.
- Links: [config flow facts](config-to-run-execution-flow-facts.md); guardrails G14, G18, G28, and G29
- Verification: public API surface tests, dependency checks, and capability snapshot tests.

## FACT-BOUNDARY-013: Runtime calls cross axes before commit

- Status: active
- Owner: [Axis descriptions](../design/runtime-interface-boundaries.md#axis-descriptions)
- Fact: hook, tool, and model calls start in the execution axis, may stage state or event candidates, and become durable runtime truth only through the commit axis.
- Links: [runtime behavior facts](runtime-behavior-facts.md); [commit taxonomy](../design/commit-fact-projection-taxonomy.md#runtime-call-staging-and-commit-layers); guardrails G1, G10, and G13
- Verification: staged command validation, event staging, checkpoint atomicity, and projection ordering tests.

## FACT-BOUNDARY-014: Activation data and runtime context are separate

- Status: active
- Owner: [Activation versus runtime context](../design/runtime-interface-boundaries.md#activation-versus-runtime-context)
- Fact: `RunActivation` carries immutable neutral run input, `RuntimeRunContext` carries per-attempt live wiring, and `ExecutableAgentSnapshot` carries immutable executable configuration.
- Links: [D21](../design/key-design-decisions.md#d21---activation-data-and-runtime-context-are-separate); [config flow facts](config-to-run-execution-flow-facts.md); guardrails G2, G3, G5, and G13
- Verification: activation/context split tests, durable request serialization tests, replay tests, and same-source commit wiring tests.

## FACT-BOUNDARY-015: Broad runtime facades hide authority

- Status: active
- Owner: [Broad interface smell](../design/runtime-interface-boundaries.md#broad-interface-smell)
- Fact: a single facade that combines run execution, resolution, live control, capability queries, registry exposure, and commit-source access should be split into the narrow runtime ports that match each axis.
- Links: [D18](../design/key-design-decisions.md#d18---runtime-axes-stay-separate); guardrails G2, G14, and G18
- Verification: public API surface tests, dependency-direction checks, and role catalog review.
