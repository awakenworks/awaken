# State Keys

The state system provides typed, scoped, persistent key-value storage for agent
runs. Plugins and tools define state keys at compile time; the runtime manages
snapshots, persistence, and parallel merge semantics.

## StateKey trait

Every state slot is identified by a type implementing `StateKey`.

```rust,ignore
pub trait StateKey: 'static + Send + Sync {
    /// Unique string identifier for serialization.
    const KEY: &'static str;

    /// Parallel merge strategy. Default: `Exclusive`.
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;

    /// Lifetime scope. Default: `Run`.
    const SCOPE: KeyScope = KeyScope::Run;

    /// The stored value type.
    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;

    /// The update command type.
    type Update: Send + 'static;

    /// Apply an update to the current value.
    fn apply(value: &mut Self::Value, update: Self::Update);

    /// Serialize value to JSON. Default uses serde_json.
    fn encode(value: &Self::Value) -> Result<JsonValue, StateError>;

    /// Deserialize value from JSON. Default uses serde_json.
    fn decode(value: JsonValue) -> Result<Self::Value, StateError>;
}
```

**Crate path:** `awaken::state::StateKey` (re-exported at `awaken::StateKey`)

### Example

```rust,ignore
struct Counter;

impl StateKey for Counter {
    const KEY: &'static str = "counter";
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}
```

## KeyScope

Controls when a key's value is cleared relative to run boundaries.

```rust,ignore
pub enum KeyScope {
    /// Cleared at run start (default).
    Run,
    /// Persists across runs on the same thread.
    Thread,
}
```

## MergeStrategy

Determines how concurrent updates to the same key are handled when merging
`MutationBatch`es from parallel tool execution.

```rust,ignore
pub enum MergeStrategy {
    /// Concurrent writes to this key conflict. Parallel batches that both
    /// touch this key cannot be merged.
    Exclusive,
    /// Updates are commutative -- they can be applied in any order. Parallel
    /// batches that both touch this key will have their ops concatenated.
    Commutative,
}
```

## StateMap

A type-erased container for state values. Backed by `TypedMap` from the
`typedmap` crate.

```rust,ignore
pub struct StateMap { /* ... */ }
```

### Methods

```rust,ignore
/// Check if a key is present.
fn contains<K: StateKey>(&self) -> bool

/// Get a reference to the value.
fn get<K: StateKey>(&self) -> Option<&K::Value>

/// Get a mutable reference to the value.
fn get_mut<K: StateKey>(&mut self) -> Option<&mut K::Value>

/// Insert a value, replacing any existing one.
fn insert<K: StateKey>(&mut self, value: K::Value)

/// Remove and return the value.
fn remove<K: StateKey>(&mut self) -> Option<K::Value>

/// Get a mutable reference, inserting `Default::default()` if absent.
fn get_or_insert_default<K: StateKey>(&mut self) -> &mut K::Value
```

`StateMap` implements `Clone` and `Default`.

## Snapshot

An immutable, versioned view of the state map. Passed to tools via
`ToolCallContext`.

```rust,ignore
pub struct Snapshot {
    pub revision: u64,
    pub ext: Arc<StateMap>,
}
```

### Methods

```rust,ignore
fn new(revision: u64, ext: Arc<StateMap>) -> Self
fn revision(&self) -> u64
fn get<K: StateKey>(&self) -> Option<&K::Value>
fn ext(&self) -> &StateMap
```

**Type alias:** `Snapshot` is not a type alias; it is a concrete struct that
wraps `Arc<StateMap>`.

## StateKeyOptions

Declarative options used when registering a state key with the runtime.

```rust,ignore
pub struct StateKeyOptions {
    /// Whether the key is persisted to the store. Default: `true`.
    pub persistent: bool,
    /// Whether the key survives plugin uninstall. Default: `false`.
    pub retain_on_uninstall: bool,
    /// Lifetime scope. Default: `KeyScope::Run`.
    pub scope: KeyScope,
}
```

## PersistedState

Serialized form of the state map used by storage backends.

```rust,ignore
pub struct PersistedState {
    pub revision: u64,
    pub extensions: HashMap<String, JsonValue>,
}
```

## Shared State (ProfileKey + StateScope)

For cross-boundary persistent state with dynamic scoping. Shared state uses `ProfileKey` --
the same trait used for profile data -- combined with a `key: &str` parameter for the runtime
key dimension. Unlike `StateKey`, shared state is accessed asynchronously through `ProfileAccess`
and does not participate in the snapshot/mutation workflow.

```rust,ignore
pub trait ProfileKey: 'static + Send + Sync {
    /// Namespace identifier (used as the storage namespace).
    const KEY: &'static str;

    /// The value type stored under this key.
    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Serialize value to JSON.
    fn encode(value: &Self::Value) -> Result<JsonValue, StateError>;

    /// Deserialize value from JSON.
    fn decode(value: JsonValue) -> Result<Self::Value, StateError>;
}
```

The two dimensions for both shared and profile state are:
- **Namespace** (`ProfileKey::KEY`) -- compile-time, binds to a `Value` type
- **Key** (`&str` parameter) -- runtime, identifies which instance

For shared state, the key is typically a `StateScope` string; for profile state, it is an agent name or `"system"`.

**Crate path:** `awaken_contract::ProfileKey` (re-exported from `awaken_contract::contract::profile_store`)

### StateScope

```rust,ignore
pub struct StateScope(String);
```

Optional convenience builder for common key string patterns. Constructors:

| Method | Produced String | Use Case |
|--------|----------------|----------|
| `StateScope::global()` | `"global"` | System-wide shared state |
| `StateScope::parent_thread(id)` | `"parent_thread::{id}"` | Parent-child agent sharing |
| `StateScope::agent_type(name)` | `"agent_type::{name}"` | Per-agent-type sharing |
| `StateScope::thread(id)` | `"thread::{id}"` | Thread-local persistent state |
| `StateScope::new(s)` | `"{s}"` | Arbitrary grouping |

Call `.as_str()` to get the key string. Users can also pass any raw `&str` directly.

### ProfileAccess Methods

`ProfileAccess` methods take `key: &str` for the runtime key dimension. The same methods
serve both shared and profile state:

```rust,ignore
impl ProfileAccess {
    async fn read<K: ProfileKey>(&self, key: &str) -> Result<K::Value, StorageError>;
    async fn write<K: ProfileKey>(&self, key: &str, value: &K::Value) -> Result<(), StorageError>;
    async fn delete<K: ProfileKey>(&self, key: &str) -> Result<(), StorageError>;
}

// Shared state usage:
let scope = StateScope::global();
let value = access.read::<MyKey>(scope.as_str()).await?;
access.write::<MyKey>(scope.as_str(), &value).await?;

// Profile state usage:
let locale = access.read::<Locale>("alice").await?;
access.write::<Locale>("system", &"en-US".into()).await?;
```

### Registration

```rust,ignore
impl PluginRegistrar {
    /// Register a profile key (used for both profile state and shared state).
    pub fn register_profile_key<K: ProfileKey>(&mut self) -> Result<(), StateError>;
}
```

## Related

- [State and Snapshot Model](../explanation/state-and-snapshot-model.md)
- [Use Shared State](../how-to/use-shared-state.md)
