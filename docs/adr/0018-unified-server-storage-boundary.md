# ADR-0018: Unified Server Storage Boundary

- **Status**: Accepted
- **Date**: 2026-03-27
- **Depends on**: ADR-0011, ADR-0012

## Context

ADR-0012 introduced `ThreadRunStore` with atomic checkpoint semantics for the runtime. The server layer also needs to read thread and run data for API queries (A2A status, thread listing, run queries). If the server uses a different store instance or a separate read path, the query results may be inconsistent with what the runtime has checkpointed.

## Decision

### D1: Single Arc\<dyn ThreadRunStore\> in the server

The server holds a single `Arc<dyn ThreadRunStore>` instance. This is the same instance provided to `AgentRuntime` for checkpoint persistence. There are no separate `ThreadStore` or `RunStore` references in the server layer.

### D2: All server reads go through ThreadRunStore

All server-side read operations — A2A task status queries, thread listing, run queries — use the same `ThreadRunStore` instance that the runtime uses for checkpoints. This ensures the query path and checkpoint path share the same consistency boundary.

### D3: Type system enforces single instance

The server's constructor requires `Arc<dyn ThreadRunStore>` and passes it to both the runtime and the query handlers. There is no way to accidentally construct a server with different store instances for reads and writes.

## Consequences

- No consistency split between read and write paths. A query immediately after a checkpoint sees the checkpointed data.
- ADR-0012's transactional guarantee (`checkpoint()` atomicity) extends to all server read paths.
- Storage implementations need only satisfy the `ThreadRunStore` trait — no additional query-specific traits for the server.
- Adding new query endpoints requires no new storage wiring; they use the existing store instance.
