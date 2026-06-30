---
type: Lessons
title: Engineering Lessons
description: Durable engineering rules extracted from the current design corpus.
tags: [lessons, engineering, design]
timestamp: 2026-06-25T00:00:00+08:00
---

# Engineering Lessons

Lessons are retrieval hooks. The linked owner remains authoritative.

## FACT-LESSON-001: Keep runtime smaller than product

- Status: active
- Owner: [Key design decisions](../design/key-design-decisions.md)
- Fact: runtime code should expose neutral execution behavior and leave public product names, protocol DTOs, and outcome labels to adapters.
- Links: guardrails G2, G10, and G11
- Verification: dependency checks, public API checks, and contract snapshots.

## FACT-LESSON-002: Send data across config edges

- Status: active
- Owner: [Neutral waist data edge](../design/neutral-waist.md#data-only-config-edge)
- Fact: cross-boundary config input should be serializable resolved data plus fingerprints, not live registries, factories, scope objects, or product DTOs.
- Links: guardrails G3 and G4
- Verification: serde roundtrip and catalog mismatch tests.

## FACT-LESSON-003: Add durability around runtime control

- Status: active
- Owner: [Run ingress](../design/run-ingress-message-delivery.md#run-ingress)
- Fact: direct runtime control stays simple; durable buffering, routing, claims, and replay are server concerns layered around it.
- Links: guardrails G5 and G6
- Verification: ingress capability and route tests.

## FACT-LESSON-004: Compatibility signals are not grants

- Status: active
- Owner: [Selection is not authorization](../design/credentials-and-vaults.md#selection-is-not-authorization)
- Fact: selection, capability compatibility, catalog visibility, and health probes can narrow candidates but cannot authorize use.
- Links: guardrail G9
- Verification: type/API checks and explicit permission-path tests.

## FACT-LESSON-005: Project only after truth is committed

- Status: active
- Owner: [Commit, fact, and projection taxonomy](../design/commit-fact-projection-taxonomy.md)
- Fact: public sessions, SSE, webhooks, and replay logs must follow the authoritative commit boundary instead of writing independent truth.
- Links: guardrails G1 and G13
- Verification: transactional store and projection ordering tests.

## FACT-LESSON-006: Name contracts by authority

- Status: active
- Owner: [D12 - Contract Names Follow Authority](../design/key-design-decisions.md#d12---contract-names-follow-authority)
- Fact: use crate names for layer/dependency direction, module paths for domain context, and short type names with explicit public aliases instead of long prefixed names.
- Links: guardrails G1, G2, G5, G10, and G13
- Verification: dependency checks, public API checks, store conformance tests, and protocol replay tests.

## FACT-LESSON-007: Execute snapshots, not bare agent ids

- Status: active
- Owner: [D19 - Executable Snapshot Is The Run Configuration Identity](../design/key-design-decisions.md#d19---executable-snapshot-is-the-run-configuration-identity)
- Fact: runtime execution should start from immutable executable snapshot data, whether supplied inline or resolved by id, because agent ids can map to different configurations across runs.
- Links: guardrails G3, G4, and G28
- Verification: inline/by-id snapshot tests, fingerprint mismatch tests, and runtime contract surface checks.

## FACT-LESSON-008: Keep publication services outside runtime

- Status: active
- Owner: [D20 - Publication Coordination Is Outside Runtime](../design/key-design-decisions.md#d20---publication-coordination-is-outside-runtime)
- Fact: config publish ordering and registry compilation should live in config-side services; runtime should expose a catalog install port and continue with snapshot resolution and execution after install.
- Links: guardrails G18, G23, and G29; [config flow facts](config-to-run-execution-flow-facts.md)
- Verification: dependency-direction checks, runtime public API checks, and publication/install transaction tests.

## FACT-LESSON-009: An ADR records a contested decision, not a feature slice

- Status: active
- Owner: [ADR-0001 Amendment (2026-06-30)](../adr/0001-documentation-model-and-vocabulary-alignment.md#amendment-2026-06-30-the-bar-for-a-new-adr)
- Fact: a new ADR needs a rejected alternative, re-litigation risk, and cross-crate or invariant reach; a refinement of a prior ADR's deferred item is an amendment to that ADR, not a new number.
- Links: ADR-0001 D1 and D6
- Verification: `check_adr.py` structure check (the bar itself is review judgment).
