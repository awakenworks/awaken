# State and Snapshot Model

Awaken uses a typed state engine with snapshot isolation. This page explains the state primitives, scoping rules, merge strategies, and the mutation lifecycle.

## StateKey Trait

Every piece of runtime state is declared as a type implementing `StateKey`:

```rust,ignore
pub trait StateKey: 'static + Send + Sync {
    const KEY: &'static str;
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;
    type Update: Send + 'static;

    fn apply(value: &mut Self::Value, update: Self::Update);
}
```

A `StateKey` is a zero-sized type that associates a string key, a value type, an update type, and a merge function. The `apply` method defines how an update transforms the current value. Serialization is handled through `encode`/`decode` with JSON as the interchange format.

Plugins register their state keys through `PluginRegistrar::register_key::<K>()` during the `Plugin::register` callback.

## KeyScope

```rust,ignore
pub enum KeyScope {
    Run,     // Cleared at run start (default)
    Thread,  // Persists across runs on the same thread
}
```

**Run-scoped** keys are reset to their default value when a new run begins. Use this for per-run counters, step state, and transient execution metadata.

**Thread-scoped** keys survive across runs on the same thread. Use this for conversation memory, accumulated context, and persistent agent preferences.

## MergeStrategy

```rust,ignore
pub enum MergeStrategy {
    Exclusive,    // Conflict on concurrent writes (default)
    Commutative,  // Order-independent updates, safe to merge
}
```

When multiple hooks run in the same phase and produce `MutationBatch` values that touch the same key:

- **Exclusive** -- the batches cannot be merged. The runtime detects the conflict and falls back to sequential execution. This is the safe default for keys where write order matters.

- **Commutative** -- the update operations can be applied in any order and produce the same result. The runtime concatenates the operations from both batches. Use this for append-only logs, counters, and set unions.

## Snapshot

A `Snapshot` is an immutable view of the state at a point in time. Phase hooks receive a snapshot reference and can read any registered key's value. They cannot mutate the snapshot directly.

```text
Phase hook receives: &Snapshot
Phase hook writes to: MutationBatch
```

This separation guarantees that hooks within the same phase see identical state regardless of execution order.

## MutationBatch

A `MutationBatch` collects state updates produced by a single hook invocation:

```rust,ignore
pub struct MutationBatch {
    base_revision: Option<u64>,
    ops: Vec<Box<dyn MutationOp>>,
    touched_keys: Vec<String>,
}
```

Each operation in `ops` is a type-erased `KeyPatch<K>` that carries the `K::Update` value. When the batch is applied, each operation calls `K::apply(value, update)` on the target key in the state map.

The `touched_keys` list enables conflict detection for `Exclusive` keys during parallel merge.

## Mutation Lifecycle

```text
1. Phase starts
2. Runtime takes a Snapshot (immutable clone)
3. Each hook reads from the Snapshot, produces a MutationBatch
4. All hooks complete (phase convergence)
5. Runtime checks MutationBatch key overlap:
   - Exclusive keys overlap -> conflict (sequential fallback)
   - Commutative keys overlap -> ops concatenated
   - No overlap -> batches merged
6. Merged batch applied atomically to the live state
7. New Snapshot taken for the next phase
```

## StateMap

`StateMap` is the runtime's typed state container. It uses `typedmap::TypedMap` internally, keyed by zero-sized `TypedKey<K>` wrappers that hash and compare by `TypeId`. The `get::<K>()` and `get_or_insert_default::<K>()` methods retrieve the concrete `K::Value` type directly without downcasting.

State keys registered with `persistent: true` in `StateKeyOptions` are serialized during checkpoint and restored on thread load. Non-persistent keys exist only in memory for the duration of the run.

## StateStore

`StateStore` wraps the `StateMap` and provides:

- Snapshot creation (cheap `Arc` clone of the inner map)
- Batch application with revision tracking
- Commit hooks (`CommitHook`) that fire after each successful state mutation
- `StateCommand` processing for programmatic state operations

## Shared State

For state that must be shared **across thread and agent boundaries**, awaken provides a separate persistent layer built on the same `ProfileStore` backend used by profile data.

### ProfileKey for shared state

Shared state uses the same `ProfileKey` trait as profile data. The `KEY` constant serves as the namespace, and the `key: &str` parameter passed to `ProfileAccess` methods identifies the runtime instance:

```rust,ignore
pub trait ProfileKey: 'static + Send + Sync {
    const KEY: &'static str;
    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;
}
```

Unlike `StateKey`, shared state does not participate in the `MutationBatch` / snapshot workflow -- it is accessed asynchronously through `ProfileAccess` with a `key: &str` parameter.

### StateScope

```rust,ignore
pub struct StateScope(String);
```

An optional convenience builder for common key string patterns. Constructors: `global()`, `parent_thread(id)`, `agent_type(name)`, `thread(id)`, `new(arbitrary)`. Call `.as_str()` to get the key string for `ProfileAccess` methods. Users can also pass any raw `&str` directly.

### State Management Overview

| Layer | Scope | Access | Lifecycle | Use Case |
|-------|-------|--------|-----------|----------|
| Run State | Current run | Sync snapshot | Cleared at run start | Transient flags, counters |
| Thread State | Same thread | Sync snapshot | Export/restore across runs | Tool call state, active agent |
| Shared State | `ProfileKey` + `StateScope` | Async | Persistent | Cross-boundary sharing |
| Profile State | `ProfileKey` + `key: &str` | Async | Persistent | User/agent preferences |

Choose shared state when you need **dynamic scoping** or **cross-boundary access**. Choose `StateKey` when you need **sync access** or **transactional merge** during parallel tool execution.

## See Also

- [State Keys](../reference/state-keys.md)
- [Run Lifecycle and Phases](./run-lifecycle-and-phases.md)
- [Architecture](./architecture.md)
- [Use Shared State](../how-to/use-shared-state.md)
