#![allow(missing_docs)]
//! Comprehensive tests for the state/action/effect/reduce model.
//!
//! Covers: StateMap edge cases, MutationBatch composition, StateCommand semantics,
//! StateStore commit ordering, plugin lifecycle edge cases, persistence roundtrips,
//! phase execution boundaries, effect dispatch, and cross-plugin interaction.

use async_trait::async_trait;
use awaken::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ===========================================================================
// Test keys
// ===========================================================================

/// Exclusive counter (default merge strategy).
struct Counter;
impl StateKey for Counter {
    const KEY: &'static str = "test.counter";
    type Value = i64;
    type Update = i64;
    fn apply(value: &mut i64, update: i64) {
        *value += update;
    }
}

/// Commutative counter — parallel updates can merge safely.
struct SharedCounter;
impl StateKey for SharedCounter {
    const KEY: &'static str = "test.shared_counter";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = i64;
    type Update = i64;
    fn apply(value: &mut i64, update: i64) {
        *value += update;
    }
}

struct Label;
impl StateKey for Label {
    const KEY: &'static str = "test.label";
    type Value = String;
    type Update = String;
    fn apply(value: &mut String, update: String) {
        *value = update;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct EventLog {
    entries: Vec<String>,
}

struct Events;
impl StateKey for Events {
    const KEY: &'static str = "test.events";
    type Value = EventLog;
    type Update = String;
    fn apply(value: &mut EventLog, update: String) {
        value.entries.push(update);
    }
}

/// Key with replacement semantics (like config).
struct Mode;
impl StateKey for Mode {
    const KEY: &'static str = "test.mode";
    type Value = Option<String>;
    type Update = Option<String>;
    fn apply(value: &mut Option<String>, update: Option<String>) {
        *value = update;
    }
}

// ===========================================================================
// Test plugins
// ===========================================================================

struct CounterPlugin;
impl Plugin for CounterPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "counter-plugin",
        }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<Counter>(StateKeyOptions::default())?;
        Ok(())
    }
}

struct LabelPlugin;
impl Plugin for LabelPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "label-plugin",
        }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<Label>(StateKeyOptions::default())?;
        Ok(())
    }
}

struct EventsPlugin;
impl Plugin for EventsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "events-plugin",
        }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<Events>(StateKeyOptions::default())?;
        Ok(())
    }
}

struct MultiKeyPlugin;
impl Plugin for MultiKeyPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "multi-key-plugin",
        }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<Counter>(StateKeyOptions::default())?;
        r.register_key::<Label>(StateKeyOptions::default())?;
        r.register_key::<Events>(StateKeyOptions::default())?;
        r.register_key::<Mode>(StateKeyOptions::default())?;
        Ok(())
    }
}

// ===========================================================================
// 1. StateKey::apply — reducer semantics
// ===========================================================================

#[test]
fn apply_additive_accumulates() {
    let mut val: i64 = 0;
    Counter::apply(&mut val, 3);
    Counter::apply(&mut val, -1);
    Counter::apply(&mut val, 5);
    assert_eq!(val, 7);
}

#[test]
fn apply_replacement_overwrites() {
    let mut val = Some("old".into());
    Mode::apply(&mut val, Some("new".into()));
    assert_eq!(val.as_deref(), Some("new"));
    Mode::apply(&mut val, None);
    assert!(val.is_none());
}

#[test]
fn apply_append_preserves_order() {
    let mut log = EventLog::default();
    Events::apply(&mut log, "first".into());
    Events::apply(&mut log, "second".into());
    Events::apply(&mut log, "third".into());
    assert_eq!(log.entries, vec!["first", "second", "third"]);
}

#[test]
fn apply_with_zero_update_is_noop_for_counter() {
    let mut val: i64 = 42;
    Counter::apply(&mut val, 0);
    assert_eq!(val, 42);
}

#[test]
fn apply_with_empty_string_replaces_label() {
    let mut val = "hello".to_string();
    Label::apply(&mut val, String::new());
    assert_eq!(val, "");
}

// ===========================================================================
// 2. StateMap — heterogeneous typed map
// ===========================================================================

#[test]
fn state_map_independent_keys_do_not_interfere() {
    let store = StateStore::new();
    store.install_plugin(MultiKeyPlugin).unwrap();

    let mut patch = MutationBatch::new();
    patch.update::<Counter>(10);
    patch.update::<Label>("hello".into());
    store.commit(patch).unwrap();

    assert_eq!(store.read::<Counter>(), Some(10));
    assert_eq!(store.read::<Label>().as_deref(), Some("hello"));
    // Events and Mode are default (not written)
    assert_eq!(store.read::<Events>(), None);
    assert_eq!(store.read::<Mode>(), None);
}

#[test]
fn state_map_multiple_updates_same_key_in_batch() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let mut patch = MutationBatch::new();
    patch.update::<Counter>(1);
    patch.update::<Counter>(2);
    patch.update::<Counter>(3);
    store.commit(patch).unwrap();

    // All three apply in order: 0 + 1 + 2 + 3 = 6
    assert_eq!(store.read::<Counter>(), Some(6));
}

#[test]
fn state_map_negative_counter_goes_below_zero() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let mut patch = MutationBatch::new();
    patch.update::<Counter>(-5);
    store.commit(patch).unwrap();

    assert_eq!(store.read::<Counter>(), Some(-5));
}

// ===========================================================================
// 3. MutationBatch — composition and extend
// ===========================================================================

#[test]
fn mutation_batch_extend_both_none_base_revision() {
    let mut left = MutationBatch::new();
    left.update::<Counter>(1);
    let mut right = MutationBatch::new();
    right.update::<Counter>(2);

    left.extend(right).unwrap();
    assert_eq!(left.base_revision(), None);
    assert!(!left.is_empty());
}

#[test]
fn mutation_batch_extend_left_some_right_none() {
    let mut left = MutationBatch::new().with_base_revision(5);
    let right = MutationBatch::new();

    left.extend(right).unwrap();
    assert_eq!(left.base_revision(), Some(5));
}

