//! Integration tests for lattice-aware conflict resolution (Phase 3).
//!
//! Validates that `commit_batch` auto-merges `LatticeMerge` ops at registered
//! lattice paths instead of rejecting them as conflicts.
#![allow(missing_docs)]

use serde_json::json;
use tirea_state::{
    apply_patch_with_registry, Flag, GCounter, LatticeRegistry, Op, Patch, Path, StateManager,
    TrackedPatch,
};

fn make_patch(ops: Vec<Op>, source: &str) -> TrackedPatch {
    TrackedPatch::new(Patch::with_ops(ops)).with_source(source)
}

// ============================================================================
// Concurrent lattice merge tests
// ============================================================================

/// Two patches each merge different GCounter nodes at the same path.
/// Both use LatticeMerge → batch should pass and result is correct.
#[tokio::test]
async fn test_concurrent_gcounter_merge() {
    let mut initial_counter = GCounter::new();
    initial_counter.increment("n1", 5);

    let manager = StateManager::new(json!({
        "counter": serde_json::to_value(&initial_counter).unwrap(),
    }));

    manager
        .register_lattice::<GCounter>(Path::root().key("counter"))
        .await;

    // Agent A: merge {n2: 3}
    let mut delta_a = GCounter::new();
    delta_a.increment("n2", 3);

    // Agent B: merge {n1: 8} (higher than existing n1=5)
    let mut delta_b = GCounter::new();
    delta_b.increment("n1", 8);

    let patches = vec![
        make_patch(
            vec![Op::lattice_merge(
                Path::root().key("counter"),
                serde_json::to_value(&delta_a).unwrap(),
            )],
            "agent_a",
        ),
        make_patch(
            vec![Op::lattice_merge(
                Path::root().key("counter"),
                serde_json::to_value(&delta_b).unwrap(),
            )],
            "agent_b",
        ),
    ];

    let result = manager.commit_batch(patches).await.unwrap();
    assert_eq!(result.patches_applied, 2);

    let state = manager.snapshot().await;
    let merged: GCounter = serde_json::from_value(state["counter"].clone()).unwrap();

    // n1: merge(merge(5, 0_from_delta_a), 8) = merge(5, 8) = 8
    assert_eq!(merged.node_value("n1"), 8);
    // n2: merge(merge(0, 3), 0_from_delta_b) = merge(3, 0) = 3
    assert_eq!(merged.node_value("n2"), 3);
    // total: 8 + 3 = 11
    assert_eq!(merged.value(), 11);
}

/// Three patches each merge Flag::enable() at the same path.
/// All use LatticeMerge → batch should pass; result is enabled.
#[tokio::test]
async fn test_concurrent_flag_enable() {
    let manager = StateManager::new(json!({
        "active": false,
    }));

    manager
        .register_lattice::<Flag>(Path::root().key("active"))
        .await;

    let mut flag = Flag::new();
    flag.enable();
    let flag_val = serde_json::to_value(&flag).unwrap();

    let patches = vec![
        make_patch(
            vec![Op::lattice_merge(
                Path::root().key("active"),
                flag_val.clone(),
            )],
            "agent_1",
        ),
        make_patch(
            vec![Op::lattice_merge(
                Path::root().key("active"),
                flag_val.clone(),
            )],
            "agent_2",
        ),
        make_patch(
            vec![Op::lattice_merge(Path::root().key("active"), flag_val)],
            "agent_3",
        ),
    ];

    let result = manager.commit_batch(patches).await.unwrap();
    assert_eq!(result.patches_applied, 3);

    let state = manager.snapshot().await;
    let merged: Flag = serde_json::from_value(state["active"].clone()).unwrap();
    assert!(merged.is_enabled());
}

// ============================================================================
// Mixed op conflict tests
// ============================================================================

/// One patch uses Op::Set, the other uses Op::LatticeMerge at the same path.
/// This should still conflict (not both LatticeMerge).
#[tokio::test]
async fn test_set_vs_lattice_merge_still_conflicts() {
    let manager = StateManager::new(json!({
        "counter": serde_json::to_value(GCounter::new()).unwrap(),
    }));

    manager
        .register_lattice::<GCounter>(Path::root().key("counter"))
        .await;

    let mut delta = GCounter::new();
    delta.increment("n1", 3);

    let patches = vec![
        make_patch(
            vec![Op::set(
                Path::root().key("counter"),
                serde_json::to_value(GCounter::new()).unwrap(),
            )],
            "setter",
        ),
        make_patch(
            vec![Op::lattice_merge(
                Path::root().key("counter"),
                serde_json::to_value(&delta).unwrap(),
            )],
            "merger",
        ),
    ];

    let err = manager.commit_batch(patches).await.unwrap_err();
    assert!(
        err.to_string().contains("conflicting"),
        "expected conflict, got: {err}"
    );
}

