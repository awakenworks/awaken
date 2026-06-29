---
type: Fact Index
title: Tool and Capability Facts
description: Cross-document facts for descriptors, capability checks, and permission boundaries.
tags: [tools, capabilities, permissions, descriptors]
timestamp: 2026-06-27T00:00:00+08:00
---

# Tool And Capability Facts

Owner: [tool-and-capability.md](../design/tool-and-capability.md).

## FACT-TOOL-001: Capabilities are segmented by responsibility

- Status: active
- Owner: [Capability segments](../design/tool-and-capability.md#capability-segments)
- Fact: descriptors, execution behavior, operator policy, secrets, and session data are separate segments with different owners.
- Links: guardrail G8
- Verification: config materialization and no-secret serialization tests.

## FACT-TOOL-002: Tool visibility grants perception only

- Status: active
- Owner: [Tool model](../design/tool-and-capability.md#tool-model)
- Fact: making a tool visible to the model does not authorize invocation; execution still passes through policy and capability checks.
- Links: guardrail G9; [credentials facts](credentials-and-vaults-facts.md)
- Verification: type/API checks and permission tests showing explicit authorization.

## FACT-TOOL-003: Capability checks start from named refs

- Status: active
- Owner: [Backend and tool capability checks](../design/tool-and-capability.md#backend-and-tool-capability-checks)
- Fact: runtime validates selected model providers, backends, and tools; it does not search for an arbitrary compatible model provider inside the run path.
- Links: guardrail G9
- Verification: capability negotiation tests and review for absence of grant fields.

## FACT-TOOL-004: The first slice proves replayable descriptors

- Status: active
- Owner: [Tool and capability](../design/tool-and-capability.md)
- Fact: new capability types start with descriptor data, fingerprinting, runtime validation, invocation by id, and an explicit permission hook.
- Links: guardrails G8 and G14
- Verification: descriptor hash tests, replay tests, and docs review checklist.

## FACT-TOOL-005: Official concrete tool ids come from builtin-tools

- Status: active
- Owner: [Official builtin tools extension](../design/tool-and-capability.md#official-builtin-tools-extension)
- Fact: runtime core provides no concrete model-callable tool ids; official hand, task, and delegation tools enter through `awaken-ext-builtin-tools`.
- Links: [D14](../design/key-design-decisions.md#d14---concrete-tool-ids-live-outside-runtime-core); guardrails G2, G8, and G9
- Verification: dependency/API checks proving runtime core has no concrete tool ids, plugin registration tests, and permission no-bypass tests.

## FACT-TOOL-006: Delegation uses one agent_run tool

- Status: active
- Owner: [Official builtin tools extension](../design/tool-and-capability.md#official-builtin-tools-extension)
- Fact: sub-agent invocation uses one `agent_run` tool with an `agent_id` argument; generated ids such as `agent_run_<agent_id>` are not runtime tool ids.
- Links: guardrails G8, G9, and G14
- Verification: catalog tests for a single descriptor, fingerprint tests for allowed target lists, and fail-closed invocation tests for unknown `agent_id` values.

## FACT-TOOL-007: Admin assistant tools are private server tools

- Status: active
- Owner: [Admin assistant tool boundary](../design/tool-and-capability.md#admin-assistant-tool-boundary)
- Fact: admin assistant tools may implement the runtime `Tool` trait, but they live in `awaken-admin-assistant-tools` and a private registry, not in `awaken-ext-builtin-tools`.
- Links: [D15](../design/key-design-decisions.md#d15---admin-assistant-tools-are-server-owned); guardrails G2 and G9
- Verification: capabilities tests proving admin tools are hidden from ordinary agent catalogs, auth/audit tests, and dependency checks.

## FACT-TOOL-008: Tool execution stays behind a neutral port

- Status: active
- Owner: [Tool model](../design/tool-and-capability.md#tool-model)
- Fact: runtime invokes tools through neutral `Tool` / `ToolExecutor` contracts; host hand, MCP, remote, and client-executed tool protocols implement those contracts on the execution side.
- Links: [runtime interface facts](runtime-interface-boundaries-facts.md); guardrails G9 and G14
- Verification: tool executor adapter tests, dependency checks, idempotency/correlation tests for remote adapters, and no-direct-store-write tests.