#[test]
fn mutation_batch_extend_left_none_right_some() {
    let mut left = MutationBatch::new();
    let right = MutationBatch::new().with_base_revision(7);

    left.extend(right).unwrap();
    assert_eq!(left.base_revision(), Some(7));
}

#[test]
fn mutation_batch_extend_empty_into_non_empty() {
    let store = StateStore::new();
    store.install_plugin(MultiKeyPlugin).unwrap();

    let mut batch = MutationBatch::new();
    batch.update::<Counter>(10);
    batch.extend(MutationBatch::new()).unwrap();
    store.commit(batch).unwrap();

    assert_eq!(store.read::<Counter>(), Some(10));
}

#[test]
fn mutation_batch_extend_preserves_op_order() {
    let store = StateStore::new();
    store.install_plugin(EventsPlugin).unwrap();

    let mut left = MutationBatch::new();
    left.update::<Events>("from-left".into());
    let mut right = MutationBatch::new();
    right.update::<Events>("from-right".into());

    left.extend(right).unwrap();
    store.commit(left).unwrap();

    let log = store.read::<Events>().unwrap();
    assert_eq!(log.entries, vec!["from-left", "from-right"]);
}

#[test]
fn mutation_batch_triple_extend_chain() {
    let store = StateStore::new();
    store.install_plugin(EventsPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Events>("a".into());
    let mut b = MutationBatch::new();
    b.update::<Events>("b".into());
    let mut c = MutationBatch::new();
    c.update::<Events>("c".into());

    a.extend(b).unwrap();
    a.extend(c).unwrap();
    store.commit(a).unwrap();

    let log = store.read::<Events>().unwrap();
    assert_eq!(log.entries, vec!["a", "b", "c"]);
}

// ===========================================================================
// 4. StateCommand — composition edge cases
// ===========================================================================

#[test]
fn state_command_extend_empty_commands() {
    let left = StateCommand::new();
    let mut combined = StateCommand::new();
    combined.extend(left).unwrap();
    assert!(combined.is_empty());
}

#[test]
fn state_command_extend_mismatched_revisions_fails() {
    let left = StateCommand::new().with_base_revision(1);
    let right = StateCommand::new().with_base_revision(2);
    let mut combined = left;
    let err = combined.extend(right).unwrap_err();
    assert!(matches!(
        err,
        StateError::MutationBaseRevisionMismatch { left: 1, right: 2 }
    ));
}

#[test]
fn state_command_multiple_effects_accumulate() {
    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::Suspend { reason: "a".into() })
        .unwrap();
    cmd.effect(RuntimeEffect::Suspend { reason: "b".into() })
        .unwrap();
    assert!(!cmd.is_empty());
}

// ===========================================================================
// 5. StateStore — commit semantics
// ===========================================================================

#[test]
fn commit_increments_revision_by_one() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let r0 = store.revision();
    let mut p1 = MutationBatch::new();
    p1.update::<Counter>(1);
    let r1 = store.commit(p1).unwrap();
    assert_eq!(r1, r0 + 1);

    let mut p2 = MutationBatch::new();
    p2.update::<Counter>(1);
    let r2 = store.commit(p2).unwrap();
    assert_eq!(r2, r1 + 1);
}

#[test]
fn commit_without_base_revision_always_succeeds() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    // Advance revision several times
    for _ in 0..5 {
        let mut p = MutationBatch::new();
        p.update::<Counter>(1);
        store.commit(p).unwrap();
    }

    // Commit without base_revision doesn't check revision
    let mut p = MutationBatch::new();
    p.update::<Counter>(100);
    store.commit(p).unwrap();

    assert_eq!(store.read::<Counter>(), Some(105));
}

#[test]
fn commit_rejects_stale_base_revision() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let stale_rev = store.revision();

    let mut p = MutationBatch::new();
    p.update::<Counter>(1);
    store.commit(p).unwrap();

    let mut stale = MutationBatch::new().with_base_revision(stale_rev);
    stale.update::<Counter>(1);
    let err = store.commit(stale).unwrap_err();
    assert!(matches!(err, StateError::RevisionConflict { .. }));
}

#[test]
fn commit_to_unregistered_key_fails() {
    let store = StateStore::new();
    // No plugin installed — Counter not registered
    let mut p = MutationBatch::new();
    p.update::<Counter>(1);
    let err = store.commit(p).unwrap_err();
    assert!(matches!(err, StateError::UnknownKey { .. }));
}

#[test]
fn commit_hooks_fire_in_registration_order() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let order = Arc::new(Mutex::new(Vec::<&str>::new()));

    struct OrderHook {
        label: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
    }
    impl CommitHook for OrderHook {
        fn on_commit(&self, _event: &CommitEvent) {
            self.order.lock().unwrap().push(self.label);
        }
    }

    store.add_hook(OrderHook {
        label: "first",
        order: Arc::clone(&order),
    });
    store.add_hook(OrderHook {
        label: "second",
        order: Arc::clone(&order),
    });

    let mut p = MutationBatch::new();
    p.update::<Counter>(1);
    store.commit(p).unwrap();

    assert_eq!(*order.lock().unwrap(), vec!["first", "second"]);
}

#[test]
fn commit_hooks_see_post_commit_snapshot() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let seen = Arc::new(Mutex::new(None::<i64>));
    struct SnapshotHook(Arc<Mutex<Option<i64>>>);
    impl CommitHook for SnapshotHook {
        fn on_commit(&self, event: &CommitEvent) {
            *self.0.lock().unwrap() = event.snapshot.get::<Counter>().copied();
        }
    }
    store.add_hook(SnapshotHook(Arc::clone(&seen)));

    let mut p = MutationBatch::new();
    p.update::<Counter>(42);
    store.commit(p).unwrap();

    assert_eq!(*seen.lock().unwrap(), Some(42));
}

#[test]
fn snapshot_is_isolated_from_future_commits() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let mut p = MutationBatch::new();
    p.update::<Counter>(10);
    store.commit(p).unwrap();

    let snap = store.snapshot();
    assert_eq!(snap.get::<Counter>().copied(), Some(10));

    // Future commit doesn't affect old snapshot
    let mut p2 = MutationBatch::new();
    p2.update::<Counter>(90);
    store.commit(p2).unwrap();

    assert_eq!(snap.get::<Counter>().copied(), Some(10));
    assert_eq!(store.read::<Counter>(), Some(100));
}

