---
type: Fact Index
title: Neutral Waist Facts
description: Cross-document facts for runtime execution ports and the data-only config edge.
tags: [neutral-waist, runtime, ports, config-edge]
timestamp: 2026-06-25T00:00:00+08:00
---

# Neutral Waist Facts

Owner: [neutral-waist.md](../design/neutral-waist.md).

## FACT-WAIST-001: Runtime ports are the approved waist

- Status: active
- Owner: [Neutral waist core ports](../design/neutral-waist.md#core-ports)
- Fact: adapters enter runtime through the named execution ports; protocol DTOs, route state, and execution driver details are translated before the call.
- Links: guardrail G2; [architecture overview](../design/architecture-overview.md)
- Verification: dependency-direction checks, public API tests, and backend capability tests.

## FACT-WAIST-002: Config crosses as data and fingerprints

- Status: active
- Owner: [Neutral waist data edge](../design/neutral-waist.md#data-only-config-edge)
- Fact: the config domain prepares resolved serializable input and a catalog fingerprint; runtime rebuilds live execution objects from its own catalog.
- Links: guardrails G3 and G4
- Verification: serde roundtrip, catalog fingerprint mismatch, and dependency-grep checks.

## FACT-WAIST-003: Backend requirements fail before execution

- Status: active
- Owner: [Neutral waist backend requirements](../design/neutral-waist.md#backend-requirements)
- Fact: backend features are checked as advertised capabilities before a run starts, so unsupported continuation, tool roundtrip, or decision surfaces fail with typed errors.
- Links: [tool facts](tool-and-capability-facts.md)
- Verification: backend profile negotiation tests.

## FACT-WAIST-004: Goal continuation is an extension

- Status: active
- Owner: [Neutral waist goal continuation](../design/neutral-waist.md#goal-continuation)
- Fact: runtime stores opaque continuation verdicts and terminal conclusions; grading semantics and public outcome names live outside runtime.
- Links: guardrail G11; [product protocol facts](anthropic-alignment-and-sessions-facts.md)
- Verification: extension boundary tests and replay tests using recorded verdicts.
