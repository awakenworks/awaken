---
type: Fact Index
title: Credentials And Vaults Facts
description: Cross-document facts for credential refs, vault lifecycle, selection, and availability.
tags: [credentials, vaults, selection, availability]
timestamp: 2026-06-25T00:00:00+08:00
---

# Credentials And Vaults Facts

Owner: [credentials-and-vaults.md](../design/credentials-and-vaults.md).

## FACT-CRED-001: Runtime receives opaque credential refs

- Status: active
- Owner: [Runtime boundary](../design/credentials-and-vaults.md#runtime-boundary)
- Fact: resolved runtime input may name credential references and allowed ids, but secret values and vault handles remain in product or environment code.
- Links: guardrail G8
- Verification: no-secret serialization tests and config materialization tests.

## FACT-CRED-002: Selection and probes never authorize

- Status: active
- Owner: [Selection is not authorization](../design/credentials-and-vaults.md#selection-is-not-authorization)
- Fact: candidate selection, pool membership, availability, and health checks produce operational signals only; grants come from the explicit permission path.
- Links: guardrail G9; [tool facts](tool-and-capability-facts.md)
- Verification: result type checks and permission tests.

## FACT-CRED-003: Vault lifecycle is product-owned

- Status: active
- Owner: [Domain model](../design/credentials-and-vaults.md#domain-model)
- Fact: vault records, account grouping, refresh, rotation, cooldown, and operator overrides are product data-plane concerns, not runtime core state.
- Links: guardrail G8
- Verification: dependency checks and secret-free output contract tests.

## FACT-CRED-004: Credential support starts with ref resolution

- Status: active
- Owner: [First vertical slice](../design/credentials-and-vaults.md#first-vertical-slice)
- Fact: the first implementation slice resolves a named credential ref, verifies policy separately, materializes opaque runtime input, and proves secret-free replay.
- Links: guardrails G8, G9, and G14
- Verification: docs review checklist and replay/serialization tests.