#[test]
fn concurrent_non_conflicting_commits_both_succeed() {
    let store = StateStore::new();
    store.install_plugin(MultiKeyPlugin).unwrap();

    // Both patches have no base_revision, so no conflict
    let mut p1 = MutationBatch::new();
    p1.update::<Counter>(10);
    store.commit(p1).unwrap();

    let mut p2 = MutationBatch::new();
    p2.update::<Label>("hello".into());
    store.commit(p2).unwrap();

    assert_eq!(store.read::<Counter>(), Some(10));
    assert_eq!(store.read::<Label>().as_deref(), Some("hello"));
}

// ===========================================================================
// 6. Plugin lifecycle — edge cases
// ===========================================================================

#[test]
fn plugin_reinstall_after_uninstall_gets_clean_state() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let mut p = MutationBatch::new();
    p.update::<Counter>(42);
    store.commit(p).unwrap();
    assert_eq!(store.read::<Counter>(), Some(42));

    store.uninstall_plugin::<CounterPlugin>().unwrap();
    assert!(store.read::<Counter>().is_none());

    store.install_plugin(CounterPlugin).unwrap();
    // Fresh install — counter is back to default (0), not 42
    assert_eq!(store.read::<Counter>(), None);
}

#[test]
fn plugin_on_install_state_visible_immediately() {
    struct SeedPlugin;
    impl Plugin for SeedPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "seed" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions::default())
        }
        fn on_install(&self, patch: &mut MutationBatch) -> Result<(), StateError> {
            patch.update::<Counter>(99);
            Ok(())
        }
    }

    let store = StateStore::new();
    store.install_plugin(SeedPlugin).unwrap();
    assert_eq!(store.read::<Counter>(), Some(99));
}

#[test]
fn plugin_on_install_failure_propagates_error() {
    struct FailingInstallPlugin;
    impl Plugin for FailingInstallPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "failing-install",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions::default())
        }
        fn on_install(&self, _patch: &mut MutationBatch) -> Result<(), StateError> {
            Err(StateError::PluginNotInstalled {
                type_name: "synthetic",
            })
        }
    }

    let store = StateStore::new();
    let err = store.install_plugin(FailingInstallPlugin).unwrap_err();
    assert!(matches!(err, StateError::PluginNotInstalled { .. }));

    // No state was committed — counter is None
    assert!(store.read::<Counter>().is_none());
}

#[test]
fn plugin_register_failure_prevents_installation() {
    struct BadRegisterPlugin;
    impl Plugin for BadRegisterPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "bad-register",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions::default())?;
            // Duplicate registration within same plugin
            r.register_key::<Counter>(StateKeyOptions::default())
        }
    }

    let store = StateStore::new();
    let err = store.install_plugin(BadRegisterPlugin).unwrap_err();
    assert!(matches!(err, StateError::KeyAlreadyRegistered { .. }));
}

#[test]
fn two_plugins_share_no_state() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();
    store.install_plugin(LabelPlugin).unwrap();

    let mut p = MutationBatch::new();
    p.update::<Counter>(5);
    store.commit(p).unwrap();

    assert_eq!(store.read::<Counter>(), Some(5));
    assert_eq!(store.read::<Label>(), None);

    store.uninstall_plugin::<CounterPlugin>().unwrap();
    // Label plugin and its state should be unaffected
    let mut p = MutationBatch::new();
    p.update::<Label>("still here".into());
    store.commit(p).unwrap();
    assert_eq!(store.read::<Label>().as_deref(), Some("still here"));
}

#[test]
fn plugin_with_retained_key_survives_uninstall() {
    struct RetainedPlugin;
    impl Plugin for RetainedPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "retained" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions {
                persistent: true,
                retain_on_uninstall: true,
            })
        }
        fn on_install(&self, patch: &mut MutationBatch) -> Result<(), StateError> {
            patch.update::<Counter>(100);
            Ok(())
        }
    }

    let store = StateStore::new();
    store.install_plugin(RetainedPlugin).unwrap();
    assert_eq!(store.read::<Counter>(), Some(100));

    store.uninstall_plugin::<RetainedPlugin>().unwrap();
    // State retained
    assert_eq!(store.read::<Counter>(), Some(100));
}

// ===========================================================================
// 7. Persistence — edge cases
// ===========================================================================

#[test]
fn persistence_roundtrip_preserves_multiple_keys() {
    let store = StateStore::new();
    store.install_plugin(MultiKeyPlugin).unwrap();

    let mut p = MutationBatch::new();
    p.update::<Counter>(42);
    p.update::<Label>("hello".into());
    p.update::<Events>("evt1".into());
    p.update::<Mode>(Some("debug".into()));
    store.commit(p).unwrap();

    let persisted = store.export_persisted().unwrap();

    let store2 = StateStore::new();
    store2.install_plugin(MultiKeyPlugin).unwrap();
    store2
        .restore_persisted(persisted, UnknownKeyPolicy::Error)
        .unwrap();

    assert_eq!(store2.read::<Counter>(), Some(42));
    assert_eq!(store2.read::<Label>().as_deref(), Some("hello"));
    assert_eq!(store2.read::<Events>().unwrap().entries, vec!["evt1"]);
    assert_eq!(store2.read::<Mode>().unwrap().as_deref(), Some("debug"));
}

#[test]
fn persistence_skip_policy_ignores_unknown_keys() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let persisted = PersistedState {
        revision: 5,
        extensions: std::collections::HashMap::from([
            ("test.counter".to_string(), serde_json::json!(10)),
            ("unknown.key".to_string(), serde_json::json!("ignored")),
        ]),
    };

    store
        .restore_persisted(persisted, UnknownKeyPolicy::Skip)
        .unwrap();

    assert_eq!(store.read::<Counter>(), Some(10));
    assert_eq!(store.revision(), 5);
}

