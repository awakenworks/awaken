# ADR-0021: Shared State Scoping

- **Status**: Accepted
- **Date**: 2026-04-07
- **Depends on**: ADR-0002 (State Engine), ADR-0008 (State Scoping), ADR-0009 (Configuration and Profile)

## Context

The existing state system provides two distinct layers:

1. **Run-scoped and Thread-scoped `StateKey`** — in-memory, synchronous, transactional via `TypedMap`. Each key maps one Rust type to one value per scope. Designed for hot-path plugin state within a single run or thread.
2. **`ProfileKey` + `ProfileStore`** — persistent, async, key-scoped. Designed for long-lived per-agent or per-user data that survives across runs.

Neither layer supports **cross-boundary sharing with dynamic keys**. Common coordination needs that fall outside both:

- **Parent-child agent coordination**: a parent agent publishes state that child agents (running in separate threads) consume, keyed by `parent_thread_id`.
- **Global configuration**: feature flags or shared settings readable by all agents regardless of thread or run.
- **Agent-type-level state**: all instances of a given agent type share accumulated knowledge (e.g. learned preferences).
- **Custom groupings**: arbitrary scope strings for domain-specific partitioning (e.g. `"team::engineering"`).

Extending `StateKey` is not viable because `TypedMap` stores one value per type — there is no way to have multiple values of the same type under different dynamic scope strings. Extending `ProfileOwner` with new enum variants would conflate ownership semantics (who owns data) with scope semantics (who can see data), and every new scope pattern would require an enum change and recompilation.

## Decision

### D1: `ProfileKey` — compile-time namespace binding

Shared state uses the same `ProfileKey` trait as profile data. No separate trait or alias is needed because shared state keys have exactly the same shape — a static `KEY` string (the namespace) and a typed `Value`:

```rust
pub trait ProfileKey: 'static + Send + Sync {
    const KEY: &'static str;
    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;
}
```

Type safety is enforced at compile time: callers cannot accidentally read a key of one type and get back a different type. The two dimensions are:

- **Namespace** (`ProfileKey::KEY`) — compile-time, binds to a `Value` type
- **Key** (`&str` parameter) — runtime, identifies which instance

### D2: `StateScope` — convenience key string builder

`StateScope` is a helper type that builds well-known key strings from common scope patterns. It wraps a `String` and provides convenience constructors (`global()`, `parent_thread(id)`, `agent_type(name)`, `thread(id)`, `new(arbitrary)`). Callers use `scope.as_str()` to get the key string. Users can also pass any raw `&str` directly to `ProfileAccess` methods — `StateScope` is optional.

### D3: `ProfileAccess` takes `key: &str`

`ProfileAccess` methods accept a plain `key: &str` parameter for the runtime key dimension. No `ProfileOwner` is needed in the public access API:

- `read::<K>(&self, key: &str) -> Result<K::Value>`
- `write::<K>(&self, key: &str, &value) -> Result<()>`
- `delete::<K>(&self, key: &str) -> Result<()>`

Both shared state and profile state use the same methods — the only difference is the key string convention:

```rust
// Shared state — key is a scope string
let scope = StateScope::parent_thread("pt-1");
access.read::<TeamContext>(scope.as_str()).await?;

// Profile state — key is an agent name or "system"
access.read::<Locale>("alice").await?;
access.write::<Locale>("system", &"en-US".into()).await?;
```

### D4: Four-layer state model

| Layer | Scope | Access | Lifecycle | Use Case |
|-------|-------|--------|-----------|----------|
| Run State | `KeyScope::Run` | Sync, in-memory | Cleared at run start | Hot-path plugin state, permission overrides |
| Thread State | `KeyScope::Global` | Sync, in-memory, transactional | Thread lifetime | Conversation history, handoff state |
| Shared State | `ProfileKey` + `StateScope` | Async, via `ProfileAccess` | Persistent, explicit delete | Cross-agent coordination, global config |
| Profile State | `ProfileKey` + `key: &str` | Async, via `ProfileAccess` | Persistent, key-scoped | Per-agent/user long-lived data |

## Rationale

**Why not extend `StateKey`?** `TypedMap` enforces one value per Rust type. Shared state requires multiple values of the same type under different scope strings. Forcing this into `TypedMap` would require wrapper types per scope, losing ergonomics and defeating the purpose of dynamic scoping.

**Why a plain `&str` key instead of `ProfileOwner`?** The key dimension is just a string partition — it does not imply ownership or access policy. Using `&str` keeps the API simple and avoids coupling storage identity to access semantics. `StateScope` provides structured constructors for common patterns, but any string works.

**Why keep Thread-scoped `StateKey`?** Synchronous, transactional access with optimistic locking is essential for hot-path operations (permission checks, tool interception, catalog rendering). Shared state is async and persistent — wrong trade-off for sub-millisecond phase hooks.

## Consequences

- `awaken-contract` gains `StateScope` as a key string builder.
- `ProfileAccess` methods take `key: &str` — unified API for both profile and shared state.
- `PluginRegistrar::register_profile_key::<K>()` is used to register both profile and shared state keys.
- No new `ProfileAccess` methods needed — existing `read`, `write`, `delete` work for both use cases.
- No changes required to `ProfileStore` or storage adapters.
- Future: watch/subscribe for shared state changes is a natural extension but tracked separately from this ADR.
