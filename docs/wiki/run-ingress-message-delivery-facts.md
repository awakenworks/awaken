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
- Fact: the runtime executes tools in-process; OS process lifecycle and durable-dispatch mechanics around the run are a server/dispatch concern.
- Links: [resources facts](resources-memory-files-skills-facts.md)
- Verification: dependency checks and execution integration tests.

## FACT-DISPATCH-004: Durable ingress still commits through the store boundary

- Status: active
- Owner: [Durable semantics](../design/run-ingress-message-delivery.md#durable-semantics)
- Fact: queued work, recovery, and projections are valid only when they preserve the runtime commit path and derive public views after commit.
- Links: guardrails G1 and G13
- Verification: staged commit, projection ordering, and transactional store tests.

## FACT-DISPATCH-005: A caller-owned Run id binds one canonical dispatch

- Status: active
- Owner: [ADR-0060 D6](../adr/0060-durable-dispatch-completion-tombstone.md#d6-a-caller-owned-run-id-identifies-one-canonical-dispatch)
- Fact: exact retries ignore only request-local tracing; a different or historically unverifiable payload under the same Run id fails closed before eligibility, and completion retains the canonical identity on the existing tombstone.
- Links: guardrail G35
- Verification: shared memory/SQLite/Postgres dispatch cause/effect conformance table plus SQL legacy-NULL migration fixtures.

## FACT-DISPATCH-006: Retry exhaustion commits terminal Run truth

- Status: active
- Owner: [ADR-0015](../adr/0015-crash-retry-budget-and-dead-letter.md)
- Fact: the active drainer (standalone service, local pool, or remote Worker pool) atomically claims retry exhaustion before ordinary work and commits `Ended(Indeterminate)` through the ordinary Worker terminal and `Done` path. Coordinator-only maintenance never competes; without a drainer the row remains durable. Dead-letter is explicit operator quarantine only.
- Links: guardrails G31 and G35
- Verification: shared backend terminal-claim decision table plus service, pool, commit-failure, and remote-transport tests.
