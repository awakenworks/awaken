---
type: Fact Index
title: Product Protocol And Session Facts
description: Cross-document facts for product adapters, sessions, events, and outcome mapping.
tags: [product-adapter, sessions, events, outcomes]
timestamp: 2026-06-25T00:00:00+08:00
---

# Product Protocol And Session Facts

Owner: [anthropic-alignment-and-sessions.md](../design/anthropic-alignment-and-sessions.md).

## FACT-ALIGN-001: Public protocol names stop at adapters

- Status: active
- Owner: [Anti-corruption layer](../design/anthropic-alignment-and-sessions.md#anti-corruption-layer)
- Fact: public DTO names, event names, and product status names are translated at the adapter boundary and do not enter runtime events or errors.
- Links: guardrail G10
- Verification: runtime deny-list checks and contract snapshot tests.

## FACT-ALIGN-002: Sessions are public projections

- Status: active
- Owner: [Sessions](../design/anthropic-alignment-and-sessions.md#sessions)
- Fact: public session records are projections over committed runtime and server facts; adapters do not invent independent authority. Internal profiled modes and mutation policy never enter ordinary Managed Agents DTOs, ordinary metadata-backed replay remains compatible, and typed system-prompt selection preserves omitted, explicit-null, and exact-value round trips.
- Links: guardrails G1, G10, and G13
- Verification: projection ordering, legacy Managed serialization/fingerprint, prompt round-trip, and store-truth tests.

## FACT-ALIGN-003: Outcome mapping is not runtime semantics

- Status: active
- Owner: [Outcome and goal mapping](../design/anthropic-alignment-and-sessions.md#outcome--goal-mapping)
- Fact: public outcome labels map onto opaque runtime verdicts through an extension or adapter; runtime does not interpret product-specific success categories.
- Links: guardrail G11
- Verification: extension boundary tests and replay tests.

## FACT-ALIGN-004: The first product slice is projection-only

- Status: active
- Owner: [First vertical slice](../design/anthropic-alignment-and-sessions.md#first-vertical-slice)
- Fact: build the product edge by translating requests, calling runtime/server ports, and projecting committed results before adding broader public API surface.
- Links: guardrails G10, G13, and G14
- Verification: contract snapshots and docs review checklist.

## FACT-ALIGN-005: Typed profiled Runs reuse canonical Session admission

- Status: active
- Owner: [ADR-0075 profiled Run amendment](../adr/0075-unified-managed-session-worker-execution.md#amendment-2026-08-29-profiled-run-submission-is-a-typed-leaf-over-canonical-admission)
- Fact: the private typed profiled Run leaf lowers into the sole Session admission, durable Run dispatch, and committed Managed projection authorities. Its complete command fingerprint is computed before mutable Runtime projection; it adds no Event fallback, second Run store, lifecycle, or product status.
- Links: guardrails G1, G10, and G13
- Verification: route-lowering tests, current/legacy fingerprint decision tables on Memory, SQLite, and PostgreSQL, real ManagedHost phase tests, and committed projection tests.