#[test]
fn persistence_error_policy_rejects_unknown_keys() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let persisted = PersistedState {
        revision: 5,
        extensions: std::collections::HashMap::from([
            ("test.counter".to_string(), serde_json::json!(10)),
            ("unknown.key".to_string(), serde_json::json!("boom")),
        ]),
    };

    let err = store
        .restore_persisted(persisted, UnknownKeyPolicy::Error)
        .unwrap_err();
    assert!(matches!(err, StateError::UnknownKey { .. }));
}

#[test]
fn persistence_type_mismatch_returns_decode_error() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let persisted = PersistedState {
        revision: 1,
        extensions: std::collections::HashMap::from([(
            "test.counter".to_string(),
            serde_json::json!("not a number"),
        )]),
    };

    let err = store
        .restore_persisted(persisted, UnknownKeyPolicy::Error)
        .unwrap_err();
    assert!(matches!(err, StateError::KeyDecode { .. }));
}

#[test]
fn persistence_non_persistent_key_excluded_from_export() {
    struct EphemeralPlugin;
    impl Plugin for EphemeralPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "ephemeral" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions {
                persistent: false,
                retain_on_uninstall: false,
            })
        }
    }

    let store = StateStore::new();
    store.install_plugin(EphemeralPlugin).unwrap();

    let mut p = MutationBatch::new();
    p.update::<Counter>(42);
    store.commit(p).unwrap();

    let persisted = store.export_persisted().unwrap();
    assert!(!persisted.extensions.contains_key("test.counter"));
}

#[test]
fn persistence_empty_state_exports_empty_extensions() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let persisted = store.export_persisted().unwrap();
    // Counter exists but has no value written — not exported
    assert!(persisted.extensions.is_empty());
}

// ===========================================================================
// 8. Phase execution — edge cases
// ===========================================================================

struct TestAction;
impl ScheduledActionSpec for TestAction {
    const KEY: &'static str = "test.action";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = String;
}

struct CountingAction;
impl ScheduledActionSpec for CountingAction {
    const KEY: &'static str = "test.counting_action";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

#[tokio::test]
async fn phase_with_no_hooks_no_actions_reports_zero() {
    let app = AppRuntime::new().unwrap();
    let report = app.run_phase(Phase::RunStart).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 0);
    assert_eq!(report.skipped_scheduled_actions, 0);
    assert_eq!(report.failed_scheduled_actions, 0);
    assert_eq!(report.effect_report.attempted, 0);
}

#[tokio::test]
async fn phase_max_rounds_boundary_exact() {
    // Action handler that always spawns one more action of the same type
    struct RespawningHandler;
    #[async_trait]
    impl TypedScheduledActionHandler<CountingAction> for RespawningHandler {
        async fn handle_typed(
            &self,
            _ctx: &PhaseContext,
            _payload: (),
        ) -> Result<StateCommand, StateError> {
            let mut cmd = StateCommand::new();
            cmd.schedule_action::<CountingAction>(()).unwrap();
            Ok(cmd)
        }
    }

    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<CountingAction, _>(RespawningHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<CountingAction>(()).unwrap();
    app.submit_command(cmd).await.unwrap();

    // With limit 3, should process 3 actions then fail on round 4
    let err = app
        .phase_runtime()
        .run_phase_with_limit(Phase::BeforeInference, 3)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StateError::PhaseRunLoopExceeded { max_rounds: 3, .. }
    ));
}

#[tokio::test]
async fn phase_action_for_different_phase_is_skipped() {
    struct OtherPhaseAction;
    impl ScheduledActionSpec for OtherPhaseAction {
        const KEY: &'static str = "test.other_phase_action";
        const PHASE: Phase = Phase::AfterInference;
        type Payload = ();
    }
    struct OtherHandler;
    #[async_trait]
    impl TypedScheduledActionHandler<OtherPhaseAction> for OtherHandler {
        async fn handle_typed(
            &self,
            _ctx: &PhaseContext,
            _payload: (),
        ) -> Result<StateCommand, StateError> {
            Ok(StateCommand::new())
        }
    }

    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<OtherPhaseAction, _>(OtherHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<OtherPhaseAction>(()).unwrap();
    app.submit_command(cmd).await.unwrap();

    // Run BeforeInference — should skip the AfterInference action
    let report = app.run_phase(Phase::BeforeInference).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 0);
    assert_eq!(report.skipped_scheduled_actions, 1);

    // Run AfterInference — should process it
    let report = app.run_phase(Phase::AfterInference).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
}

#[tokio::test]
async fn phase_hook_state_mutation_visible_to_action_handler() {
    struct WriterHook;
    #[async_trait]
    impl PhaseHook for WriterHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            let mut cmd = StateCommand::new();
            cmd.update::<Counter>(100);
            Ok(cmd)
        }
    }

    struct ReaderHandler {
        seen: Arc<Mutex<Option<i64>>>,
    }
    #[async_trait]
    impl TypedScheduledActionHandler<TestAction> for ReaderHandler {
        async fn handle_typed(
            &self,
            ctx: &PhaseContext,
            _payload: String,
        ) -> Result<StateCommand, StateError> {
            *self.seen.lock().unwrap() = ctx.state::<Counter>().copied();
            Ok(StateCommand::new())
        }
    }

    struct WriterPlugin;
    impl Plugin for WriterPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "writer" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions::default())?;
            r.register_phase_hook("writer", Phase::BeforeInference, WriterHook)?;
            Ok(())
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let app = AppRuntime::new().unwrap();
    app.install_plugin(WriterPlugin).unwrap();
    app.phase_runtime()
        .register_scheduled_action::<TestAction, _>(ReaderHandler {
            seen: Arc::clone(&seen),
        })
        .unwrap();

    // Schedule action, then run phase — hook writes first, action reads
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<TestAction>("check".into()).unwrap();
    app.submit_command(cmd).await.unwrap();

    app.run_phase(Phase::BeforeInference).await.unwrap();

    // Action handler should have seen the counter value written by the hook
    assert_eq!(*seen.lock().unwrap(), Some(100));
}

// ===========================================================================
// 9. Effect dispatch — edge cases
// ===========================================================================

