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
- Owner: [Static domain model](../design/resources-memory-files-skills.md#static-domain-model)
- Fact: runtime input carries logical refs, descriptors, and hashes; local paths and live resource handles are created inside environment adapters.
- Links: guardrails G3 and G8
- Verification: serde tests, dependency checks, and environment realization tests.

## FACT-RES-002: Memory and files are product or environment resources

- Status: active
- Owner: [Owning contexts](../design/resources-memory-files-skills.md#owning-contexts)
- Fact: mutable Memory and immutable content-addressed Files, including their indexes, quotas, sharing, mounts, and cleanup, belong outside Runtime Core; execution uses approved resource and environment ports.
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
- Owner: [Recovery and reclamation](../design/resources-memory-files-skills.md#recovery-and-reclamation)
- Fact: committed runtime facts are replayable, while Session resource activations, mounts, paths, leases, and process ids are re-created from the secret-free effective input manifest outside durable Runtime truth.
- Links: guardrails G1 and G13
- Verification: replay tests and recovery tests that avoid local path authority.

## FACT-RES-005: External work is execution offload

- Status: active
- Owner: [External work](../design/resources-memory-files-skills.md#external-work)
- Fact: off-process work uses tool or backend execution paths and returns typed results; it does not add a separate session dispatcher.
- Links: guardrails G5 and G6
- Verification: ingress capability tests and dependency checks.

## FACT-RES-006: Input identity follows content lifecycle

- Status: active
- Owner: [Static domain model](../design/resources-memory-files-skills.md#static-domain-model)
- Fact: a File binding names immutable content, while Agent bindings for Memory and Repository name only their stable resource identities.
- Links: guardrail G37
- Verification: typed-binding serialization and content-id tests.

## FACT-RES-007: Session resolution is the configuration pin point

- Status: active
- Owner: [Common configure-to-reclaim flow](../design/resources-memory-files-skills.md#common-configure-to-reclaim-flow)
- Fact: one Session resolver merges Agent defaults and direct attachments into revision-1 desired truth before the original insert; active truth appears only after realization. Later Resource commands first obey the baseline mutation policy, then reuse the canonical manifest lifecycle; configuration selection still does not select Memory content or a Git commit.
- Links: guardrail G37; [profiled input amendment](../adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md#amendment-profiled-inputs-freeze-in-the-original-root-2026-08-27)
- Verification: single-resolution, revision-1 desired-versus-active, complete-before-insert, policy-constrained replacement, and no-content-pin tests.

## FACT-RES-008: Lifecycle stages have named owners

- Status: active
- Owner: [Lifecycle stage ownership](../design/resources-memory-files-skills.md#lifecycle-stage-ownership)
- Fact: catalog, binding, resolution, authorization, activation, use, release, and reclamation each have one orchestration component; resource repositories enforce only their intrinsic data invariants.
- Links: guardrails G14 and G38
- Verification: component-boundary review and lifecycle scenario tests.

## FACT-RES-009: Live deny overrides historical configuration

- Status: active
- Owner: [Lifecycle state and live deny](../design/resources-memory-files-skills.md#lifecycle-state-and-live-deny)
- Fact: current ownership, suspension/deletion, authorization, and credential revocation can deny use of a previously resolved configuration.
- Links: guardrails G21 and G37
- Verification: cross-Workspace and post-revocation fail-closed tests.

## FACT-RES-010: Logical deletion precedes physical reclamation

- Status: active
- Owner: [Recovery and reclamation](../design/resources-memory-files-skills.md#recovery-and-reclamation)
- Fact: a resource is denied and tombstoned before asynchronous cleanup; purge waits for the references and activations required by that resource kind.
- Links: guardrail G38
- Verification: crash-reconciliation, retention, and no-live-reference purge tests.

## FACT-RES-011: Physical reclamation is resource-fenced, not IAM-locked

- Status: active
- Owner: [Recovery and reclamation](../design/resources-memory-files-skills.md#recovery-and-reclamation)
- Fact: zero-reference proof, durable physical-identity fencing, and racing reference rejection are one resource-store consistency protocol; the protocol contains no principal, role, API key, policy, Org, Project, or WorkUnit.
- Links: guardrails G21 and G38
- Verification: in-memory/SQLite/Postgres store conformance, cross-Workspace shared-blob fencing, crash/retry reclamation tests, and dependency-boundary checks.

## FACT-RES-012: Session resource mutations share one root fence

- Status: active
- Owner: [File lifecycle and Session-owned Repository adoption](../design/resources-memory-files-skills.md#file-lifecycle)
- Fact: File item create/delete is an adapter over the exact-read-revision complete-manifest root CAS, so a concurrent loser is `409` without rebase or lost update. Registry and Vault participants for a Session-owned Repository retain independent `Applied | Replayed` provenance until root adoption; pre-root compensation retires only unreferenced Applied work. After adoption, successful item or whole-manifest omission persists an exact retirement intent in the Session Resource root. The common reconciler retains and retries it across receipt replay, external activation, terminal cleanup, and restart; same-id reintroduction conflicts until completion. Retirement requires both the closed Managed/Profiled namespace and exact owner metadata, leaves shared/markerless definitions and Vault untouched, and retires an owned inline credential by exact revision before the Repository.
- Links: [ADR-0063 Session mutation fencing](../adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md#session-scoped-repository-adoption-and-mutation-fencing)
- Verification: File create/delete race tests, item/whole-manifest durable-retirement tests, same-id admission barrier, SQLite cleanup retry after reopen, whole-manifest root-loser tests, Applied/Replayed compensation matrix, owner-marker filter tests, and exact inline-credential reclamation tests.

## FACT-RES-013: Explicit Repository publication is terminal-cleanup work

- Status: active
- Owner: [Explicit terminal publication](../design/resources-memory-files-skills.md#explicit-terminal-publication)
- Fact: an explicit release freezes one active writable Repository input and its exact branch/full commit in the Session cleanup operation. Delegated children settle first, the existing Repository realizer returns one canonical publication receipt, the Session root persists it, and only then may root cleanup dispose the working tree. Omitted intent keeps the v1 no-publication path; credentials and mediated capabilities remain effect-local.
- Links: guardrail G48; [ADR-0063 publication amendment](../adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md#amendment-explicit-terminal-repository-publication-2026-08-28); [ADR-0075 Worker amendment](../adr/0075-unified-managed-session-worker-execution.md#amendment-2026-08-28-terminal-repository-publication-stays-on-the-unified-worker-path)
- Verification: legacy wire/fingerprint compatibility, child/publication/root ordering, exact receipt replay, stale-lease transport rejection, and absent/exact/conflicting remote-ref tests.
