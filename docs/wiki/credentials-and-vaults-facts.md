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

## FACT-CRED-005: Session MCP convergence is an accepted target

- Status: accepted target; Slice 0 contract closure required, not a current invariant
- Owner: [ADR-0066](../adr/0066-session-service-binding-and-realization.md)
- Fact: the scoped Session persistence row carries one consumed complete creation intent, one immutable finalized baseline, the existing versioned Resource state, and one MCP-only attachment set under replace/tombstone root mutation. Agent and Session MCP inputs share one normalizer; the existing Managed full-replacement API diffs into the same generations. A self-hosted Session uses the Environment WorkQueue, and a Session realization lease fences local/remote stage, CAS Active, publish, drain, and recovery (ADR-0075).
- Links: [architecture invariants](../INVARIANTS.md); [remote Worker protocol](../design/remote-worker-protocol.md)
- Verification: planned root-CAS/store conformance, old-path deletion, Environment pin/network-intersection, MCP create/call/add/replace/remove/recovery, Native/ACP parity, stale-ownership, and out-of-policy target tests.

## FACT-CRED-006: Plaintext boundary and model exposure are separate

- Status: accepted target landing in verified slices; partial mechanisms do not establish the whole target policy
- Owner: [ADR-0067](../adr/0067-credential-custody-model-exposure-and-secret-delivery.md)
- Fact: Vault is storage at rest. Published access separates material source from a recipient-bound sealed payload reference, exact resolver and optional OAuth refresh/reseal access, and lists allowed plaintext-holder trust domains independently from `Forbidden`/`VirtualOnly` model exposure. MCP and Repository Resource generations or the atomic dispatch claim epoch pin purpose-specific exact Environment-requested holders before materialization; claim admission checks frozen Session MCP pins against the selected Worker's installed capabilities through the common admission kernel. A hosted deployment may seal only that already-authorized exact Vault revision and binding through the one issuer port; the actual mechanism appears only in a secret-free receipt. Model, MCP, and Repository share one exact material resolver without sharing an aggregate, and Runtime has no bare Repository source-materialization path. There is no custody order, inference Service attachment, runtime holder fallback, plaintext material API, or URL/current-Vault refresh rediscovery. Automatic LLM Vault authoring is a separate, not-yet-designed application workflow.
- Links: [credentials and vaults](../design/credentials-and-vaults.md); [ADR-0066](../adr/0066-session-service-binding-and-realization.md)
- Verification: access source/envelope migration, explicit trust-domain selection and attempt/MCP/Repository generation pinning, Repository admission decision table, workload/Worker network conformance, model-exposure, Native/ACP parity, secret-leak, and no-downgrade tests.

## FACT-CRED-007: Hosted application MCP bearers reuse the Vault aggregate

- Status: active
- Owner: [Hosted application static-bearer admission](../design/credentials-and-vaults.md#hosted-application-static-bearer-admission)
- Fact: a trusted hosted application creates or rotates its stable MCP bearer through the existing Credential/Vault WAL and CAS, then gives the returned Vault id to ordinary Managed Session creation; it owns no local credential mirror or fallback.
- Links: [ADR-0066](../adr/0066-session-service-binding-and-realization.md#2026-08-13-amendment-hosted-application-mcp-credential-admission)
- Verification: idempotent replay, rotation, conflicting replay, concurrent-winner, normalized-target, and secret-free response tests.

## FACT-CRED-008: Hosted governance credentials extend the canonical CRUD

- Status: active
- Owner: [Hosted governance Credential Resources](../design/credentials-and-vaults.md#hosted-governance-credential-resources)
- Fact: hosted governance products create, recover, list, and validate generic business Credential Resource backing references through Awaken's existing `/v1/config/credentials` owner and Credential repository; reference validation uses the durable source id plus Workspace/provider and never requires a parallel operation-key map, local Vault, or provider-specific secret type.
- Links: Flow ADR-0041 (resource credential mechanism delegates to the execution secret-by-reference seam).
- Verification: operation-identity create/replay/conflict, scoped lookup/list, reference-validation, and secret-free response tests.

## FACT-CRED-009: Hosted Connector effects use one platform-held materializer

- Status: active
- Owner: [ADR-0067 platform-held Connector effects](../adr/0067-credential-custody-model-exposure-and-secret-delivery.md#amendment-platform-held-connector-effects-2026-08-13)
- Fact: the trusted Gateway process composes Awaken's canonical Credential repository, SecretStore, and pinned materializer under one exact `PlatformRelay` holder; built-in `CredentialUsage::HttpEffect` freezes every material field's exact header, query, or RFC 6901 JSON-pointer destinations, and Flow receives no material and owns neither a raw-secret endpoint nor a duplicate Vault.
- Links: [credentials and vaults](../design/credentials-and-vaults.md#hosted-governance-credential-resources)
- Verification: exact holder, Workspace, source revision, field/placement set, material shape, usage, and effect-target binding tests plus the hosted Gateway effect E2E; deployments without the canonical in-process adapter fail closed because envelope metadata is not a cryptographic transport.
