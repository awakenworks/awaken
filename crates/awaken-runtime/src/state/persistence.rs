use std::sync::Arc;

use crate::state::KeyScope;
use awaken_contract::{StateError, UnknownKeyPolicy};

use super::{PersistedState, StateMap, StateStore};

impl StateStore {
    pub fn export_persisted(&self) -> Result<PersistedState, StateError> {
        let registry = self.registry.lock();
        let state = self.inner.read();
        let mut extensions = std::collections::HashMap::new();

        for reg in registry.keys_by_type.values() {
            if !reg.options.persistent {
                continue;
            }

            if let Some(json) = (reg.export)(state.ext.as_ref()).map_err(|err| match err {
                StateError::KeyEncode { key, message } => StateError::KeyEncode { key, message },
                other => StateError::KeyEncode {
                    key: reg.key.clone(),
                    message: other.to_string(),
                },
            })? {
                extensions.insert(reg.key.clone(), json);
            }
        }

        Ok(PersistedState {
            revision: state.revision,
            extensions,
        })
    }

    pub fn restore_persisted(
        &self,
        persisted: PersistedState,
        unknown_policy: UnknownKeyPolicy,
    ) -> Result<(), StateError> {
        let registry = self.registry.lock();
        let mut next_ext = StateMap::default();

        for (key, json) in persisted.extensions {
            let Some(reg) = registry.keys_by_name.get(&key) else {
                match unknown_policy {
                    UnknownKeyPolicy::Error => return Err(StateError::UnknownKey { key }),
                    UnknownKeyPolicy::Skip => continue,
                }
            };

            (reg.import)(&mut next_ext, json).map_err(|err| match err {
                StateError::KeyDecode { key, message } => StateError::KeyDecode { key, message },
                other => StateError::KeyDecode {
                    key: reg.key.clone(),
                    message: other.to_string(),
                },
            })?;
        }

        let mut state = self.inner.write();
        state.ext = Arc::new(next_ext);
        state.revision = persisted.revision;
        Ok(())
    }

    /// Restore only `Thread`-scoped keys from a persisted state snapshot.
    ///
    /// Run-scoped keys in `persisted` are ignored. Unknown keys follow `unknown_policy`.
    pub fn restore_thread_scoped(
        &self,
        persisted: PersistedState,
        unknown_policy: UnknownKeyPolicy,
    ) -> Result<(), StateError> {
        let registry = self.registry.lock();
        let mut state = self.inner.write();
        let ext = Arc::make_mut(&mut state.ext);

        for (key, json) in persisted.extensions {
            let Some(reg) = registry.keys_by_name.get(&key) else {
                match unknown_policy {
                    UnknownKeyPolicy::Error => return Err(StateError::UnknownKey { key }),
                    UnknownKeyPolicy::Skip => continue,
                }
            };

            if reg.scope != KeyScope::Thread {
                continue;
            }

            (reg.import)(ext, json).map_err(|err| match err {
                StateError::KeyDecode { key, message } => StateError::KeyDecode { key, message },
                other => StateError::KeyDecode {
                    key: reg.key.clone(),
                    message: other.to_string(),
                },
            })?;
        }

        Ok(())
    }

    /// Clear all `Run`-scoped keys, preserving `Thread`-scoped keys.
    pub fn clear_run_scoped(&self) {
        let registry = self.registry.lock();
        let mut state = self.inner.write();
        let ext = Arc::make_mut(&mut state.ext);

        for reg in registry.keys_by_type.values() {
            if reg.scope == KeyScope::Run {
                (reg.clear)(ext);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
    use crate::state::{StateKey, StateKeyOptions};
    use awaken_contract::UnknownKeyPolicy;

    struct PersistentCounter;

    impl StateKey for PersistentCounter {
        const KEY: &'static str = "test.persist_counter";
        type Value = i64;
        type Update = i64;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value += update;
        }
    }

    struct TransientFlag;

    impl StateKey for TransientFlag {
        const KEY: &'static str = "test.transient_flag";
        type Value = bool;
        type Update = bool;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value = update;
        }
    }

    struct PersistenceTestPlugin;

    impl Plugin for PersistenceTestPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "persistence-test-plugin",
            }
        }

        fn register(
            &self,
            registrar: &mut PluginRegistrar,
        ) -> Result<(), awaken_contract::StateError> {
            registrar.register_key::<PersistentCounter>(StateKeyOptions {
                persistent: true,
                ..Default::default()
            })?;
            registrar.register_key::<TransientFlag>(StateKeyOptions {
                persistent: false,
                ..Default::default()
            })?;
            Ok(())
        }
    }

    #[test]
    fn export_import_roundtrip() {
        let store = StateStore::new();
        store.install_plugin(PersistenceTestPlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<PersistentCounter>(42);
        store.commit(batch).unwrap();

        let exported = store.export_persisted().unwrap();

        // Create a new store, install same plugin, restore
        let store2 = StateStore::new();
        store2.install_plugin(PersistenceTestPlugin).unwrap();
        store2
            .restore_persisted(exported, UnknownKeyPolicy::Error)
            .unwrap();

        let val = store2.read::<PersistentCounter>().unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn export_skips_non_persistent_keys() {
        let store = StateStore::new();
        store.install_plugin(PersistenceTestPlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<PersistentCounter>(10);
        batch.update::<TransientFlag>(true);
        store.commit(batch).unwrap();

        let exported = store.export_persisted().unwrap();

        // Only the persistent key should be in the export
        assert!(
            exported.extensions.contains_key(PersistentCounter::KEY),
            "persistent key should be exported"
        );
        assert!(
            !exported.extensions.contains_key(TransientFlag::KEY),
            "non-persistent key should NOT be exported"
        );
    }

    #[test]
    fn import_unknown_key_with_skip_policy() {
        let store = StateStore::new();
        store.install_plugin(PersistenceTestPlugin).unwrap();

        // Build a PersistedState with an unknown key
        let mut extensions = std::collections::HashMap::new();
        extensions.insert("unknown.key".to_string(), serde_json::json!("some_value"));
        let persisted = PersistedState {
            revision: 5,
            extensions,
        };

        // Should succeed with Skip policy
        let result = store.restore_persisted(persisted, UnknownKeyPolicy::Skip);
        assert!(
            result.is_ok(),
            "skip policy should not error on unknown keys"
        );
    }
}
