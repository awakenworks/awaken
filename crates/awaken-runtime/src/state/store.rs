use std::any::TypeId;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::plugins::{InstalledPlugin, KeyRegistration, Plugin, PluginRegistrar, PluginRegistry};
use awaken_contract::StateError;

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
        self.inner.read().clone()
    }

    pub fn revision(&self) -> u64 {
        self.inner.read().revision
    }

    pub fn read<K>(&self) -> Option<K::Value>
    where
        K: StateKey,
    {
        let guard = self.inner.read();
        guard.get::<K>().cloned()
    }

    pub fn add_hook<H>(&self, hook: H)
    where
        H: CommitHook,
    {
        self.hooks.write().push(Arc::new(hook));
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
        let registry = self.registry.lock();
        left.merge_parallel(right, |key| registry.merge_strategy(key))
    }

    /// Merge multiple commands from parallel execution into one.
    pub fn merge_all_commands(
        &self,
        commands: Vec<StateCommand>,
    ) -> Result<StateCommand, StateError> {
        let registry = self.registry.lock();
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
        let hooks = self.hooks.read().clone();

        let registry = self.registry.lock();
        let mut state = self.inner.write();

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
            let mut registry = self.registry.lock();
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
                    owned_key_type_ids: registrations.iter().map(|r| r.type_id).collect(),
                },
            );
        }

        Ok(())
    }

    /// Register standalone state keys (not owned by any plugin).
    ///
    /// Keys that are already registered are silently skipped.
    /// This is used to install plugin-declared state keys collected by
    /// `ExecutionEnv::from_plugins()`.
    pub(crate) fn register_keys(
        &self,
        registrations: &[KeyRegistration],
    ) -> Result<(), StateError> {
        let mut registry = self.registry.lock();
        for reg in registrations {
            if registry.keys_by_name.contains_key(&reg.key) {
                // Already registered (e.g., by LoopStatePlugin or another source) — skip.
                continue;
            }
            registry.keys_by_name.insert(reg.key.clone(), reg.clone());
            registry.keys_by_type.insert(reg.type_id, reg.clone());
        }
        Ok(())
    }

    pub fn uninstall_plugin<P>(&self) -> Result<(), StateError>
    where
        P: Plugin,
    {
        let plugin_type_id = TypeId::of::<P>();
        let registrations =
            {
                let registry = self.registry.lock();
                let installed = registry.plugins.get(&plugin_type_id).ok_or(
                    StateError::PluginNotInstalled {
                        type_name: std::any::type_name::<P>(),
                    },
                )?;
                installed
                    .owned_key_type_ids
                    .iter()
                    .filter_map(|type_id| registry.keys_by_type.get(type_id).cloned())
                    .collect::<Vec<_>>()
            };

        let mut patch = MutationBatch::new().with_base_revision(self.revision());
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
                let mut registry = self.registry.lock();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
    use crate::state::StateKey;
    use std::sync::atomic::AtomicU64;

    struct TestCounter;

    impl StateKey for TestCounter {
        const KEY: &'static str = "test.store_counter";
        type Value = i64;
        type Update = i64;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value += update;
        }
    }

    struct TestStorePlugin;

    impl Plugin for TestStorePlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "test-store-plugin",
            }
        }

        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_key::<TestCounter>(crate::state::StateKeyOptions::default())
        }
    }

    #[test]
    fn store_new_starts_at_revision_zero() {
        let store = StateStore::new();
        assert_eq!(store.revision(), 0);
    }

    #[test]
    fn store_commit_increments_revision() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(1);
        let rev = store.commit(batch).unwrap();
        assert_eq!(rev, 1);

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(2);
        let rev = store.commit(batch).unwrap();
        assert_eq!(rev, 2);
    }

    #[test]
    fn store_empty_commit_returns_current_revision() {
        let store = StateStore::new();
        let batch = store.begin_mutation();
        let rev = store.commit(batch).unwrap();
        assert_eq!(rev, 0);
    }

    #[test]
    fn store_read_returns_none_before_write() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();
        let val = store.read::<TestCounter>();
        assert!(val.is_none());
    }

    #[test]
    fn store_read_after_write() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(42);
        store.commit(batch).unwrap();

        let val = store.read::<TestCounter>().unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn store_multiple_updates_accumulate() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(10);
        store.commit(batch).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(20);
        store.commit(batch).unwrap();

        let val = store.read::<TestCounter>().unwrap();
        assert_eq!(val, 30);
    }

    #[test]
    fn store_snapshot_is_independent_copy() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(10);
        store.commit(batch).unwrap();

        let snap = store.snapshot();
        assert_eq!(snap.revision, 1);

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(20);
        store.commit(batch).unwrap();

        assert_eq!(snap.revision, 1);
        assert_eq!(store.revision(), 2);
    }

    #[test]
    fn store_clone_shares_state() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(100);
        store.commit(batch).unwrap();

        let store2 = store.clone();
        assert_eq!(store2.read::<TestCounter>().unwrap(), 100);
        assert_eq!(store2.revision(), 1);

        let mut batch = store2.begin_mutation();
        batch.update::<TestCounter>(50);
        store2.commit(batch).unwrap();
        assert_eq!(store.read::<TestCounter>().unwrap(), 150);
    }

    #[test]
    fn store_install_plugin_duplicate_rejected() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();
        let err = store.install_plugin(TestStorePlugin);
        assert!(err.is_err());
    }

    #[test]
    fn store_commit_hook_fires() {
        use std::sync::atomic::Ordering;

        struct TestHook {
            revision: Arc<AtomicU64>,
        }

        impl CommitHook for TestHook {
            fn on_commit(&self, event: &CommitEvent) {
                self.revision.store(event.new_revision, Ordering::SeqCst);
            }
        }

        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let rev = Arc::new(AtomicU64::new(0));
        store.add_hook(TestHook {
            revision: rev.clone(),
        });

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(1);
        store.commit(batch).unwrap();

        assert_eq!(rev.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn store_base_revision_conflict() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(1);
        store.commit(batch).unwrap();

        let mut batch = MutationBatch::new().with_base_revision(0);
        batch.update::<TestCounter>(2);
        let err = store.commit(batch);
        assert!(err.is_err());
    }

    #[test]
    fn store_uninstall_plugin() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();
        store.uninstall_plugin::<TestStorePlugin>().unwrap();
        let err = store.uninstall_plugin::<TestStorePlugin>();
        assert!(err.is_err());
    }

    #[test]
    fn store_multiple_updates_in_single_batch() {
        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(10);
        batch.update::<TestCounter>(20);
        batch.update::<TestCounter>(30);
        store.commit(batch).unwrap();

        let val = store.read::<TestCounter>().unwrap();
        assert_eq!(val, 60);
        assert_eq!(store.revision(), 1);
    }

    #[test]
    fn store_commit_event_has_correct_metadata() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MetadataHook {
            op_count: Arc<AtomicUsize>,
            prev_rev: Arc<AtomicU64>,
        }

        impl CommitHook for MetadataHook {
            fn on_commit(&self, event: &CommitEvent) {
                self.op_count.store(event.op_count, Ordering::SeqCst);
                self.prev_rev
                    .store(event.previous_revision, Ordering::SeqCst);
            }
        }

        let store = StateStore::new();
        store.install_plugin(TestStorePlugin).unwrap();

        let op_count = Arc::new(AtomicUsize::new(0));
        let prev_rev = Arc::new(AtomicU64::new(999));
        store.add_hook(MetadataHook {
            op_count: op_count.clone(),
            prev_rev: prev_rev.clone(),
        });

        let mut batch = store.begin_mutation();
        batch.update::<TestCounter>(1);
        batch.update::<TestCounter>(2);
        store.commit(batch).unwrap();

        assert_eq!(op_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(prev_rev.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
