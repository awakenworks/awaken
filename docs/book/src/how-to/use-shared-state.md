# Use Shared State

Use this when agents need to share persistent state across thread boundaries, agent types, or delegation trees. Shared state lives in the `ProfileStore` and is addressed by a typed **namespace** (`ProfileKey`) and a **key** (`&str`), giving you fine-grained control over who sees what.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- A `ProfileStore` backend configured on the runtime (e.g. file store or Postgres)

## Concepts

Shared state has two dimensions:

| Dimension | Type | Purpose |
|-----------|------|---------|
| **Namespace** | `ProfileKey` | Defines *what* is stored — a compile-time binding between a static string key (`KEY`) and a typed `Value`. Each key is registered once per plugin via `register_profile_key`. |
| **Key** | `&str` (or `StateScope` helper) | Defines *which instance* — a runtime string that partitions storage. Different keys isolate or share data between agents and threads. |

Together, `(ProfileKey::KEY, key: &str)` uniquely identifies a shared state entry in the profile store.

## Steps

### 1. Define a shared state key

Create a struct that implements `ProfileKey`. The `KEY` constant is the namespace; the `Value` type is what gets serialized.

```rust,ignore
use serde::{Deserialize, Serialize};
use awaken_contract::ProfileKey;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TeamContext {
    pub goal: String,
    pub constraints: Vec<String>,
}

pub struct TeamContextKey;

impl ProfileKey for TeamContextKey {
    const KEY: &'static str = "team_context";
    type Value = TeamContext;
}
```

### 2. Register in a plugin

Inside your plugin's `register` method, call `register_profile_key` on the registrar.

```rust,ignore
use awaken_contract::StateError;
use awaken_runtime::plugins::registry::PluginRegistrar;

fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
    r.register_profile_key::<TeamContextKey>()?;
    Ok(())
}
```

### 3. Read and write in a hook

In any phase hook, obtain `ProfileAccess` from the context and use `read` / `write` with a key string. `StateScope` is a convenience builder for common key patterns — call `.as_str()` to get the key.

```rust,ignore
use awaken_contract::StateScope;

async fn execute(&self, ctx: &mut PhaseContext) -> Result<(), anyhow::Error> {
    let profile = ctx.profile().expect("ProfileStore not configured");
    let identity = ctx.snapshot().run_identity();

    // Build a scope key from the current agent's parent thread
    let scope = match &identity.parent_thread_id {
        Some(pid) => StateScope::parent_thread(pid),
        None => StateScope::global(),
    };

    // Read (returns TeamContext::default() if missing)
    let mut team: TeamContext = profile.read::<TeamContextKey>(scope.as_str()).await?;

    // Mutate and write back
    team.goal = "Ship the feature".into();
    profile.write::<TeamContextKey>(scope.as_str(), &team).await?;

    Ok(())
}
```

### 4. Choose the right scope

`StateScope` has several constructors. Pick the one that matches your sharing pattern:

| Scenario | Scope | Example |
|----------|-------|---------|
| All agents across all threads | `StateScope::global()` | Org-wide configuration |
| All agents spawned from the same parent thread | `StateScope::parent_thread(id)` | A delegation tree sharing context |
| All instances of the same agent type | `StateScope::agent_type(name)` | Planner agents sharing learned heuristics |
| Single thread only | `StateScope::thread(id)` | Thread-local scratchpad |
| Custom partition | `StateScope::new("custom-key")` | Any application-defined grouping |

You can also pass any raw `&str` directly — `StateScope` is optional convenience.

## When to use shared state

| Mechanism | Lifetime | Scope | Best for |
|-----------|----------|-------|----------|
| `StateKey` | Single run (in-memory snapshot) | One agent thread | Transient per-run state (counters, flags, accumulated context) |
| `ProfileKey` with agent/system key | Persistent (profile store) | Per-agent or system | Per-agent or per-user settings that don't cross boundaries |
| `ProfileKey` with `StateScope` key | Persistent (profile store) | Any `StateScope` string | Cross-agent, cross-thread persistent state |

Use `ProfileKey` with a `StateScope` key when state must survive across runs **and** be visible to agents in different threads or of different types.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| `profile key not registered: <ns>` | Key not registered in any plugin | Call `r.register_profile_key::<YourKey>()` in the plugin's `register` method |
| Always reads `Value::default()` | Writing and reading use different key strings | Verify both sides construct the same `StateScope` or use the same `&str` key |
| Data leaks between scopes | Using `StateScope::global()` when a narrower scope is needed | Switch to `parent_thread`, `agent_type`, or `thread` scope |

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-contract/src/contract/shared_state.rs` | `StateScope` type |
| `crates/awaken-contract/src/contract/profile_store.rs` | `ProfileKey` trait, `ProfileOwner` enum |
| `crates/awaken-runtime/src/profile/mod.rs` | `ProfileAccess` with `read`, `write`, `delete` methods |
| `crates/awaken-runtime/src/plugins/registry.rs` | `PluginRegistrar::register_profile_key` registration |

## Related

- [State and Snapshot Model](../explanation/state-and-snapshot-model.md)
- [State Keys](../reference/state-keys.md)
- [Add a Plugin](./add-a-plugin.md)
