---
type: Fact Index
title: Configuration-To-Response Flow Facts
description: Retrieval anchors for the two canonical distributed service flows.
tags: [control, coordinator, worker, publication, session, response]
timestamp: 2026-07-30T00:00:00+08:00
---

# Configuration-To-Response Flow Facts

Owner: [Configuration-to-application and request-to-response flows](../design/config-to-run-execution-flow.md).

## FACT-FLOW-001: Publication and registration are distinct

- Status: active
- Owner: [Flow one](../design/config-to-run-execution-flow.md#flow-one-configuration-to-application)
- Fact: Control publishes immutable Agent truth and Coordinator registers its rebuildable executable projection through one idempotent boundary.
- Links: [ADR-0071](../adr/0071-distributed-service-boundaries-and-executable-agent-registration.md); guardrails G3, G23, G29, and G45
- Verification: registration rules E1 through E3 in the owner document.

## FACT-FLOW-002: Deployment launch reuses Session authority

- Status: active
- Owner: [Deployment and Session creation](../design/config-to-run-execution-flow.md#deployment-and-session-creation)
- Fact: the Coordinator-owned Deployment application invokes the local Session command and uses `deployment_run_id` to prevent duplicate Sessions; the former remote launch path is retired.
- Links: [Managed Deployments](../design/managed-deployments.md); guardrail G45
- Verification: launch rule E4 in the owner document.

## FACT-FLOW-003: Resource realization stays type-specific

- Status: active
- Owner: [Per-kind materialization](../design/config-to-run-execution-flow.md#per-kind-materialization)
- Fact: the Session manifest is common input while File, Memory, Repository, Skill, and credential realization retain separate ports and consistency rules.
- Links: [Resource owner](../design/resources-memory-files-skills.md); [credential owner](../design/credentials-and-vaults.md); guardrails G37, G38, G43, and G45
- Verification: realization rules E5 through E7 and E10 in the owner document.

## FACT-FLOW-004: Committed truth owns the complete response

- Status: active
- Owner: [Commit and response authority](../design/config-to-run-execution-flow.md#commit-and-response-authority)
- Fact: preview frames are optional, while committed facts drive terminal SSE delivery, HTTP receipts, replay, and reconnect backfill.
- Links: [Commit taxonomy](../design/commit-fact-projection-taxonomy.md); guardrails G1, G13, and G23
- Verification: recovery and response rules E8 and E9 in the owner document.
