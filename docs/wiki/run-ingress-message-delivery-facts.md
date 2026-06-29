---
type: Fact Index
title: Run Ingress And Message Delivery Facts
description: Cross-document facts for direct and durable run ingress, pending input, message delivery, and recovery.
tags: [dispatch, durable-ingress, message-delivery, recovery]
timestamp: 2026-06-25T00:00:00+08:00
---

# Run Ingress And Message Delivery Facts

Owner: [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md). This design
owns run ingress, durable dispatch, pending input, message delivery, and recovery
guidance.

## FACT-DISPATCH-001: Run ingress has direct and durable forms

- Status: active
- Owner: [Run ingress](../design/run-ingress-message-delivery.md#run-ingress)
- Fact: direct runtime control and durable buffered ingress are distinct entry shapes; durable-only operations must reject direct ingress.
- Links: guardrail G5
- Verification: `RunIngressCapabilities` tests.

## FACT-DISPATCH-002: Durable ingress behavior is additive

- Status: active
- Owner: [Durable ingress internal responsibilities](../design/run-ingress-message-delivery.md#durable-ingress-internal-responsibilities)
- Fact: input buffering, claims, recovery, and scheduling are internal responsibilities layered over runtime control, not public runtime extension points or stable route dependencies.
- Links: guardrail G6
- Verification: API surface tests and route tests covering both ingress modes.

## FACT-DISPATCH-003: Execution placement belongs outside runtime

- Status: active
- Owner: [Execution placement boundary](../design/run-ingress-message-delivery.md#execution-placement-boundary)
- Fact: execution placement and process mechanics are owned by the orchestration layer above; runtime sees backend/tool ports.
- Links: [resources facts](resources-memory-files-skills-facts.md)
- Verification: dependency checks and execution integration tests.

## FACT-DISPATCH-004: Durable ingress still commits through the store boundary

- Status: active
- Owner: [Durable semantics](../design/run-ingress-message-delivery.md#durable-semantics)
- Fact: queued work, recovery, and projections are valid only when they preserve the runtime commit path and derive public views after commit.
- Links: guardrails G1 and G13
- Verification: staged commit, projection ordering, and transactional store tests.
