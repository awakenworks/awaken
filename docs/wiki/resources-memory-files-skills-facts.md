---
type: Fact Index
title: Resources, Memory, Files, And Skills Facts
description: Cross-document facts for logical resource refs, environment realization, skills, and recovery.
tags: [resources, memory, files, skills, recovery]
timestamp: 2026-06-25T00:00:00+08:00
---

# Resources, Memory, Files, And Skills Facts

Owner: [resources-memory-files-skills.md](../design/resources-memory-files-skills.md).

## FACT-RES-001: Resource refs cross; host paths do not

- Status: active
- Owner: [Resource boundary](../design/resources-memory-files-skills.md#resource-boundary)
- Fact: runtime input carries logical refs, descriptors, and hashes; local paths and live resource handles are created inside environment adapters.
- Links: guardrails G3 and G8
- Verification: serde tests, dependency checks, and environment realization tests.

## FACT-RES-002: Memory and files are product or environment resources

- Status: active
- Owner: [Memory and files](../design/resources-memory-files-skills.md#memory-and-files)
- Fact: mutable memory, files, indexes, quotas, sharing, mounts, and cleanup belong outside runtime core; runtime uses approved tool/resource ports.
- Links: guardrails G8 and G13
- Verification: no-path API checks and store truth review.

## FACT-RES-003: Skills and MCP execute by validated id

- Status: active
- Owner: [Skills and MCP](../design/resources-memory-files-skills.md#skills-and-mcp)
- Fact: public skill and MCP config produces descriptors and content hashes; runtime validates catalog data and invokes by id through existing tool/backend paths.
- Links: guardrails G4 and G8
- Verification: catalog fingerprint tests and descriptor hash tests.

## FACT-RES-004: Recovery replays facts and re-binds resources

- Status: active
- Owner: [Portable sessions and recovery](../design/resources-memory-files-skills.md#portable-sessions-and-recovery)
- Fact: committed runtime facts are replayable, while resource bindings, mounts, paths, and process ids are re-created outside durable runtime truth.
- Links: guardrails G1 and G13
- Verification: replay tests and recovery tests that avoid local path authority.

## FACT-RES-005: External work is execution offload

- Status: active
- Owner: [External work](../design/resources-memory-files-skills.md#external-work)
- Fact: off-process work uses tool or backend execution paths and returns typed results; it does not add a separate session dispatcher.
- Links: guardrails G5 and G6
- Verification: ingress capability tests and dependency checks.
