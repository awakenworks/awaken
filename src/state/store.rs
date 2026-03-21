use std::any::TypeId;
use std::sync::{Arc, Mutex, RwLock};

use crate::error::StateError;
use crate::plugins::{InstalledPlugin, KeyRegistration, Plugin, PluginRegistrar, PluginRegistry};

use super::{MutationBatch, Snapshot, StateCommand, StateKey, StateMap};

#[derive(Clone)]
pub struct CommitEvent {
    pub previous_revision: u64,
    pub new_revision: u64,
    pub op_count: usize,
    pub snapshot: Snapshot,
}

pub trait CommitHook: Send + Sync + 'static {
    fn on_commit(&self, event: &CommitEvent);
}

pub struct StateStore {
    pub(crate) inner: Arc<RwLock<Snapshot>>,
    pub(crate) registry: Arc<Mutex<PluginRegistry>>,
    pub(crate) hooks: Arc<RwLock<Vec<Arc<dyn CommitHook>>>>,
}

impl Clone for StateStore {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            registry: Arc::clone(&self.registry),
            hooks: Arc::clone(&self.hooks),
        }
    }
}

impl StateStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Snapshot {
                revision: 0,
                ext: Arc::new(StateMap::default()),
            })),
            registry: Arc::new(Mutex::new(PluginRegistry::default())),
            hooks: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.read().expect("state lock poisoned").clone()
    }

    pub fn revision(&self) -> u64 {
        self.inner.read().expect("state lock poisoned").revision
    }

    pub fn read<K>(&self) -> Option<K::Value>
    where
        K: StateKey,
    {
        let guard = self.inner.read().expect("state lock poisoned");
        guard.get::<K>().cloned()
    }

    pub fn add_hook<H>(&self, hook: H)
    where
        H: CommitHook,
    {
        self.hooks
            .write()
            .expect("hook lock poisoned")
            .push(Arc::new(hook));
    }

    pub fn begin_mutation(&self) -> MutationBatch {
        MutationBatch::new()
    }

    /// Merge two batches from parallel execution using registered merge strategies.
    pub fn merge_parallel(
        &self,
        left: MutationBatch,
        right: MutationBatch,
    ) -> Result<MutationBatch, StateError> {
        let registry = self.registry.lock().expect("registry lock poisoned");
        left.merge_parallel(right, |key| registry.merge_strategy(key))
    }

    /// Merge multiple commands from parallel execution into one.
    pub fn merge_all_commands(
        &self,
        commands: Vec<StateCommand>,
    ) -> Result<StateCommand, StateError> {
        let registry = self.registry.lock().expect("registry lock poisoned");
        commands
            .into_iter()
            .try_fold(StateCommand::new(), |acc, cmd| {
                acc.merge_parallel(cmd, |key| registry.merge_strategy(key))
            })
    }

    pub fn commit(&self, patch: MutationBatch) -> Result<u64, StateError> {
        if patch.is_empty() {
            return Ok(self.revision());
        }

        let op_count = patch.op_len();
        let hooks = self.hooks.read().expect("hook lock poisoned").clone();

        let registry = self.registry.lock().expect("registry lock poisoned");
        let mut state = self.inner.write().expect("state lock poisoned");

        if let Some(expected) = patch.base_revision
            && state.revision != expected
        {
            return Err(StateError::RevisionConflict {
                expected,
                actual: state.revision,
            });
        }

        for key in &patch.touched_keys {
            registry.ensure_key(key)?;
        }

        let previous_revision = state.revision;
        for op in patch.ops {
            op.apply(&mut state);
        }
        state.revision += 1;
        let new_revision = state.revision;
        let snapshot = state.clone();
        drop(state);
        drop(registry);

        let event = CommitEvent {
            previous_revision,
            new_revision,
            op_count,
            snapshot,
        };
        for hook in hooks {
            hook.on_commit(&event);
        }

        Ok(new_revision)
    }

    pub fn install_plugin<P>(&self, plugin: P) -> Result<(), StateError>
    where
        P: Plugin,
    {
        let mut registrar = PluginRegistrar::new();
        plugin.register(&mut registrar)?;
        let plugin_type_id = TypeId::of::<P>();
        self.install_plugin_with_keys(plugin_type_id, Arc::new(plugin), registrar.keys)
    }

    pub(crate) fn install_plugin_with_keys(
        &self,
        plugin_type_id: TypeId,
        plugin: Arc<dyn Plugin>,
        registrations: Vec<KeyRegistration>,
    ) -> Result<(), StateError> {
        let descriptor = plugin.descriptor();

        {
            let mut registry = self.registry.lock().expect("registry lock poisoned");
            if registry.plugins.contains_key(&plugin_type_id) {
                return Err(StateError::PluginAlreadyInstalled {
                    name: descriptor.name.to_string(),
                });
            }

            for reg in &registrations {
                if registry.keys_by_name.contains_key(&reg.key) {
                    return Err(StateError::KeyAlreadyRegistered {
                        key: reg.key.clone(),
                    });
                }
            }

            for reg in &registrations {
                registry.keys_by_name.insert(reg.key.clone(), reg.clone());
                registry.keys_by_type.insert(reg.type_id, reg.clone());
            }

            registry.plugins.insert(
                plugin_type_id,
                InstalledPlugin {
                    plugin: Arc::clone(&plugin),
                    owned_key_type_ids: registrations.iter().map(|r| r.type_id).collect(),
                },
            );
        }

        let mut patch = MutationBatch::new().with_base_revision(self.revision());
        plugin.on_install(&mut patch)?;
        self.commit(patch).map(|_| ()).inspect_err(|_| {
            let _ = self.unregister_plugin_type_id(plugin_type_id, true);
        })
    }

    pub fn uninstall_plugin<P>(&self) -> Result<(), StateError>
    where
        P: Plugin,
    {
        let plugin_type_id = TypeId::of::<P>();
        let (plugin, registrations) =
            {
                let registry = self.registry.lock().expect("registry lock poisoned");
                let installed = registry.plugins.get(&plugin_type_id).ok_or(
                    StateError::PluginNotInstalled {
                        type_name: std::any::type_name::<P>(),
                    },
                )?;
                let regs = installed
                    .owned_key_type_ids
                    .iter()
                    .filter_map(|type_id| registry.keys_by_type.get(type_id).cloned())
                    .collect::<Vec<_>>();
                (Arc::clone(&installed.plugin), regs)
            };

        let mut patch = MutationBatch::new().with_base_revision(self.revision());
        plugin.on_uninstall(&mut patch)?;
        for reg in &registrations {
            if !reg.options.retain_on_uninstall {
                patch.clear_extension_with(reg.key.clone(), reg.clear);
            }
        }
        self.commit(patch).map(|_| ())?;
        self.unregister_plugin_type_id(plugin_type_id, false)
    }

    fn unregister_plugin_type_id(
        &self,
        plugin_type_id: TypeId,
        rollback_install: bool,
    ) -> Result<(), StateError> {
        let removed =
            {
                let mut registry = self.registry.lock().expect("registry lock poisoned");
                let installed = registry.plugins.remove(&plugin_type_id).ok_or(
                    StateError::PluginNotInstalled {
                        type_name: "unknown",
                    },
                )?;

                let mut removed = Vec::new();
                for type_id in &installed.owned_key_type_ids {
                    if let Some(reg) = registry.keys_by_type.remove(type_id) {
                        registry.keys_by_name.remove(&reg.key);
                        removed.push(reg);
                    }
                }
                removed
            };

        let _ = rollback_install;
        let _ = removed;
        Ok(())
    }
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}
