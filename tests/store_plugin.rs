#![allow(missing_docs)]

use awaken::*;

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CoreState {
    status: String,
    jobs_finished: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum CoreAction {
    SetStatus(String),
    AddJobs(usize),
}

struct CoreChannel;

impl StateKey for CoreChannel {
    const KEY: &'static str = "app.core";
    type Value = CoreState;
    type Update = CoreAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            CoreAction::SetStatus(status) => value.status = status,
            CoreAction::AddJobs(count) => value.jobs_finished += count,
        }
    }
}

struct Messages;

impl StateKey for Messages {
    const KEY: &'static str = "chat.messages";
    type Value = Vec<String>;
    type Update = String;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.push(update);
    }
}

struct TokenUsage;

impl StateKey for TokenUsage {
    const KEY: &'static str = "chat.token_usage";
    type Value = u64;
    type Update = u64;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

struct Summary;

impl StateKey for Summary {
    const KEY: &'static str = "chat.summary";
    type Value = Option<String>;
    type Update = String;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = Some(update);
    }
}

struct SharedCounter;

impl StateKey for SharedCounter {
    const KEY: &'static str = "shared.counter";
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

struct EphemeralCounter;

impl StateKey for EphemeralCounter {
    const KEY: &'static str = "ephemeral.counter";
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

struct RetainedSummary;

impl StateKey for RetainedSummary {
    const KEY: &'static str = "retained.summary";
    type Value = Option<String>;
    type Update = String;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = Some(update);
    }
}

struct ChatPlugin;

impl Plugin for ChatPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "chat-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<CoreChannel>(StateKeyOptions::default())?;
        registrar.register_key::<Messages>(StateKeyOptions::default())?;
        registrar.register_key::<TokenUsage>(StateKeyOptions::default())?;
        registrar.register_key::<Summary>(StateKeyOptions::default())?;
        Ok(())
    }
}

struct SharedPlugin;

impl Plugin for SharedPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "shared-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<SharedCounter>(StateKeyOptions::default())?;
        Ok(())
    }
}

struct EphemeralPlugin;

impl Plugin for EphemeralPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "ephemeral-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<EphemeralCounter>(StateKeyOptions {
            persistent: false,
            retain_on_uninstall: false,
            ..Default::default()
        })?;
        Ok(())
    }
}

struct RetainedPlugin;

impl Plugin for RetainedPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "retained-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<RetainedSummary>(StateKeyOptions {
            persistent: true,
            retain_on_uninstall: true,
            ..Default::default()
        })?;
        Ok(())
    }
}

struct DuplicateKeyPlugin;

impl Plugin for DuplicateKeyPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "duplicate-key-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<Messages>(StateKeyOptions::default())?;
        registrar.register_key::<Messages>(StateKeyOptions::default())?;
        Ok(())
    }
}

struct CountingHook(Arc<AtomicUsize>);

impl CommitHook for CountingHook {
    fn on_commit(&self, _event: &CommitEvent) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn plugin_lifecycle_and_seed_state_work() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();

    // After install, keys are registered but no seed data (on_install removed)
    let snapshot = store.snapshot();
    assert!(snapshot.get::<CoreChannel>().is_none());
    assert!(snapshot.get::<Messages>().is_none());

    // Manually seed state
    let mut patch = MutationBatch::new();
    patch.update::<CoreChannel>(CoreAction::SetStatus("ready".into()));
    patch.update::<Messages>("system: plugin installed".into());
    store.commit(patch).unwrap();

    let snapshot = store.snapshot();
    assert_eq!(snapshot.get::<CoreChannel>().unwrap().status, "ready");
    assert_eq!(snapshot.get::<Messages>().unwrap().len(), 1);

    store.uninstall_plugin::<ChatPlugin>().unwrap();
    let snapshot = store.snapshot();
    assert!(snapshot.get::<Messages>().is_none());
    assert!(snapshot.get::<TokenUsage>().is_none());
    assert!(snapshot.get::<CoreChannel>().is_none());
}

#[test]
fn plugin_uninstall_commits_revision_and_triggers_hooks() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    store.add_hook(CountingHook(Arc::clone(&hits)));

    let before = store.revision();
    store.uninstall_plugin::<ChatPlugin>().unwrap();

    assert_eq!(store.revision(), before + 1);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_compute_serial_commit_works() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();

    let handles: Vec<_> = (0..8)
        .map(|index| {
            let store = store.clone();
            thread::spawn(move || {
                let snapshot = store.snapshot();
                let mut patch = MutationBatch::new().with_base_revision(snapshot.revision());
                patch.update::<CoreChannel>(CoreAction::AddJobs(1));
                patch.update::<Messages>(format!("worker-{index}"));
                patch.update::<TokenUsage>((index + 1) as u64);
                patch
            })
        })
        .collect();

    let mut committed = 0;
    for handle in handles {
        let patch = handle.join().unwrap();
        if store.commit(patch).is_ok() {
            committed += 1;
        }
    }

    let snapshot = store.snapshot();
    assert!(committed >= 1);
    assert!(snapshot.get::<CoreChannel>().unwrap().jobs_finished >= 1);
    assert!(snapshot.get::<TokenUsage>().copied().unwrap_or_default() >= 1);
}