#[derive(Clone, Default)]
struct EffectRecorder(Arc<Mutex<Vec<RuntimeEffect>>>);

#[async_trait]
impl TypedEffectHandler<RuntimeEffect> for EffectRecorder {
    async fn handle_typed(
        &self,
        payload: RuntimeEffect,
        _snapshot: &Snapshot,
    ) -> Result<(), String> {
        self.0.lock().unwrap().push(payload);
        Ok(())
    }
}

#[tokio::test]
async fn effect_dispatch_preserves_order() {
    let recorder = EffectRecorder::default();
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(recorder.clone())
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::Suspend {
        reason: "first".into(),
    })
    .unwrap();
    cmd.effect(RuntimeEffect::Suspend {
        reason: "second".into(),
    })
    .unwrap();
    cmd.effect(RuntimeEffect::Block {
        reason: "third".into(),
    })
    .unwrap();
    app.submit_command(cmd).await.unwrap();

    let effects = recorder.0.lock().unwrap();
    assert_eq!(effects.len(), 3);
    assert!(matches!(&effects[0], RuntimeEffect::Suspend { reason } if reason == "first"));
    assert!(matches!(&effects[1], RuntimeEffect::Suspend { reason } if reason == "second"));
    assert!(matches!(&effects[2], RuntimeEffect::Block { reason } if reason == "third"));
}

#[tokio::test]
async fn effect_handler_failure_does_not_block_other_effects() {
    struct FailingHandler;
    #[async_trait]
    impl TypedEffectHandler<RuntimeEffect> for FailingHandler {
        async fn handle_typed(
            &self,
            _payload: RuntimeEffect,
            _snapshot: &Snapshot,
        ) -> Result<(), String> {
            Err("boom".into())
        }
    }

    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(FailingHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::Suspend { reason: "a".into() })
        .unwrap();
    cmd.effect(RuntimeEffect::Block { reason: "b".into() })
        .unwrap();
    let report = app.submit_command(cmd).await.unwrap();

    assert_eq!(report.effect_report.attempted, 2);
    assert_eq!(report.effect_report.failed, 2);
    assert_eq!(report.effect_report.dispatched, 0);
}

#[tokio::test]
async fn effect_with_no_handler_rejected_at_submit() {
    let app = AppRuntime::new().unwrap();
    // No handler registered for RuntimeEffect

    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::Suspend {
        reason: "test".into(),
    })
    .unwrap();
    let err = app.submit_command(cmd).await.unwrap_err();
    assert!(matches!(err, StateError::UnknownEffectHandler { .. }));
}

#[tokio::test]
async fn effect_handler_sees_post_commit_snapshot() {
    let seen = Arc::new(Mutex::new(None::<i64>));

    struct SnapshotReader(Arc<Mutex<Option<i64>>>);
    #[async_trait]
    impl TypedEffectHandler<RuntimeEffect> for SnapshotReader {
        async fn handle_typed(
            &self,
            _payload: RuntimeEffect,
            snapshot: &Snapshot,
        ) -> Result<(), String> {
            *self.0.lock().unwrap() = snapshot.get::<Counter>().copied();
            Ok(())
        }
    }

    let app = AppRuntime::new().unwrap();
    app.install_plugin(CounterPlugin).unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(SnapshotReader(Arc::clone(&seen)))
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.update::<Counter>(77);
    cmd.effect(RuntimeEffect::Suspend {
        reason: "check".into(),
    })
    .unwrap();
    app.submit_command(cmd).await.unwrap();

    // Effect handler should see the committed counter value
    assert_eq!(*seen.lock().unwrap(), Some(77));
}

// ===========================================================================
// 10. Cross-plugin interaction — phase hooks
// ===========================================================================

#[tokio::test]
async fn hooks_from_different_plugins_see_each_others_mutations() {
    struct PluginA;
    impl Plugin for PluginA {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "plugin-a" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions::default())?;
            struct Hook;
            #[async_trait]
            impl PhaseHook for Hook {
                async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    let mut cmd = StateCommand::new();
                    cmd.update::<Counter>(10);
                    Ok(cmd)
                }
            }
            r.register_phase_hook("plugin-a", Phase::BeforeInference, Hook)?;
            Ok(())
        }
    }

    struct PluginB {
        seen: Arc<Mutex<Option<i64>>>,
    }
    impl Plugin for PluginB {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "plugin-b" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            struct ReadHook(Arc<Mutex<Option<i64>>>);
            #[async_trait]
            impl PhaseHook for ReadHook {
                async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
                    *self.0.lock().unwrap() = ctx.state::<Counter>().copied();
                    Ok(StateCommand::new())
                }
            }
            r.register_phase_hook(
                "plugin-b",
                Phase::BeforeInference,
                ReadHook(self.seen.clone()),
            )?;
            Ok(())
        }
    }

    let seen = Arc::new(Mutex::new(None));
    let app = AppRuntime::new().unwrap();
    // Install A first (writes counter), then B (reads counter)
    app.install_plugin(PluginA).unwrap();
    app.install_plugin(PluginB {
        seen: Arc::clone(&seen),
    })
    .unwrap();

    app.run_phase(Phase::BeforeInference).await.unwrap();

    // PluginB's hook should see PluginA's write
    assert_eq!(*seen.lock().unwrap(), Some(10));
}

#[tokio::test]
async fn uninstalled_plugin_hooks_do_not_fire() {
    let count = Arc::new(AtomicUsize::new(0));
    struct CountHook(Arc<AtomicUsize>);
    #[async_trait]
    impl PhaseHook for CountHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(StateCommand::new())
        }
    }

    struct CountPlugin(Arc<AtomicUsize>);
    impl Plugin for CountPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "count" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_phase_hook("count", Phase::RunStart, CountHook(self.0.clone()))?;
            Ok(())
        }
    }

    let app = AppRuntime::new().unwrap();
    app.install_plugin(CountPlugin(Arc::clone(&count))).unwrap();

    app.run_phase(Phase::RunStart).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);

    app.uninstall_plugin::<CountPlugin>().unwrap();

    app.run_phase(Phase::RunStart).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1); // No additional fires
}

