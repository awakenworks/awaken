# ADR-0017: Thread Execution Ownership and Queue Semantics

- **Status**: Accepted
- **Date**: 2026-03-27
- **Depends on**: ADR-0006, ADR-0011

## Context

Multiple layers (server, runtime, transport protocols) need to manage run lifecycle and concurrency. Without a clear ownership boundary, concurrency control can be duplicated or inconsistent across layers. The system must enforce at most one active run per thread while supporting queued submissions from the server layer.

## Decision

### D1: AgentRuntime is the single concurrency authority

`AgentRuntime` (via `ActiveRunRegistry`) is the sole authority for per-thread execution concurrency. It enforces at most one active run per thread at any time. No other layer independently enforces this constraint.

### D2: RunDispatcher provides queue semantics

`RunDispatcher` in the server layer (`awaken-server`) provides queue semantics for runs targeting the same thread. It serializes submission so that queued runs are submitted to the runtime in order. However, `RunDispatcher` does not independently enforce concurrency — it relies on the runtime to accept or reject runs.

### D3: Queue items consumed after runtime acceptance

A queued run is only consumed (dequeued) after the runtime successfully accepts it. If the runtime rejects a run (thread already has an active run), the queue item remains and is retried when the current run completes.

### D4: Transport layers must not bypass runtime

Transport and protocol layers (A2A, HTTP API) must not bypass `AgentRuntime` for lifecycle or concurrency decisions. They submit runs through the server layer, which forwards to the runtime. Failed runs are reported to callers, not silently dropped.

## Consequences

- The server layer is thin: it queues and forwards. The runtime owns execution and concurrency.
- No duplicate concurrency control across layers — a single enforcement point in `ActiveRunRegistry`.
- Protocol-specific adapters (A2A, REST) share the same submission path and get consistent behavior.
- Callers receive explicit errors when runs cannot be accepted, enabling retry or user feedback.
