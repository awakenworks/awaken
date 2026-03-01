//! Integration tests for #[tirea(lattice)] derive support.
#![allow(missing_docs)]

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Mutex;
use tirea_state::{
    apply_patch, DocCell, Flag, GCounter, GSet, PatchSink, Path,
    State as StateTrait,
};
use tirea_state_derive::State;

/// Helper to create a state ref and collect patches for testing.
fn with_state_ref<T: StateTrait, F>(doc: &serde_json::Value, path: Path, f: F) -> tirea_state::Patch
where
    F: FnOnce(T::Ref<'_>),
{
    let doc_cell = DocCell::new(doc.clone());
    let ops = Mutex::new(Vec::new());
    let sink = PatchSink::new(&ops);
    let state_ref = T::state_ref(&doc_cell, path, sink);
    f(state_ref);
    tirea_state::Patch::with_ops(ops.into_inner().unwrap())
}

// ============================================================================
// Test structs
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State)]
struct WithCounter {
    name: String,
    #[tirea(lattice)]
    counter: GCounter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State)]
struct WithFlag {
    #[tirea(lattice)]
    enabled: Flag,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State)]
struct WithDefault {
    #[tirea(lattice, default = "GCounter::new()")]
    counter: GCounter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, State)]
struct MultiLattice {
    #[tirea(lattice)]
    flag: Flag,
    #[tirea(lattice)]
    counter: GCounter,
    #[tirea(lattice)]
    tags: GSet<String>,
}

// ============================================================================
// Read tests
// ============================================================================

#[test]
fn test_lattice_read_present() {
    let mut counter = GCounter::new();
    counter.increment("node1", 5);
    let counter_json = serde_json::to_value(&counter).unwrap();

    let doc = json!({
        "name": "test",
        "counter": counter_json,
    });

    let patch = with_state_ref::<WithCounter, _>(&doc, Path::root(), |state| {
        let c = state.counter().unwrap();
        assert_eq!(c.value(), 5);
        assert_eq!(c.node_value("node1"), 5);
    });

    assert!(patch.is_empty());
}

#[test]
fn test_lattice_read_flag() {
    let mut flag = Flag::new();
    flag.enable();

    let doc = json!({
        "enabled": serde_json::to_value(&flag).unwrap(),
    });

    let patch = with_state_ref::<WithFlag, _>(&doc, Path::root(), |state| {
        let f = state.enabled().unwrap();
        assert!(f.is_enabled());
    });

    assert!(patch.is_empty());
}

#[test]
fn test_lattice_read_missing_errors() {
    let doc = json!({
        "name": "test",
    });

    let _ = with_state_ref::<WithCounter, _>(&doc, Path::root(), |state| {
        let err = state.counter().unwrap_err();
        assert!(
            matches!(err, tirea_state::TireaError::PathNotFound { .. }),
            "expected PathNotFound, got: {err:?}"
        );
    });
}

#[test]
fn test_lattice_read_with_default() {
    let doc = json!({});

    let patch = with_state_ref::<WithDefault, _>(&doc, Path::root(), |state| {
        let c = state.counter().unwrap();
        assert_eq!(c.value(), 0);
    });

    assert!(patch.is_empty());
}

// ============================================================================
// Set tests
// ============================================================================

#[test]
fn test_lattice_set() {
    let doc = json!({});

    let mut counter = GCounter::new();
    counter.increment("node1", 10);

    let patch = with_state_ref::<WithCounter, _>(&doc, Path::root(), |state| {
        state.set_name("test").unwrap();
        state.set_counter(counter.clone()).unwrap();
    });

    assert_eq!(patch.len(), 2);

    let result = apply_patch(&doc, &patch).unwrap();
    assert_eq!(result["name"], "test");

    let read_back: GCounter = serde_json::from_value(result["counter"].clone()).unwrap();
    assert_eq!(read_back.value(), 10);
}

// ============================================================================
// Merge tests
// ============================================================================

#[test]
fn test_lattice_merge_from_empty() {
    let doc = json!({
        "name": "test",
    });

    let mut other = GCounter::new();
    other.increment("node1", 7);

    let patch = with_state_ref::<WithCounter, _>(&doc, Path::root(), |state| {
        state.merge_counter(&other).unwrap();
    });

    assert_eq!(patch.len(), 1);

    let result = apply_patch(&doc, &patch).unwrap();
    let read_back: GCounter = serde_json::from_value(result["counter"].clone()).unwrap();
    assert_eq!(read_back.value(), 7);
    assert_eq!(read_back.node_value("node1"), 7);
}