// ===========================================================================
// 11. Action handler chains — multi-round convergence
// ===========================================================================

#[tokio::test]
async fn action_handler_spawning_different_action_converges() {
    struct StepOneAction;
    impl ScheduledActionSpec for StepOneAction {
        const KEY: &'static str = "test.step_one";
        const PHASE: Phase = Phase::BeforeInference;
        type Payload = ();
    }
    struct StepTwoAction;
    impl ScheduledActionSpec for StepTwoAction {
        const KEY: &'static str = "test.step_two";
        const PHASE: Phase = Phase::BeforeInference;
        type Payload = ();
    }

    struct StepOneHandler;
    #[async_trait]
    impl TypedScheduledActionHandler<StepOneAction> for StepOneHandler {
        async fn handle_typed(
            &self,
            _ctx: &PhaseContext,
            _payload: (),
        ) -> Result<StateCommand, StateError> {
            let mut cmd = StateCommand::new();
            cmd.schedule_action::<StepTwoAction>(()).unwrap();
            Ok(cmd)
        }
    }
    struct StepTwoHandler;
    #[async_trait]
    impl TypedScheduledActionHandler<StepTwoAction> for StepTwoHandler {
        async fn handle_typed(
            &self,
            _ctx: &PhaseContext,
            _payload: (),
        ) -> Result<StateCommand, StateError> {
            // Terminal — does not spawn further actions
            Ok(StateCommand::new())
        }
    }

    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<StepOneAction, _>(StepOneHandler)
        .unwrap();
    app.phase_runtime()
        .register_scheduled_action::<StepTwoAction, _>(StepTwoHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<StepOneAction>(()).unwrap();
    app.submit_command(cmd).await.unwrap();

    let report = app.run_phase(Phase::BeforeInference).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 2); // step_one + step_two
    assert_eq!(report.rounds, 3); // round 1: step_one, round 2: step_two, round 3: empty → exit
}

// ===========================================================================
// 12. Hook effect emission during phase execution
// ===========================================================================

#[tokio::test]
async fn hook_emitted_effects_dispatched_during_phase() {
    let recorder = EffectRecorder::default();

    struct EffectHook;
    #[async_trait]
    impl PhaseHook for EffectHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            let mut cmd = StateCommand::new();
            cmd.effect(RuntimeEffect::AddSystemReminder {
                message: "from hook".into(),
            })?;
            Ok(cmd)
        }
    }

    struct EffectPlugin;
    impl Plugin for EffectPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "effect-plugin",
            }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_phase_hook("effect-plugin", Phase::RunStart, EffectHook)?;
            Ok(())
        }
    }

    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(recorder.clone())
        .unwrap();
    app.install_plugin(EffectPlugin).unwrap();

    let report = app.run_phase(Phase::RunStart).await.unwrap();
    assert_eq!(report.effect_report.dispatched, 1);

    let effects = recorder.0.lock().unwrap();
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        &effects[0],
        RuntimeEffect::AddSystemReminder { message } if message == "from hook"
    ));
}

// ===========================================================================
// 13. StateKey encode/decode
// ===========================================================================

#[test]
fn state_key_encode_decode_roundtrip() {
    let val: i64 = 42;
    let json = Counter::encode(&val).unwrap();
    let decoded: i64 = Counter::decode(json).unwrap();
    assert_eq!(decoded, 42);
}

#[test]
fn state_key_encode_decode_complex_value() {
    let val = EventLog {
        entries: vec!["a".into(), "b".into()],
    };
    let json = Events::encode(&val).unwrap();
    let decoded = Events::decode(json).unwrap();
    assert_eq!(decoded, val);
}

#[test]
fn state_key_decode_wrong_type_fails() {
    let json = serde_json::json!("not a number");
    let err = Counter::decode(json).unwrap_err();
    assert!(matches!(err, StateError::KeyDecode { .. }));
}

// ===========================================================================
// 14. Concurrent commit stress test
// ===========================================================================

#[test]
fn concurrent_commits_are_serialized_correctly() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let handles: Vec<_> = (0..16)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                // No base_revision → no conflict, all should succeed
                let mut p = MutationBatch::new();
                p.update::<Counter>(1);
                store.commit(p).unwrap();
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(store.read::<Counter>(), Some(16));
}

#[test]
fn concurrent_commits_with_base_revision_only_one_wins() {
    let store = StateStore::new();
    store.install_plugin(CounterPlugin).unwrap();

    let base = store.revision();
    let success_count = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let store = store.clone();
            let success_count = Arc::clone(&success_count);
            std::thread::spawn(move || {
                let mut p = MutationBatch::new().with_base_revision(base);
                p.update::<Counter>(i + 1);
                if store.commit(p).is_ok() {
                    success_count.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Exactly one thread should succeed (the first to commit)
    assert_eq!(success_count.load(Ordering::SeqCst), 1);
    // Counter should have the winner's value
    assert!(store.read::<Counter>().unwrap() > 0);
}

// ===========================================================================
// 15. Parallel merge — MutationBatch::merge_parallel
// ===========================================================================

struct ParallelPlugin;
impl Plugin for ParallelPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "parallel-plugin",
        }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<Counter>(StateKeyOptions::default())?;
        r.register_key::<Label>(StateKeyOptions::default())?;
        r.register_key::<SharedCounter>(StateKeyOptions::default())?;
        r.register_key::<Events>(StateKeyOptions::default())?;
        Ok(())
    }
}

#[test]
fn merge_parallel_disjoint_keys_succeeds() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Counter>(10);

    let mut b = MutationBatch::new();
    b.update::<Label>("hello".into());

    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();

    assert_eq!(store.read::<Counter>(), Some(10));
    assert_eq!(store.read::<Label>().as_deref(), Some("hello"));
}

#[test]
fn merge_parallel_exclusive_overlap_rejected() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Counter>(1);

    let mut b = MutationBatch::new();
    b.update::<Counter>(2);

    let err = store.merge_parallel(a, b).err().expect("should fail");
    assert!(matches!(err, StateError::ParallelMergeConflict { ref key } if key == "test.counter"));
}

