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
