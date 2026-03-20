# ADR-0008: State Scoping and Parallel Safety

- **Status**: Not Implemented
- **Date**: 2026-03-21
- **Depends on**: ADR-0002

## Context

Different state has different lifetimes: some persists across runs (Global), some resets per run (Run), some is isolated per tool call (ToolCall). Parallel tool execution (ADR-0007) requires concurrent `StateCommand`s to merge safely.

## Decision

**SlotScope**:

```rust
pub enum SlotScope {
    Global,     // never auto-cleared
    Run,        // cleared at run start
    ToolCall,   // isolated per tool call, cleared after completion
}
```

Specified in `SlotOptions` at registration. Lifecycle: `StateStore::begin_run()` clears Run-scoped slots; `StateStore::end_tool_call(call_id)` removes the ToolCall namespace. ToolCall slots keyed by `(TypeId, call_id)`. Reading ToolCall-scoped slots requires `call_id` in `PhaseContext` (set during BeforeToolExecute/AfterToolExecute); without it, returns None.

**Parallel merge**: ToolCall-scoped slots cannot conflict (disjoint namespaces). Run/Global-scoped slots require disjoint-write validation: two tools modifying different slots → merge; same slot → reject. `MutationBatch` gains a disjoint merge operation.

Rejected slot-level CRDT merge: would require every `StateSlot` to implement a merge function. The common case (tools writing to their own ToolCall-scoped slots) is conflict-free by construction.

## Consequences

- `SlotOptions` gains `scope: SlotScope` field
- `StateStore` gains `begin_run()`, `end_tool_call(call_id)` methods
- `PhaseContext` gains optional `call_id` field
- `MutationBatch` gains disjoint merge validation