#[test]
fn merge_parallel_commutative_overlap_succeeds() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<SharedCounter>(3);

    let mut b = MutationBatch::new();
    b.update::<SharedCounter>(7);

    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();

    // 0 + 3 + 7 = 10, order doesn't matter
    assert_eq!(store.read::<SharedCounter>(), Some(10));
}

#[test]
fn merge_parallel_mixed_exclusive_and_commutative() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    // a: writes exclusive Counter + commutative SharedCounter
    let mut a = MutationBatch::new();
    a.update::<Counter>(1);
    a.update::<SharedCounter>(5);

    // b: writes commutative SharedCounter + exclusive Label (different from Counter)
    let mut b = MutationBatch::new();
    b.update::<SharedCounter>(3);
    b.update::<Label>("world".into());

    // SharedCounter overlaps but is Commutative → OK
    // Counter and Label are disjoint → OK
    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();

    assert_eq!(store.read::<Counter>(), Some(1));
    assert_eq!(store.read::<SharedCounter>(), Some(8));
    assert_eq!(store.read::<Label>().as_deref(), Some("world"));
}

#[test]
fn merge_parallel_one_exclusive_overlap_blocks_entire_merge() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Counter>(1); // exclusive
    a.update::<SharedCounter>(5); // commutative

    let mut b = MutationBatch::new();
    b.update::<Counter>(2); // exclusive — conflicts with a
    b.update::<SharedCounter>(3); // commutative — would be fine alone

    let err = store.merge_parallel(a, b).err().expect("should fail");
    assert!(matches!(err, StateError::ParallelMergeConflict { .. }));
}

#[test]
fn merge_parallel_empty_batches() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let a = MutationBatch::new();
    let b = MutationBatch::new();
    let merged = store.merge_parallel(a, b).unwrap();
    assert!(merged.is_empty());
}

#[test]
fn merge_parallel_one_empty_one_non_empty() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Counter>(42);
    let b = MutationBatch::new();

    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();
    assert_eq!(store.read::<Counter>(), Some(42));
}

#[test]
fn merge_parallel_preserves_base_revision() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let rev = store.revision();

    let mut a = MutationBatch::new().with_base_revision(rev);
    a.update::<Counter>(1);
    let mut b = MutationBatch::new().with_base_revision(rev);
    b.update::<Label>("x".into());

    let merged = store.merge_parallel(a, b).unwrap();
    assert_eq!(merged.base_revision(), Some(rev));
    store.commit(merged).unwrap();
}

#[test]
fn merge_parallel_mismatched_base_revision_rejected() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let a = MutationBatch::new().with_base_revision(1);
    let b = MutationBatch::new().with_base_revision(2);

    let err = store.merge_parallel(a, b).err().expect("should fail");
    assert!(matches!(
        err,
        StateError::MutationBaseRevisionMismatch { .. }
    ));
}

#[test]
fn merge_parallel_commutative_preserves_op_order() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Events>("from-a".into());

    let mut b = MutationBatch::new();
    b.update::<Events>("from-b".into());

    // Events is Exclusive by default, so this would fail.
    // Use merge_parallel directly with a custom strategy for this test.
    let merged = a.merge_parallel(b, |_| MergeStrategy::Commutative).unwrap();
    store.commit(merged).unwrap();

    let log = store.read::<Events>().unwrap();
    // a's ops come before b's ops
    assert_eq!(log.entries, vec!["from-a", "from-b"]);
}

#[test]
fn merge_parallel_three_way_via_chaining() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<SharedCounter>(1);
    let mut b = MutationBatch::new();
    b.update::<SharedCounter>(2);
    let mut c = MutationBatch::new();
    c.update::<SharedCounter>(3);

    let ab = store.merge_parallel(a, b).unwrap();
    let abc = store.merge_parallel(ab, c).unwrap();
    store.commit(abc).unwrap();

    assert_eq!(store.read::<SharedCounter>(), Some(6));
}

#[test]
fn merge_parallel_commutative_multiple_updates_per_batch() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<SharedCounter>(1);
    a.update::<SharedCounter>(2);
    a.update::<SharedCounter>(3);

    let mut b = MutationBatch::new();
    b.update::<SharedCounter>(10);
    b.update::<SharedCounter>(20);

    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();

    // 1+2+3+10+20 = 36
    assert_eq!(store.read::<SharedCounter>(), Some(36));
}

#[test]
fn merge_parallel_commutative_negative_deltas() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    // Seed initial value
    let mut seed = MutationBatch::new();
    seed.update::<SharedCounter>(100);
    store.commit(seed).unwrap();

    let mut a = MutationBatch::new();
    a.update::<SharedCounter>(-30);

    let mut b = MutationBatch::new();
    b.update::<SharedCounter>(-20);

    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();

    assert_eq!(store.read::<SharedCounter>(), Some(50));
}

#[test]
fn merge_parallel_four_way_all_disjoint() {
    struct ModePlugin;
    impl Plugin for ModePlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "mode" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_key::<Counter>(StateKeyOptions::default())?;
            r.register_key::<Label>(StateKeyOptions::default())?;
            r.register_key::<Events>(StateKeyOptions::default())?;
            r.register_key::<Mode>(StateKeyOptions::default())?;
            Ok(())
        }
    }

    let store = StateStore::new();
    store.install_plugin(ModePlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Counter>(1);
    let mut b = MutationBatch::new();
    b.update::<Label>("b".into());
    let mut c = MutationBatch::new();
    c.update::<Events>("c".into());
    let mut d = MutationBatch::new();
    d.update::<Mode>(Some("d".into()));

    let ab = store.merge_parallel(a, b).unwrap();
    let cd = store.merge_parallel(c, d).unwrap();
    let abcd = store.merge_parallel(ab, cd).unwrap();
    store.commit(abcd).unwrap();

    assert_eq!(store.read::<Counter>(), Some(1));
    assert_eq!(store.read::<Label>().as_deref(), Some("b"));
    assert_eq!(store.read::<Events>().unwrap().entries, vec!["c"]);
    assert_eq!(store.read::<Mode>().unwrap().as_deref(), Some("d"));
}

