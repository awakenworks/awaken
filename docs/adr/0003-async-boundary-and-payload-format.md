# ADR-0003: Async Boundary and Payload Format

- **Status**: Implemented
- **Date**: 2026-03-21
- **Depends on**: ADR-0001, ADR-0002

## Context

The framework spans pure in-memory state operations and I/O-bound plugin logic (LLM inference, tool execution, persistence). Need to decide where the sync/async boundary sits, and how action/effect payloads cross the dispatch boundary.

## Decision

**Synchronous state engine, asynchronous runtime boundary**:

- Sync (no async, no tokio locks): `StateStore.commit()`, `MutationBatch`, `StateMap` / `Snapshot` reads
- Async (`async_trait` / Tokio): plugin phase hooks, action handlers, effect handlers, `PhaseRuntime.run_phase()`, runtime orchestration, inference/tool execution, credentials, live run control, mailbox/persistence calls

State operations are in-memory mutations. Async would force
`tokio::sync::RwLock` into the state engine, adding deadlock surface with no
benefit. The async boundary sits around orchestration and I/O: agent loop
control, model calls, tool calls, credentials, live control streams, and
durable store calls.

**Action/effect payloads serialized via `serde_json::Value`**: Action frequency is low (single-digit to tens per phase), making JSON overhead negligible. Gains: human-readable audit logs, persistable without extra infrastructure. Rejected: binary (schema management burden), `Box<dyn Any>` (loses auditability and cross-process capability).

## Implementation Status

- Sync state engine: implemented
- JSON payloads (`serde_json::Value`): implemented
- Async hooks / handlers / orchestration: implemented (phase hooks, action handlers, effect handlers, `PhaseRuntime::run_phase()`, and runtime control paths use async fn signatures)

## Consequences

- Action/effect logs are natively inspectable as JSON
