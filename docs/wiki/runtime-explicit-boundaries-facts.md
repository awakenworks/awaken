---
type: Fact Index
title: Runtime Explicit Boundary Facts
description: Cross-document facts for protocol adapters, permission, binding, projection, errors, and package enforcement.
tags: [runtime, boundaries, protocol, permission, packaging]
timestamp: 2026-06-27T00:00:00+08:00
---

# Runtime Explicit Boundary Facts

Owner: [packaging-enforcement-matrix.md](../design/packaging-enforcement-matrix.md).

## FACT-EXPLICIT-001: Protocol adapters own public wire shape

- Status: active
- Owner: [Protocol adapter boundaries](../design/protocol-adapter-boundaries.md)
- Fact: public protocol adapters translate requests, streams, replay, errors, and unsupported APIs at the edge while runtime receives neutral commands and projection sources.
- Links: guardrails G10, G19, and G26
- Verification: adapter conformance fixtures, DTO leak checks, and unsupported-management tests.

## FACT-EXPLICIT-003: Permission is the authorization path

- Status: active
- Owner: [Permission policy axis](../design/permission-policy-axis.md)
- Fact: protected operations pass through explicit permission decisions; visibility, compatibility, health, and resource realization do not authorize.
- Links: guardrails G9 and G21
- Verification: no-hidden-grant tests, permission decision API checks, and audit commit tests.

## FACT-EXPLICIT-004: Runtime validates selected model bindings

- Status: active
- Owner: [Model provider, model, and backend binding](../design/model-provider-backend-binding.md)
- Fact: model-provider/model selection happens from explicit `ModelProviderSpec`, `ModelSpec`, `ModelPoolSpec`, and `AgentSpec` graph data before execution; `ModelProviderSpec` is a model-access provider record, `AgentSpec` assembles behavior by reference, and runtime validates the selected binding instead of searching for replacements.
- Links: guardrail G22
- Verification: binding snapshots and provider-search dependency checks.

## FACT-EXPLICIT-005: Publication and projection are atomic handoffs

- Status: active
- Owner: [Config publication lifecycle](../design/config-publication-lifecycle.md)
- Fact: config publication uses versioned snapshots and atomic runtime catalog install; public durable projection derives from committed runtime truth.
- Links: guardrails G3, G4, G23, and G29
- Verification: publication transaction tests, rollback tests, and projection replay tests.

## FACT-EXPLICIT-006: Builtin tools are extension-owned

- Status: active
- Owner: [Builtin tools extension contract](../design/builtin-tools-extension-contract.md)
- Fact: first-party tool ids live in `awaken-ext-builtin-tools`, enter through plugin/catalog/permission ports, and execute through runtime adapters.
- Links: guardrails G14 and G16
- Verification: plugin registration tests, runtime dependency checks, and environment execution tests.

## FACT-EXPLICIT-008: Observability and eval consume truth

- Status: active
- Owner: [Observability, eval, and dataset boundary](../design/observability-eval-dataset-boundary.md)
- Fact: traces, datasets, eval reports, and experiments consume committed facts or normal runtime ports rather than rewriting runtime truth.
- Links: guardrail G25
- Verification: dataset lineage tests, eval harness tests, and trace deletion/replay tests.

## FACT-EXPLICIT-009: Errors stay domain-owned

- Status: active
- Owner: [Error taxonomy](../design/error-taxonomy.md)
- Fact: stable errors are owned by their source domain and protocol adapters map neutral errors to public error schemas.
- Links: guardrails G10 and G26
- Verification: error mapping snapshots and public DTO dependency checks.

## FACT-EXPLICIT-010: Packaging checks enforce boundaries

- Status: active
- Owner: [Packaging enforcement matrix](../design/packaging-enforcement-matrix.md)
- Fact: import direction, license metadata, vocabulary rules, ownership rows, and catalog coverage are expected to be enforced mechanically.
- Links: guardrails G15, G16, G27, and G29
- Verification: CI dependency checks, SPDX checks, ownership-index checks, and role-catalog checks.

## FACT-EXPLICIT-011: Model pools own fallback policy

- Status: active
- Owner: [Spec graph and binding values](../design/model-provider-backend-binding.md#spec-graph-and-binding-values)
- Fact: fallback, routing, weighting, health inputs, and downgrade rules belong in `ModelPoolSpec` or pre-activation adapter input, not in the runtime execution loop.
- Links: [D22](../design/key-design-decisions.md#d22---config-graph-is-explicit-before-runtime); guardrail G22
- Verification: fallback policy tests, no-runtime-provider-search dependency checks, and replay/debug metadata tests.