#[test]
fn merge_parallel_commutative_then_commit_with_base_revision() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let rev = store.revision();

    let mut a = MutationBatch::new().with_base_revision(rev);
    a.update::<SharedCounter>(5);
    let mut b = MutationBatch::new().with_base_revision(rev);
    b.update::<SharedCounter>(10);

    let merged = store.merge_parallel(a, b).unwrap();
    let new_rev = store.commit(merged).unwrap();
    assert_eq!(new_rev, rev + 1);
    assert_eq!(store.read::<SharedCounter>(), Some(15));

    // A second merge with stale base_revision should fail on commit
    let mut c = MutationBatch::new().with_base_revision(rev);
    c.update::<SharedCounter>(1);
    let err = store.commit(c).unwrap_err();
    assert!(matches!(err, StateError::RevisionConflict { .. }));
}

#[test]
fn merge_parallel_commutative_idempotent_zero_update() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut seed = MutationBatch::new();
    seed.update::<SharedCounter>(42);
    store.commit(seed).unwrap();

    let mut a = MutationBatch::new();
    a.update::<SharedCounter>(0);
    let mut b = MutationBatch::new();
    b.update::<SharedCounter>(0);

    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();

    assert_eq!(store.read::<SharedCounter>(), Some(42));
}

#[test]
fn merge_parallel_exclusive_detects_first_conflicting_key() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Counter>(1);
    a.update::<Label>("a".into());

    let mut b = MutationBatch::new();
    b.update::<Label>("b".into()); // Label is Exclusive — conflict
    b.update::<Counter>(2); // Counter is also Exclusive — another conflict

    let err = store.merge_parallel(a, b).err().expect("should fail");
    // Should detect at least one conflict
    assert!(matches!(err, StateError::ParallelMergeConflict { .. }));
}

#[test]
fn merge_parallel_left_none_right_some_base_revision() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let a = MutationBatch::new(); // no base_revision
    let mut b = MutationBatch::new().with_base_revision(5);
    b.update::<Counter>(1);

    let merged = store.merge_parallel(a, b).unwrap();
    assert_eq!(merged.base_revision(), Some(5));
}

#[test]
fn merge_parallel_left_some_right_none_base_revision() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new().with_base_revision(3);
    a.update::<Counter>(1);
    let b = MutationBatch::new(); // no base_revision

    let merged = store.merge_parallel(a, b).unwrap();
    assert_eq!(merged.base_revision(), Some(3));
}

#[test]
fn merge_parallel_unregistered_key_defaults_to_exclusive() {
    // Two batches both updating an unregistered key — registry returns Exclusive by default
    let store = StateStore::new();
    // Counter not registered — no plugin installed

    let mut a = MutationBatch::new();
    a.update::<Counter>(1);
    let mut b = MutationBatch::new();
    b.update::<Counter>(2);

    let err = store.merge_parallel(a, b).err().expect("should fail");
    assert!(matches!(err, StateError::ParallelMergeConflict { .. }));
}

#[test]
fn merge_parallel_symmetric_commutative_same_result() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    // Merge a+b vs b+a — should produce same final state
    let make_batches = || {
        let mut a = MutationBatch::new();
        a.update::<SharedCounter>(3);
        let mut b = MutationBatch::new();
        b.update::<SharedCounter>(7);
        (a, b)
    };

    let (a1, b1) = make_batches();
    let merged_ab = store.merge_parallel(a1, b1).unwrap();
    store.commit(merged_ab).unwrap();
    let result_ab = store.read::<SharedCounter>().unwrap();

    // Reset
    let mut reset = MutationBatch::new();
    reset.update::<SharedCounter>(-result_ab);
    store.commit(reset).unwrap();

    let (a2, b2) = make_batches();
    let merged_ba = store.merge_parallel(b2, a2).unwrap();
    store.commit(merged_ba).unwrap();
    let result_ba = store.read::<SharedCounter>().unwrap();

    assert_eq!(result_ab, result_ba);
}

#[test]
fn merge_parallel_concurrent_threads_merge_commutative() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    // Simulate parallel tool execution: each thread produces a batch
    let batches: Vec<MutationBatch> = (0..8)
        .map(|i| {
            let mut b = MutationBatch::new();
            b.update::<SharedCounter>(i + 1);
            b
        })
        .collect();

    // Merge all batches
    let mut merged = MutationBatch::new();
    for batch in batches {
        merged = store.merge_parallel(merged, batch).unwrap();
    }
    store.commit(merged).unwrap();

    // 1+2+3+4+5+6+7+8 = 36
    assert_eq!(store.read::<SharedCounter>(), Some(36));
}

#[test]
fn merge_parallel_disjoint_with_multiple_ops_per_key() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    let mut a = MutationBatch::new();
    a.update::<Counter>(1);
    a.update::<Counter>(2);
    a.update::<Counter>(3);

    let mut b = MutationBatch::new();
    b.update::<Label>("first".into());
    b.update::<Label>("second".into()); // replaces

    let merged = store.merge_parallel(a, b).unwrap();
    store.commit(merged).unwrap();

    assert_eq!(store.read::<Counter>(), Some(6));
    assert_eq!(store.read::<Label>().as_deref(), Some("second"));
}

#[test]
fn merge_parallel_after_prior_state_exists() {
    let store = StateStore::new();
    store.install_plugin(ParallelPlugin).unwrap();

    // Pre-existing state
    let mut seed = MutationBatch::new();
    seed.update::<Counter>(100);
    seed.update::<SharedCounter>(50);
    store.commit(seed).unwrap();

    // Parallel batches on top of existing state
    let mut a = MutationBatch::new();
    a.update::<Counter>(10); // exclusive, only a writes it

    let mut b = MutationBatch::new();
    b.update::<SharedCounter>(5); // commutative

    let mut c = MutationBatch::new();
    c.update::<SharedCounter>(3); // commutative

    let ab = store.merge_parallel(a, b).unwrap();
    let abc = store.merge_parallel(ab, c).unwrap();
    store.commit(abc).unwrap();

    assert_eq!(store.read::<Counter>(), Some(110));
    assert_eq!(store.read::<SharedCounter>(), Some(58));
}