#[test]
fn revision_conflict_is_detected() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();

    let snapshot = store.snapshot();

    let mut ok = MutationBatch::new().with_base_revision(snapshot.revision());
    ok.update::<TokenUsage>(1);
    store.commit(ok).unwrap();

    let mut stale = MutationBatch::new().with_base_revision(snapshot.revision());
    stale.update::<TokenUsage>(1);
    let err = store.commit(stale).unwrap_err();
    assert!(matches!(err, StateError::RevisionConflict { .. }));
}

#[test]
fn patch_commits_as_single_revision() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();
    let start = store.revision();

    let mut patch = MutationBatch::new().with_base_revision(start);
    patch.update::<Messages>("a".into());
    patch.update::<Messages>("b".into());
    patch.update::<TokenUsage>(3);
    let end = store.commit(patch).unwrap();

    assert_eq!(end, start + 1);
    let snapshot = store.snapshot();
    assert_eq!(snapshot.get::<Messages>().unwrap().len(), 2);
    assert_eq!(snapshot.get::<TokenUsage>().copied(), Some(3));
}

#[test]
fn hooks_are_called() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    store.add_hook(CountingHook(Arc::clone(&hits)));

    let mut patch = MutationBatch::new();
    patch.update::<TokenUsage>(1);
    store.commit(patch).unwrap();

    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn empty_patch_commit_keeps_revision_and_skips_hooks() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    store.add_hook(CountingHook(Arc::clone(&hits)));

    let before = store.revision();
    let after = store.commit(MutationBatch::new()).unwrap();

    assert_eq!(before, after);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn persistence_roundtrip_works() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();
    store.install_plugin(SharedPlugin).unwrap();

    let mut patch = MutationBatch::new();
    patch.update::<CoreChannel>(CoreAction::SetStatus("ready".into()));
    patch.update::<TokenUsage>(42);
    patch.update::<SharedCounter>(2);
    store.commit(patch).unwrap();

    let persisted = store.export_persisted().unwrap();
    let restored = StateStore::new();
    restored.install_plugin(ChatPlugin).unwrap();
    restored.install_plugin(SharedPlugin).unwrap();
    restored
        .restore_persisted(persisted, UnknownKeyPolicy::Error)
        .unwrap();

    let snapshot = restored.snapshot();
    assert_eq!(snapshot.get::<TokenUsage>().copied(), Some(42));
    assert_eq!(snapshot.get::<SharedCounter>().copied(), Some(2));
    assert_eq!(snapshot.get::<CoreChannel>().unwrap().status, "ready");
}

#[test]
fn unregistered_slot_is_rejected() {
    let store = StateStore::new();
    let mut patch = MutationBatch::new();
    patch.update::<TokenUsage>(1);
    let err = store.commit(patch).unwrap_err();
    assert!(matches!(err, StateError::UnknownKey { .. }));
}

#[test]
fn duplicate_plugin_install_is_rejected() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();

    let err = store.install_plugin(ChatPlugin).unwrap_err();
    assert!(matches!(err, StateError::PluginAlreadyInstalled { .. }));
}

#[test]
fn uninstalling_unknown_plugin_is_rejected() {
    let store = StateStore::new();

    let err = store.uninstall_plugin::<ChatPlugin>().unwrap_err();
    assert!(matches!(err, StateError::PluginNotInstalled { .. }));
}

#[test]
fn duplicate_slot_registration_within_plugin_is_rejected() {
    let store = StateStore::new();
    let err = store.install_plugin(DuplicateKeyPlugin).unwrap_err();
    assert!(matches!(err, StateError::KeyAlreadyRegistered { .. }));
}

#[test]
fn retained_slots_survive_plugin_uninstall() {
    let store = StateStore::new();
    store.install_plugin(RetainedPlugin).unwrap();

    // Manually seed (on_install removed)
    let mut patch = MutationBatch::new();
    patch.update::<RetainedSummary>("seed".into());
    store.commit(patch).unwrap();

    assert_eq!(
        store.read::<RetainedSummary>(),
        Some(Some("seed".to_string()))
    );

    store.uninstall_plugin::<RetainedPlugin>().unwrap();

    assert_eq!(
        store.read::<RetainedSummary>(),
        Some(Some("seed".to_string()))
    );
}

#[test]
fn restore_persisted_can_skip_unknown_slots() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();

    let persisted = PersistedState {
        revision: 7,
        extensions: std::collections::HashMap::from([
            ("chat.token_usage".to_string(), serde_json::json!(99_u64)),
            (
                "missing.slot".to_string(),
                serde_json::json!({"ignored": true}),
            ),
        ]),
    };

    store
        .restore_persisted(persisted, UnknownKeyPolicy::Skip)
        .unwrap();

    assert_eq!(store.revision(), 7);
    assert_eq!(store.read::<TokenUsage>(), Some(99));
    assert!(store.read::<Messages>().is_none());
}