#[test]
fn test_lattice_merge_existing() {
    let mut existing = GCounter::new();
    existing.increment("node1", 5);
    existing.increment("node2", 3);

    let doc = json!({
        "name": "test",
        "counter": serde_json::to_value(&existing).unwrap(),
    });

    let mut other = GCounter::new();
    other.increment("node1", 8); // higher than existing node1=5
    other.increment("node3", 2); // new node

    let patch = with_state_ref::<WithCounter, _>(&doc, Path::root(), |state| {
        state.merge_counter(&other).unwrap();
    });

    let result = apply_patch(&doc, &patch).unwrap();
    let merged: GCounter = serde_json::from_value(result["counter"].clone()).unwrap();

    // node1: max(5, 8) = 8
    assert_eq!(merged.node_value("node1"), 8);
    // node2: max(3, 0) = 3
    assert_eq!(merged.node_value("node2"), 3);
    // node3: max(0, 2) = 2
    assert_eq!(merged.node_value("node3"), 2);
    // total: 8 + 3 + 2 = 13
    assert_eq!(merged.value(), 13);
}

#[test]
fn test_lattice_merge_flag() {
    let doc = json!({
        "enabled": false,
    });

    let mut flag = Flag::new();
    flag.enable();

    let patch = with_state_ref::<WithFlag, _>(&doc, Path::root(), |state| {
        state.merge_enabled(&flag).unwrap();
    });

    let result = apply_patch(&doc, &patch).unwrap();
    let merged: Flag = serde_json::from_value(result["enabled"].clone()).unwrap();
    assert!(merged.is_enabled());
}

// ============================================================================
// Delete tests
// ============================================================================

#[test]
fn test_lattice_delete() {
    let mut counter = GCounter::new();
    counter.increment("n", 1);

    let doc = json!({
        "name": "test",
        "counter": serde_json::to_value(&counter).unwrap(),
    });

    let patch = with_state_ref::<WithCounter, _>(&doc, Path::root(), |state| {
        state.delete_counter().unwrap();
    });

    assert_eq!(patch.len(), 1);
    let result = apply_patch(&doc, &patch).unwrap();
    assert!(result.get("counter").is_none());
    assert_eq!(result["name"], "test");
}

// ============================================================================
// Multiple lattice fields
// ============================================================================

#[test]
fn test_lattice_multiple_fields() {
    let mut flag = Flag::new();
    flag.enable();

    let mut counter = GCounter::new();
    counter.increment("n1", 10);

    let mut tags = GSet::new();
    tags.insert("alpha".to_string());
    tags.insert("beta".to_string());

    let doc = json!({});

    let patch = with_state_ref::<MultiLattice, _>(&doc, Path::root(), |state| {
        state.set_flag(flag.clone()).unwrap();
        state.set_counter(counter.clone()).unwrap();
        state.set_tags(tags.clone()).unwrap();
    });

    assert_eq!(patch.len(), 3);

    let result = apply_patch(&doc, &patch).unwrap();

    let f: Flag = serde_json::from_value(result["flag"].clone()).unwrap();
    assert!(f.is_enabled());

    let c: GCounter = serde_json::from_value(result["counter"].clone()).unwrap();
    assert_eq!(c.value(), 10);

    let t: GSet<String> = serde_json::from_value(result["tags"].clone()).unwrap();
    assert!(t.contains(&"alpha".to_string()));
    assert!(t.contains(&"beta".to_string()));
    assert_eq!(t.len(), 2);
}

#[test]
fn test_lattice_multiple_fields_merge() {
    let mut flag = Flag::new();
    flag.enable();
    let mut counter = GCounter::new();
    counter.increment("n1", 5);
    let mut tags = GSet::new();
    tags.insert("existing".to_string());

    let doc = json!({
        "flag": serde_json::to_value(&flag).unwrap(),
        "counter": serde_json::to_value(&counter).unwrap(),
        "tags": serde_json::to_value(&tags).unwrap(),
    });

    let mut other_counter = GCounter::new();
    other_counter.increment("n2", 3);

    let mut other_tags = GSet::new();
    other_tags.insert("new_tag".to_string());

    let patch = with_state_ref::<MultiLattice, _>(&doc, Path::root(), |state| {
        state.merge_counter(&other_counter).unwrap();
        state.merge_tags(&other_tags).unwrap();
    });

    let result = apply_patch(&doc, &patch).unwrap();

    let c: GCounter = serde_json::from_value(result["counter"].clone()).unwrap();
    assert_eq!(c.value(), 8); // 5 + 3

    let t: GSet<String> = serde_json::from_value(result["tags"].clone()).unwrap();
    assert!(t.contains(&"existing".to_string()));
    assert!(t.contains(&"new_tag".to_string()));
    assert_eq!(t.len(), 2);
}
