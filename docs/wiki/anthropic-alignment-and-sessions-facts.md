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
- Fact: public session records are projections over committed runtime and server facts; adapters do not invent independent authority.
- Links: guardrails G1, G10, and G13
- Verification: projection ordering tests and store truth review.

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
