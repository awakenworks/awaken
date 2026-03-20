//! ConfigSlot trait and ConfigMap — typed heterogeneous configuration container.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any::TypeId;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use typedmap::clone::SyncCloneBounds;
use typedmap::{TypedMap, TypedMapKey};

/// Typed configuration slot — parallel to `StateSlot` but without Update/apply.
///
/// Plugins define their own config types by implementing this trait.
/// Values are whole-replacement (not incrementally reduced).
pub trait ConfigSlot: 'static + Send + Sync {
    const KEY: &'static str;
    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;
}

// -- ConfigMap internals --

struct ConfigMarker;

struct ConfigKey<C>(PhantomData<fn() -> C>);

impl<C> ConfigKey<C> {
    const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<C> Clone for ConfigKey<C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for ConfigKey<C> {}

impl<C> PartialEq for ConfigKey<C> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<C> Eq for ConfigKey<C> {}

impl<C: 'static> Hash for ConfigKey<C> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        TypeId::of::<C>().hash(state);
    }
}

impl<C> TypedMapKey<ConfigMarker> for ConfigKey<C>
where
    C: ConfigSlot,
{
    type Value = C::Value;
}

/// Typed heterogeneous configuration container.
///
/// Uses the same `TypedMap` infrastructure as `SlotMap` but with a
/// separate marker type to prevent accidental mixing.
#[derive(Default)]
pub struct ConfigMap {
    values: TypedMap<ConfigMarker, SyncCloneBounds, SyncCloneBounds>,
}

impl Clone for ConfigMap {
    fn clone(&self) -> Self {
        let mut values = TypedMap::new_with_bounds();
        for entry in self.values.iter() {
            values.insert_key_value(entry.to_owned());
        }
        Self { values }
    }
}

impl std::fmt::Debug for ConfigMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigMap")
            .field("len", &self.values.len())
            .finish()
    }
}

impl ConfigMap {
    /// Create an empty config map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a config value.
    pub fn set<C: ConfigSlot>(&mut self, value: C::Value) {
        self.values.insert(ConfigKey::<C>::new(), value);
    }

    /// Get a config value by type.
    pub fn get<C: ConfigSlot>(&self) -> Option<&C::Value> {
        self.values.get(&ConfigKey::<C>::new())
    }

    /// Get a config value, returning the type's default if not set.
    pub fn get_or_default<C: ConfigSlot>(&self) -> C::Value {
        self.get::<C>().cloned().unwrap_or_default()
    }

    /// Check if a config slot is set.
    pub fn contains<C: ConfigSlot>(&self) -> bool {
        self.values.contains_key(&ConfigKey::<C>::new())
    }

    /// Remove a config value, returning it if present.
    pub fn remove<C: ConfigSlot>(&mut self) -> Option<C::Value> {
        self.values.remove(&ConfigKey::<C>::new())
    }

    /// Number of config entries.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Merge another ConfigMap on top (other's values override self's).
    pub fn merge_from(&mut self, other: &ConfigMap) {
        for entry in other.values.iter() {
            self.values.insert_key_value(entry.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    // Test config types

    struct ModelConfig;
    impl ConfigSlot for ModelConfig {
        const KEY: &'static str = "model";
        type Value = ModelSettings;
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct ModelSettings {
        model: String,
        temperature: Option<u32>, // scaled by 100
    }

    struct PermConfig;
    impl ConfigSlot for PermConfig {
        const KEY: &'static str = "permission";
        type Value = PermSettings;
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct PermSettings {
        rules: Vec<String>,
    }

    #[test]
    fn config_map_set_and_get() {
        let mut map = ConfigMap::new();
        map.set::<ModelConfig>(ModelSettings {
            model: "gpt-4o".into(),
            temperature: Some(70),
        });
        let val = map.get::<ModelConfig>().unwrap();
        assert_eq!(val.model, "gpt-4o");
        assert_eq!(val.temperature, Some(70));
    }

    #[test]
    fn config_map_get_missing_returns_none() {
        let map = ConfigMap::new();
        assert!(map.get::<ModelConfig>().is_none());
    }

    #[test]
    fn config_map_get_or_default() {
        let map = ConfigMap::new();
        let val = map.get_or_default::<ModelConfig>();
        assert_eq!(val, ModelSettings::default());
    }

    #[test]
    fn config_map_contains() {
        let mut map = ConfigMap::new();
        assert!(!map.contains::<ModelConfig>());
        map.set::<ModelConfig>(ModelSettings::default());
        assert!(map.contains::<ModelConfig>());
    }

    #[test]
    fn config_map_remove() {
        let mut map = ConfigMap::new();
        map.set::<ModelConfig>(ModelSettings {
            model: "x".into(),
            temperature: None,
        });
        let removed = map.remove::<ModelConfig>().unwrap();
        assert_eq!(removed.model, "x");
        assert!(map.get::<ModelConfig>().is_none());
    }

    #[test]
    fn config_map_multiple_types() {
        let mut map = ConfigMap::new();
        map.set::<ModelConfig>(ModelSettings {
            model: "gpt-4o".into(),
            temperature: None,
        });
        map.set::<PermConfig>(PermSettings {
            rules: vec!["allow_read".into()],
        });
        assert_eq!(map.len(), 2);
        assert_eq!(map.get::<ModelConfig>().unwrap().model, "gpt-4o");
        assert_eq!(map.get::<PermConfig>().unwrap().rules, vec!["allow_read"]);
    }

    #[test]
    fn config_map_set_overwrites() {
        let mut map = ConfigMap::new();
        map.set::<ModelConfig>(ModelSettings {
            model: "a".into(),
            temperature: None,
        });
        map.set::<ModelConfig>(ModelSettings {
            model: "b".into(),
            temperature: Some(50),
        });
        assert_eq!(map.get::<ModelConfig>().unwrap().model, "b");
    }

    #[test]
    fn config_map_clone() {
        let mut map = ConfigMap::new();
        map.set::<ModelConfig>(ModelSettings {
            model: "gpt-4o".into(),
            temperature: None,
        });
        let cloned = map.clone();
        assert_eq!(cloned.get::<ModelConfig>().unwrap().model, "gpt-4o");
    }

    #[test]
    fn config_map_merge_from() {
        let mut base = ConfigMap::new();
        base.set::<ModelConfig>(ModelSettings {
            model: "base".into(),
            temperature: Some(50),
        });
        base.set::<PermConfig>(PermSettings {
            rules: vec!["base_rule".into()],
        });

        let mut overlay = ConfigMap::new();
        overlay.set::<ModelConfig>(ModelSettings {
            model: "override".into(),
            temperature: None,
        });

        base.merge_from(&overlay);

        // ModelConfig overridden
        assert_eq!(base.get::<ModelConfig>().unwrap().model, "override");
        // PermConfig preserved
        assert_eq!(base.get::<PermConfig>().unwrap().rules, vec!["base_rule"]);
    }

    #[test]
    fn config_map_is_empty() {
        let mut map = ConfigMap::new();
        assert!(map.is_empty());
        map.set::<ModelConfig>(ModelSettings::default());
        assert!(!map.is_empty());
    }
}
