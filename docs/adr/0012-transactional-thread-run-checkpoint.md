# ADR-0012: Transactional Thread+Run Checkpoint Persistence

- **Status**: Accepted
- **Date**: 2026-03-24
- **Depends on**: ADR-0002, ADR-0006, ADR-0011

## Context

The runtime state engine (`StateStore`) supports export/restore, but persistence is not automatic by itself.  
Run correctness requires conversation history (thread messages) and run snapshot (`RunRecord.state`) to stay in sync.

If thread messages and run state are persisted separately, partial writes can produce split-brain checkpoints:

- thread updated but run snapshot stale
- run snapshot updated but thread stale

This is unacceptable for resume correctness.

## Decision

### D1: Introduce atomic checkpoint storage contract

Add a unified storage trait:

```rust
pub trait ThreadRunStore {
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError>;
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError>;
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError>;
}
```

`checkpoint(...)` is the transactional boundary and MUST atomically persist:

1. thread messages
2. run record (including `state`)

### D2: Runtime uses ThreadRunStore as single persistence dependency

`AgentRuntime` persistence dependency is unified as:

```rust
storage: Option<Arc<dyn ThreadRunStore>>
```

Builder API:

```rust
with_thread_run_store(...)
```

No split write path (`ThreadStore` then `RunStore`) is used in runtime checkpoints.

### D3: Loop checkpoints persist state + thread together

At step checkpoint and run end:

1. export `StateStore` via `export_persisted()`
2. build `RunRecord { state: Some(...) }`
3. call `ThreadRunStore::checkpoint(...)`

This guarantees durable checkpoint consistency for resume.

## Consequences

- Thread history and run snapshot are version-aligned at each checkpoint boundary.
- Resume reads no longer depend on best-effort dual writes.
- Storage implementations must provide transactional semantics for `checkpoint(...)`.

## Supersedes

This ADR supersedes the split-persistence assumption in ADR-0011 runtime storage sections (`thread_store` + `run_store` as independent checkpoint writes).
