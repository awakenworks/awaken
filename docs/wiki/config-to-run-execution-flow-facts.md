---
type: Fact Index
title: Config To Run Execution Flow Facts
description: Cross-document facts for configuration publication, executable snapshot selection, run activation, resolution, execution, and commit handoffs.
tags: [runtime, config, snapshots, resolution, execution, commit]
timestamp: 2026-06-27T00:00:00+08:00
---

# Config To Run Execution Flow Facts

Owner: [config-to-run-execution-flow.md](../design/config-to-run-execution-flow.md).

## FACT-FLOW-001: Configuration publishes before runtime execution

- Status: active
- Owner: [Flow contract](../design/config-to-run-execution-flow.md#flow-contract)
- Fact: config records are loaded into a config snapshot and published as a registry set before executable snapshot selection, activation, and execution consume them.
- Links: guardrails G2, G3, and G4; [architecture overview](../design/architecture-overview.md)
- Verification: config publication tests, fingerprint mismatch tests, and dependency checks.

## FACT-FLOW-002: Runtime has a configuration publication axis

- Status: active
- Owner: [Runtime configuration axis](../design/config-to-run-execution-flow.md#runtime-configuration-axis)
- Fact: runtime behavior depends on a configuration publication axis, but config authoring, publication coordination, and registry compilation stay config-side; runtime consumes installed catalog versions and fingerprints through install and resolver ports.
- Links: guardrails G2, G3, G4, G14, G18, and G29; [runtime interface facts](runtime-interface-boundaries-facts.md)
- Verification: role catalog checks, publication transaction tests, and dependency checks.

## FACT-FLOW-003: Run parsing ends at RunActivation

- Status: active
- Owner: [Stage 4 - Run Activation](../design/config-to-run-execution-flow.md#stage-4---run-activation)
- Fact: protocol adapters convert public payloads, resume decisions, and client-executed tools into neutral `RunActivation` data, while per-attempt handles travel through `RuntimeRunContext`; public DTOs and route state do not enter runtime core.
- Links: guardrails G2, G5, G6, and G10; [runtime interface facts](runtime-interface-boundaries-facts.md)
- Verification: route adapter tests, public API surface checks, and dependency deny-lists.

## FACT-FLOW-004: Resolution materializes the execution environment

- Status: active
- Owner: [Stage 5 - Run Resolution](../design/config-to-run-execution-flow.md#stage-5---run-resolution)
- Fact: resolution validates catalog data, resolves model/provider/plugin/tool state, and produces a resolved run environment without executing the loop.
- Links: guardrails G3, G4, G8, and G14
- Verification: resolver tests, descriptor fingerprint tests, plugin activation-scope tests, and catalog collision tests.

## FACT-FLOW-005: Execution commits through one boundary

- Status: active
- Owner: [Stage 8 - Commit And Projection](../design/config-to-run-execution-flow.md#stage-8---commit-and-projection)
- Fact: LLM and tool execution may emit live stream output, but durable runtime truth appears only after `CommitCoordinator` commits facts, events, messages, and state.
- Links: guardrails G1, G10, and G13; [runtime behavior facts](runtime-behavior-facts.md)
- Verification: commit atomicity tests, projection ordering tests, and replay tests from committed facts.

## FACT-FLOW-006: Neutral code avoids product hosting vocabulary

- Status: active
- Owner: [Naming boundary for config, admin, and product adapters](../design/config-to-run-execution-flow.md#naming-boundary-for-config-admin-and-product-adapters)
- Fact: neutral runtime, protocol, config, and ordinary extension code uses names such as `config`, `admin`, `run`, `thread`, and `activation`; product hosting terms such as `managed` are restricted to product adapters or explicit boundary mapping docs.
- Links: [D16](../design/key-design-decisions.md#d16---neutral-code-avoids-product-hosting-vocabulary), guardrail G16
- Verification: grep/deny-list checks over neutral crates and adapter boundary review.

## FACT-FLOW-007: Runtime primary axes stay separate

- Status: active
- Owner: [Primary runtime axes](../design/runtime-interface-boundaries.md#primary-runtime-axes)
- Fact: configuration publication, live control, and execution are separate primary runtime-facing axes; activation, resolution, state, event, wait/resume, commit, and extension are supporting axes that connect them without merging authority.
- Links: [D18](../design/key-design-decisions.md#d18---runtime-axes-stay-separate), guardrail G18
- Verification: dependency-direction checks, public API surface tests, adapter boundary tests, and role catalog checks.

## FACT-FLOW-008: Executable snapshots bridge config data and runtime activation

- Status: active
- Owner: [Snapshot execution and inspection contract](../design/config-to-run-execution-flow.md#snapshot-execution-and-inspection-contract)
- Fact: a run may start from inline executable snapshot data or from a snapshot id that resolves to the same immutable data before activation and resolution continue.
- Links: [runtime interface facts](runtime-interface-boundaries-facts.md); [D19](../design/key-design-decisions.md#d19---executable-snapshot-is-the-run-configuration-identity); guardrail G28
- Verification: inline/by-id execution tests, resolver miss tests, and capability mismatch tests.

## FACT-FLOW-009: Configuration surfaces use read and validation ports

- Status: active
- Owner: [Snapshot execution and inspection contract](../design/config-to-run-execution-flow.md#snapshot-execution-and-inspection-contract)
- Fact: configuration surfaces list snapshots, read runtime capability facts, and validate plugin config through runtime-facing ports instead of asking runtime to store or publish config.
- Links: [runtime interface facts](runtime-interface-boundaries-facts.md); guardrails G14 and G28
- Verification: screen API surface tests, dependency checks, and plugin schema validation tests.

## FACT-FLOW-010: Publication coordination and compilation are config-side

- Status: active
- Owner: [Publication objects are outside runtime](../design/config-to-run-execution-flow.md#publication-objects-are-outside-runtime)
- Fact: `ConfigPublicationCoordinator` orders a config-side publish transaction, `RegistryCompiler` produces the publication data, and runtime only receives a complete catalog install request through `RuntimeCatalogInstaller`.
- Links: [D20](../design/key-design-decisions.md#d20---publication-coordination-is-outside-runtime); [runtime interface facts](runtime-interface-boundaries-facts.md); guardrails G18, G23, and G29
- Verification: dependency-direction checks, runtime public API checks, and publication/install transaction tests.

## FACT-FLOW-011: Runtime configuration operations do not author config

- Status: active
- Owner: [Snapshot execution and inspection contract](../design/config-to-run-execution-flow.md#snapshot-execution-and-inspection-contract)
- Fact: runtime configuration operations install complete catalogs, execute snapshots, resolve/list snapshots, report capabilities, and validate plugin sections, but they do not create drafts or mutate config records.
- Links: [runtime interface facts](runtime-interface-boundaries-facts.md); guardrails G18, G28, and G29
- Verification: public API surface tests, dependency checks, and config/admin write-denial tests.

## FACT-FLOW-012: Runtime context is recreated, not persisted

- Status: active
- Owner: [Stage 4 - Run Activation](../design/config-to-run-execution-flow.md#stage-4---run-activation)
- Fact: durable delivery persists activation data or references and later recreates `RuntimeRunContext` wiring; process-local handles are not part of executable snapshot identity or durable activation data.
- Links: [D21](../design/key-design-decisions.md#d21---activation-data-and-runtime-context-are-separate); [runtime interface facts](runtime-interface-boundaries-facts.md); guardrails G3, G5, and G13
- Verification: durable request serde tests, replay construction tests, context reconstruction tests, and handle-leak dependency checks.

## FACT-FLOW-013: Config graph is explicit before publication

- Status: active
- Owner: [Config graph model](../design/config-to-run-execution-flow.md#config-graph-model)
- Fact: `ModelProviderSpec`, `ModelSpec`, `ModelPoolSpec`, `AgentSpec`, tool specs, skill specs, and plugin refs form a versioned config graph that `RegistryCompiler` validates before runtime catalog install; future integration evidence stays outside existing specs until it has a tested authority boundary.
- Links: [D22](../design/key-design-decisions.md#d22---config-graph-is-explicit-before-runtime); [binding facts](runtime-explicit-boundaries-facts.md); guardrails G3, G4, G22, and G29
- Verification: config graph validation tests, missing-reference tests, descriptor fingerprint tests, and publication rollback tests.

## FACT-FLOW-014: Agent specs assemble behavior by reference

- Status: active
- Owner: [Spec responsibilities](../design/config-to-run-execution-flow.md#spec-responsibilities)
- Fact: `AgentSpec` references model selection, tools, skills, plugins, resources, instructions, and requirements, but it does not own provider credentials, tool implementations, concrete launch or endpoint fields, live registries, or public protocol state.
- Links: [D22](../design/key-design-decisions.md#d22---config-graph-is-explicit-before-runtime); [tool facts](tool-and-capability-facts.md); guardrails G3, G8, G9, and G22
- Verification: agent spec serde tests, config graph validation tests, no-secret tests, and tool visibility/permission separation tests.