// ============================================================================
// Sequential lattice merge
// ============================================================================

/// Single-agent commit(LatticeMerge) accumulates correctly.
#[tokio::test]
async fn test_lattice_merge_sequential() {
    let manager = StateManager::new(json!({}));

    manager
        .register_lattice::<GCounter>(Path::root().key("counter"))
        .await;

    // First merge: {n1: 5}
    let mut c1 = GCounter::new();
    c1.increment("n1", 5);
    manager
        .commit(make_patch(
            vec![Op::lattice_merge(
                Path::root().key("counter"),
                serde_json::to_value(&c1).unwrap(),
            )],
            "step1",
        ))
        .await
        .unwrap();

    // Second merge: {n2: 3}
    let mut c2 = GCounter::new();
    c2.increment("n2", 3);
    manager
        .commit(make_patch(
            vec![Op::lattice_merge(
                Path::root().key("counter"),
                serde_json::to_value(&c2).unwrap(),
            )],
            "step2",
        ))
        .await
        .unwrap();

    let state = manager.snapshot().await;
    let result: GCounter = serde_json::from_value(state["counter"].clone()).unwrap();
    assert_eq!(result.node_value("n1"), 5);
    assert_eq!(result.node_value("n2"), 3);
    assert_eq!(result.value(), 8);
}

// ============================================================================
// Replay with lattice ops
// ============================================================================

/// `replay_to` correctly replays LatticeMerge ops.
#[tokio::test]
async fn test_replay_with_lattice_ops() {
    let manager = StateManager::new(json!({}));

    manager
        .register_lattice::<GCounter>(Path::root().key("counter"))
        .await;

    let mut c1 = GCounter::new();
    c1.increment("n1", 5);
    manager
        .commit(make_patch(
            vec![Op::lattice_merge(
                Path::root().key("counter"),
                serde_json::to_value(&c1).unwrap(),
            )],
            "step1",
        ))
        .await
        .unwrap();

    let mut c2 = GCounter::new();
    c2.increment("n2", 3);
    manager
        .commit(make_patch(
            vec![Op::lattice_merge(
                Path::root().key("counter"),
                serde_json::to_value(&c2).unwrap(),
            )],
            "step2",
        ))
        .await
        .unwrap();

    // Replay to index 0: only first merge applied
    let state0 = manager.replay_to(0).await.unwrap();
    let r0: GCounter = serde_json::from_value(state0["counter"].clone()).unwrap();
    assert_eq!(r0.value(), 5);

    // Replay to index 1: both merges applied
    let state1 = manager.replay_to(1).await.unwrap();
    let r1: GCounter = serde_json::from_value(state1["counter"].clone()).unwrap();
    assert_eq!(r1.value(), 8);
}

// ============================================================================
// Unregistered path fallback
// ============================================================================

/// LatticeMerge on an unregistered path falls back to Set semantics.
#[tokio::test]
async fn test_unregistered_path_fallback() {
    let mut registry = LatticeRegistry::new();
    // Intentionally NOT registering the path

    let doc = json!({
        "counter": {"n1": 5},
    });

    let mut delta = GCounter::new();
    delta.increment("n2", 3);

    let patch = Patch::new().with_op(Op::lattice_merge(
        Path::root().key("counter"),
        serde_json::to_value(&delta).unwrap(),
    ));

    // Without registry entry, LatticeMerge acts like Set (writes delta as-is)
    let result = apply_patch_with_registry(&doc, &patch, &registry).unwrap();
    let written: GCounter = serde_json::from_value(result["counter"].clone()).unwrap();

    // Fallback: delta replaces existing value (no merge)
    assert_eq!(written.node_value("n2"), 3);
    assert_eq!(written.node_value("n1"), 0); // old value is gone
    assert_eq!(written.value(), 3);

    // Now register and apply again – should merge properly
    registry.register::<GCounter>(Path::root().key("counter"));
    let result = apply_patch_with_registry(&doc, &patch, &registry).unwrap();
    let merged: GCounter = serde_json::from_value(result["counter"].clone()).unwrap();

    assert_eq!(merged.node_value("n1"), 5); // preserved via merge
    assert_eq!(merged.node_value("n2"), 3);
    assert_eq!(merged.value(), 8);
}
