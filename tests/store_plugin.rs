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

impl StateSlot for CoreChannel {
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

impl StateSlot for Messages {
    const KEY: &'static str = "chat.messages";
    type Value = Vec<String>;
    type Update = String;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.push(update);
    }
}

struct TokenUsage;

impl StateSlot for TokenUsage {
    const KEY: &'static str = "chat.token_usage";
    type Value = u64;
    type Update = u64;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

struct Summary;

impl StateSlot for Summary {
    const KEY: &'static str = "chat.summary";
    type Value = Option<String>;
    type Update = String;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = Some(update);
    }
}

struct SharedCounter;

impl StateSlot for SharedCounter {
    const KEY: &'static str = "shared.counter";
    type Value = usize;
    type Update = usize;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

struct RetainedSummary;

impl StateSlot for RetainedSummary {
    const KEY: &'static str = "retained.summary";
    type Value = Option<String>;
    type Update = String;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = Some(update);
    }
}

struct ChatPlugin;

impl StatePlugin for ChatPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "chat-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_slot::<CoreChannel>(SlotOptions::default())?;
        registrar.register_slot::<Messages>(SlotOptions::default())?;
        registrar.register_slot::<TokenUsage>(SlotOptions::default())?;
        registrar.register_slot::<Summary>(SlotOptions::default())?;
        Ok(())
    }

    fn on_install(&self, patch: &mut MutationBatch) -> Result<(), StateError> {
        patch.update::<CoreChannel>(CoreAction::SetStatus("ready".into()));
        patch.update::<Messages>("system: plugin installed".into());
        Ok(())
    }

    fn on_uninstall(&self, patch: &mut MutationBatch) -> Result<(), StateError> {
        patch.update::<Messages>("system: uninstalling".into());
        Ok(())
    }
}

struct SharedPlugin;

impl StatePlugin for SharedPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "shared-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_slot::<SharedCounter>(SlotOptions::default())?;
        Ok(())
    }
}

struct RetainedPlugin;

impl StatePlugin for RetainedPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "retained-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_slot::<RetainedSummary>(SlotOptions {
            persistent: true,
            retain_on_uninstall: true,
        })?;
        Ok(())
    }

    fn on_install(&self, patch: &mut MutationBatch) -> Result<(), StateError> {
        patch.update::<RetainedSummary>("seed".into());
        Ok(())
    }
}

struct DuplicateSlotPlugin;

impl StatePlugin for DuplicateSlotPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "duplicate-slot-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_slot::<Messages>(SlotOptions::default())?;
        registrar.register_slot::<Messages>(SlotOptions::default())?;
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
    assert_eq!(snapshot.get::<Messages>().unwrap().len(), 3);
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
fn persistence_roundtrip_works() {
    let store = StateStore::new();
    store.install_plugin(ChatPlugin).unwrap();
    store.install_plugin(SharedPlugin).unwrap();

    let mut patch = MutationBatch::new();
    patch.update::<TokenUsage>(42);
    patch.update::<SharedCounter>(2);
    store.commit(patch).unwrap();

    let persisted = store.export_persisted().unwrap();
    let restored = StateStore::new();
    restored.install_plugin(ChatPlugin).unwrap();
    restored.install_plugin(SharedPlugin).unwrap();
    restored
        .restore_persisted(persisted, UnknownSlotPolicy::Error)
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
    assert!(matches!(err, StateError::UnknownSlot { .. }));
}

#[test]
fn duplicate_slot_registration_within_plugin_is_rejected() {
    let store = StateStore::new();
    let err = store.install_plugin(DuplicateSlotPlugin).unwrap_err();
    assert!(matches!(err, StateError::SlotAlreadyRegistered { .. }));
}

#[test]
fn retained_slots_survive_plugin_uninstall() {
    let store = StateStore::new();
    store.install_plugin(RetainedPlugin).unwrap();

    assert_eq!(
        store.read_slot::<RetainedSummary>(),
        Some(Some("seed".to_string()))
    );

    store.uninstall_plugin::<RetainedPlugin>().unwrap();

    assert_eq!(
        store.read_slot::<RetainedSummary>(),
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
        .restore_persisted(persisted, UnknownSlotPolicy::Skip)
        .unwrap();

    assert_eq!(store.revision(), 7);
    assert_eq!(store.read_slot::<TokenUsage>(), Some(99));
    assert!(store.read_slot::<Messages>().is_none());
}
