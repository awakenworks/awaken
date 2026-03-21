# ADR-0008: State Scoping and Parallel Safety

- **Status**: Not Implemented
- **Date**: 2026-03-21
- **Depends on**: ADR-0002

## Context

Different state has different lifetimes: some persists across runs (Global), some resets per run (Run), some is isolated per tool call (ToolCall). Parallel tool execution (ADR-0007) requires concurrent `StateCommand`s to merge safely.

## Decision

**KeyScope**:

```rust
pub enum KeyScope {
    Global,     // never auto-cleared
    Run,        // cleared at run start
    ToolCall,   // isolated per tool call, cleared after completion
}
```

Specified in `StateKeyOptions` at registration. Lifecycle: `StateStore::begin_run()` clears Run-scoped keys; `StateStore::end_tool_call(call_id)` removes the ToolCall namespace. ToolCall keys keyed by `(TypeId, call_id)`. Reading ToolCall-scoped keys requires `call_id` in `PhaseContext` (set during BeforeToolExecute/AfterToolExecute); without it, returns None.

**Parallel merge**: ToolCall-scoped keys cannot conflict (disjoint namespaces). Run/Global-scoped keys require disjoint-write validation: two tools modifying different keys → merge; same key → reject. `MutationBatch` gains a disjoint merge operation.

Rejected key-level CRDT merge: would require every `StateKey` to implement a merge function. The common case (tools writing to their own ToolCall-scoped keys) is conflict-free by construction.

## Consequences

- `StateKeyOptions` gains `scope: KeyScope` field
- `StateStore` gains `begin_run()`, `end_tool_call(call_id)` methods
- `PhaseContext` gains optional `call_id` field
- `MutationBatch` gains disjoint merge validation