#[test]
fn export_persisted_skips_non_persistent_slots() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();
    store.install_plugin(EphemeralPlugin).unwrap();

    let mut patch = MutationBatch::new();
    patch.update::<TokenUsage>(5);
    patch.update::<EphemeralCounter>(7);
    store.commit(patch).unwrap();

    let persisted = store.export_persisted().unwrap();

    assert!(persisted.extensions.contains_key("chat.token_usage"));
    assert!(!persisted.extensions.contains_key("ephemeral.counter"));
}

#[test]
fn restore_persisted_reports_decode_errors() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();

    let persisted = PersistedState {
        revision: 1,
        extensions: std::collections::HashMap::from([(
            "chat.token_usage".to_string(),
            serde_json::json!("bad"),
        )]),
    };

    let err = store
        .restore_persisted(persisted, UnknownKeyPolicy::Error)
        .unwrap_err();
    assert!(matches!(err, StateError::KeyDecode { .. }));
}

// ---------------------------------------------------------------------------
// Session memory: KeyScope tests
// ---------------------------------------------------------------------------

/// A thread-scoped counter that persists across runs.
struct SessionCounter;

impl StateKey for SessionCounter {
    const KEY: &'static str = "test.session_counter";
    const SCOPE: KeyScope = KeyScope::Thread;

    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

/// A run-scoped scratch value cleared at each run start.
struct RunScratch;

impl StateKey for RunScratch {
    const KEY: &'static str = "test.run_scratch";
    // SCOPE defaults to KeyScope::Run

    type Value = String;
    type Update = String;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

struct SessionMemoryPlugin;

impl Plugin for SessionMemoryPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "session_memory_test",
        }
    }

    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<SessionCounter>(StateKeyOptions {
            persistent: true,
            retain_on_uninstall: false,
            scope: KeyScope::Thread,
        })?;
        r.register_key::<RunScratch>(StateKeyOptions::default())?;
        Ok(())
    }
}

#[test]
fn thread_scoped_state_survives_run_boundary() {
    // --- Run 1: write a thread-scoped value and checkpoint ---
    let store1 = StateStore::new();
    store1.install_plugin(SessionMemoryPlugin).unwrap();

    let mut batch = store1.begin_mutation();
    batch.update::<SessionCounter>(42);
    store1.commit(batch).unwrap();

    assert_eq!(store1.read::<SessionCounter>(), Some(42));

    // Export state (simulates checkpoint at end of run 1)
    let persisted = store1.export_persisted().unwrap();

    // --- Run 2: new store, restore only thread-scoped keys ---
    let store2 = StateStore::new();
    store2.install_plugin(SessionMemoryPlugin).unwrap();

    store2
        .restore_thread_scoped(persisted, UnknownKeyPolicy::Skip)
        .unwrap();

    // Thread-scoped value survives
    assert_eq!(store2.read::<SessionCounter>(), Some(42));

    // Can continue accumulating
    let mut batch = store2.begin_mutation();
    batch.update::<SessionCounter>(8);
    store2.commit(batch).unwrap();
    assert_eq!(store2.read::<SessionCounter>(), Some(50));
}

#[test]
fn run_scoped_state_cleared_at_run_start() {
    // --- Run 1: write both run-scoped and thread-scoped values ---
    let store1 = StateStore::new();
    store1.install_plugin(SessionMemoryPlugin).unwrap();

    let mut batch = store1.begin_mutation();
    batch.update::<SessionCounter>(10);
    batch.update::<RunScratch>("hello".to_string());
    store1.commit(batch).unwrap();

    assert_eq!(store1.read::<SessionCounter>(), Some(10));
    assert_eq!(store1.read::<RunScratch>(), Some("hello".to_string()));

    // Export state (simulates checkpoint at end of run 1)
    let persisted = store1.export_persisted().unwrap();

    // --- Run 2: new store, restore only thread-scoped keys ---
    let store2 = StateStore::new();
    store2.install_plugin(SessionMemoryPlugin).unwrap();

    store2
        .restore_thread_scoped(persisted, UnknownKeyPolicy::Skip)
        .unwrap();

    // Thread-scoped value survives
    assert_eq!(store2.read::<SessionCounter>(), Some(10));
    // Run-scoped value is NOT restored
    assert_eq!(store2.read::<RunScratch>(), None);
}

#[test]
fn clear_run_scoped_preserves_thread_keys() {
    let store = StateStore::new();
    store.install_plugin(SessionMemoryPlugin).unwrap();

    let mut batch = store.begin_mutation();
    batch.update::<SessionCounter>(7);
    batch.update::<RunScratch>("temp".to_string());
    store.commit(batch).unwrap();

    assert_eq!(store.read::<SessionCounter>(), Some(7));
    assert_eq!(store.read::<RunScratch>(), Some("temp".to_string()));

    // Simulate run boundary: clear run-scoped keys only
    store.clear_run_scoped();

    // Thread-scoped key preserved
    assert_eq!(store.read::<SessionCounter>(), Some(7));
    // Run-scoped key cleared
    assert_eq!(store.read::<RunScratch>(), None);
}
