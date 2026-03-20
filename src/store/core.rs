use crate::foundation::*;
use crate::patch::MutationBatch;
use crate::plugin::{InstalledPlugin, PluginRegistrar, PluginRegistry, StatePlugin};
use std::any::TypeId;
use std::sync::{Arc, Mutex, RwLock};

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
                ext: Arc::new(SlotMap::default()),
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

    pub fn read_slot<K>(&self) -> Option<K::Value>
    where
        K: StateSlot,
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

        for key in &patch.touched_slot_keys {
            registry.ensure_slot(key)?;
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
        P: StatePlugin,
    {
        let meta = plugin.meta();
        let mut registrar = PluginRegistrar::new();
        plugin.register(&mut registrar)?;

        let plugin_type_id = TypeId::of::<P>();
        let plugin_arc: Arc<dyn StatePlugin> = Arc::new(plugin);

        {
            let mut registry = self.registry.lock().expect("registry lock poisoned");
            if registry.plugins.contains_key(&plugin_type_id) {
                return Err(StateError::PluginAlreadyInstalled {
                    name: meta.name.to_string(),
                });
            }

            for slot in &registrar.slots {
                if registry.slots_by_key.contains_key(&slot.key) {
                    return Err(StateError::SlotAlreadyRegistered {
                        key: slot.key.clone(),
                    });
                }
            }

            for slot in &registrar.slots {
                registry.slots_by_key.insert(slot.key.clone(), slot.clone());
                registry.slots_by_type.insert(slot.type_id, slot.clone());
            }

            registry.plugins.insert(
                plugin_type_id,
                InstalledPlugin {
                    plugin: Arc::clone(&plugin_arc),
                    owned_slot_type_ids: registrar.slots.iter().map(|slot| slot.type_id).collect(),
                },
            );
        }

        let mut patch = MutationBatch::new().with_base_revision(self.revision());
        plugin_arc.on_install(&mut patch)?;
        self.commit(patch).map(|_| ()).inspect_err(|_| {
            let _ = self.unregister_plugin_type_id(plugin_type_id, true);
        })
    }

    pub fn uninstall_plugin<P>(&self) -> Result<(), StateError>
    where
        P: StatePlugin,
    {
        let plugin_type_id = TypeId::of::<P>();
        let (plugin, slots) =
            {
                let registry = self.registry.lock().expect("registry lock poisoned");
                let installed = registry.plugins.get(&plugin_type_id).ok_or(
                    StateError::PluginNotInstalled {
                        type_name: std::any::type_name::<P>(),
                    },
                )?;
                let slots = installed
                    .owned_slot_type_ids
                    .iter()
                    .filter_map(|type_id| registry.slots_by_type.get(type_id).cloned())
                    .collect::<Vec<_>>();
                (Arc::clone(&installed.plugin), slots)
            };

        let mut patch = MutationBatch::new().with_base_revision(self.revision());
        plugin.on_uninstall(&mut patch)?;
        for slot in &slots {
            if !slot.options.retain_on_uninstall {
                patch.clear_extension_with(slot.key.clone(), slot.clear);
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
                for type_id in &installed.owned_slot_type_ids {
                    if let Some(slot) = registry.slots_by_type.remove(type_id) {
                        registry.slots_by_key.remove(&slot.key);
                        removed.push(slot);
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
