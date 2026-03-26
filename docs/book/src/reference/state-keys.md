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

## Related

- [State and Snapshot Model](../explanation/state-and-snapshot-model.md)
