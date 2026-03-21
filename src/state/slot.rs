use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use typedmap::clone::SyncCloneBounds;
use typedmap::{TypedMap, TypedMapKey};

use crate::error::StateError;
use crate::model::{JsonValue, decode_json, encode_json};

struct ExtensionMarker;

struct TypedKey<K>(PhantomData<fn() -> K>);

impl<K> TypedKey<K> {
    const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<K> Clone for TypedKey<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for TypedKey<K> {}

impl<K> PartialEq for TypedKey<K> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<K> Eq for TypedKey<K> {}

impl<K: 'static> Hash for TypedKey<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<K>().hash(state);
    }
}

impl<K> TypedMapKey<ExtensionMarker> for TypedKey<K>
where
    K: StateKey,
{
    type Value = K::Value;
}

pub struct StateMap {
    values: TypedMap<ExtensionMarker, SyncCloneBounds, SyncCloneBounds>,
}

impl Default for StateMap {
    fn default() -> Self {
        Self {
            values: TypedMap::new_with_bounds(),
        }
    }
}

impl Clone for StateMap {
    fn clone(&self) -> Self {
        let mut values = TypedMap::new_with_bounds();
        for entry in self.values.iter() {
            values.insert_key_value(entry.to_owned());
        }
        Self { values }
    }
}

impl StateMap {
    pub fn contains<K: StateKey>(&self) -> bool {
        self.values.contains_key(&TypedKey::<K>::new())
    }

    pub fn get<K: StateKey>(&self) -> Option<&K::Value> {
        self.values.get(&TypedKey::<K>::new())
    }

    pub fn get_mut<K: StateKey>(&mut self) -> Option<&mut K::Value> {
        self.values.get_mut(&TypedKey::<K>::new())
    }

    pub fn insert<K: StateKey>(&mut self, value: K::Value) {
        self.values.insert(TypedKey::<K>::new(), value);
    }

    pub fn remove<K: StateKey>(&mut self) -> Option<K::Value> {
        self.values.remove(&TypedKey::<K>::new())
    }

    pub fn get_or_insert_default<K: StateKey>(&mut self) -> &mut K::Value {
        if !self.contains::<K>() {
            self.insert::<K>(K::Value::default());
        }

        self.get_mut::<K>()
            .expect("value should exist after insertion")
    }
}

/// Parallel merge strategy for a state key.
///
/// Determines how concurrent updates to the same key are handled
/// when merging `MutationBatch`es from parallel tool execution.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    /// Concurrent updates to this key are mutually exclusive.
    /// Parallel batches that both touch this key cannot be merged.
    #[default]
    Exclusive,
    /// Updates to this key are commutative — they can be applied
    /// in any order and produce the same result. Parallel batches
    /// that both touch this key will have their ops concatenated.
    Commutative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateKeyOptions {
    pub persistent: bool,
    pub retain_on_uninstall: bool,
}

impl Default for StateKeyOptions {
    fn default() -> Self {
        Self {
            persistent: true,
            retain_on_uninstall: false,
        }
    }
}

pub trait StateKey: 'static + Send + Sync {
    const KEY: &'static str;

    /// Parallel merge strategy. Default: `Exclusive` (conflict on concurrent writes).
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;

    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;
    type Update: Send + 'static;

    fn apply(value: &mut Self::Value, update: Self::Update);

    fn encode(value: &Self::Value) -> Result<JsonValue, StateError> {
        encode_json(Self::KEY, value)
    }

    fn decode(value: JsonValue) -> Result<Self::Value, StateError> {
        decode_json(Self::KEY, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter;

    impl StateKey for Counter {
        const KEY: &'static str = "counter";
        type Value = usize;
        type Update = usize;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value += update;
        }
    }

    #[test]
    fn state_map_can_store_and_update_typed_values() {
        let mut slots = StateMap::default();
        Counter::apply(slots.get_or_insert_default::<Counter>(), 2);
        Counter::apply(slots.get_or_insert_default::<Counter>(), 3);

        assert_eq!(slots.get::<Counter>().copied(), Some(5));
        assert_eq!(slots.remove::<Counter>(), Some(5));
        assert!(!slots.contains::<Counter>());
    }
}
