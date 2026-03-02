use super::scope_context::ScopeContext;
use super::spec::{reduce_state_actions, AnyStateAction, StateScope};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tirea_state::{StateSpec, TrackedPatch};

/// Serialized state action, sufficient to reconstruct an [`AnyStateAction::Typed`].
///
/// Captured at the point where a tool completes execution, before the batch
/// commit. On crash recovery, these entries are deserialized back into
/// `AnyStateAction` via [`ActionDeserializerRegistry`] and re-reduced against
/// the base state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedAction {
    /// `std::any::type_name::<S>()` — used as the registry lookup key.
    pub state_type_name: String,
    /// `S::PATH` — the canonical JSON path for this state type.
    pub base_path: String,
    /// Whether this action targets run-level or tool-call-level state.
    pub scope: StateScope,
    /// When set, overrides the scope context call_id for path resolution.
    pub call_id_override: Option<String>,
    /// The serialized `S::Action` value.
    pub payload: Value,
}

/// A single tool call's pending state writes.
///
/// Persisted immediately after a tool completes execution, before the batch
/// commit. On recovery, all entries for a run are read and replayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingWriteEntry {
    pub run_id: String,
    pub thread_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub actions: Vec<SerializedAction>,
    pub created_at: u64,
}

/// Errors from pending-write operations.
#[derive(Debug, thiserror::Error)]
pub enum PendingWriteError {
    #[error("unknown state type: {0}")]
    UnknownStateType(String),
    #[error("action deserialization failed for {state_type}: {source}")]
    DeserializationFailed {
        state_type: String,
        source: serde_json::Error,
    },
    #[error("state reduce failed during recovery: {0}")]
    Reduce(String),
    #[error("store I/O error: {0}")]
    Store(String),
}

// ---------------------------------------------------------------------------
// AnyStateAction → SerializedAction
// ---------------------------------------------------------------------------

impl AnyStateAction {
    /// Convert this action into a serialized form for pending-write persistence.
    ///
    /// Returns `None` for raw `Patch` actions (they bypass the typed reducer
    /// pipeline and are not recoverable via action replay).
    pub fn to_serialized_action(&self) -> Option<SerializedAction> {
        match self {
            Self::Typed {
                state_type_name,
                scope,
                base_path,
                call_id_override,
                serialized_payload,
                ..
            } => Some(SerializedAction {
                state_type_name: (*state_type_name).to_owned(),
                base_path: (*base_path).to_owned(),
                scope: *scope,
                call_id_override: call_id_override.clone(),
                payload: serialized_payload.clone(),
            }),
            Self::Patch(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ActionDeserializerRegistry
// ---------------------------------------------------------------------------

type ActionFactory =
    Box<dyn Fn(&SerializedAction) -> Result<AnyStateAction, PendingWriteError> + Send + Sync>;

/// Registry that maps `state_type_name` → factory closure for reconstructing
/// `AnyStateAction` from a [`SerializedAction`].
///
/// Built once at agent construction (alongside `StateScopeRegistry` and
/// `LatticeRegistry`) by calling `register::<S>()` for every `StateSpec` type.
pub struct ActionDeserializerRegistry {
    factories: HashMap<String, ActionFactory>,
}

impl ActionDeserializerRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Register a `StateSpec` type so its actions can be deserialized from
    /// pending writes.
    pub fn register<S: StateSpec>(&mut self) {
        let type_name = std::any::type_name::<S>().to_owned();
        self.factories.insert(
            type_name,
            Box::new(|entry: &SerializedAction| {
                let action: S::Action = serde_json::from_value(entry.payload.clone())
                    .map_err(|e| PendingWriteError::DeserializationFailed {
                        state_type: entry.state_type_name.clone(),
                        source: e,
                    })?;
                match entry.scope {
                    StateScope::Run => Ok(AnyStateAction::new::<S>(action)),
                    StateScope::ToolCall => {
                        let call_id = entry.call_id_override.as_deref().unwrap_or("");
                        Ok(AnyStateAction::new_for_call::<S>(
                            action,
                            call_id.to_owned(),
                        ))
                    }
                }
            }),
        );
    }

    /// Deserialize a [`SerializedAction`] back into an [`AnyStateAction`].
    pub fn deserialize(
        &self,
        entry: &SerializedAction,
    ) -> Result<AnyStateAction, PendingWriteError> {
        let factory = self
            .factories
            .get(&entry.state_type_name)
            .ok_or_else(|| PendingWriteError::UnknownStateType(entry.state_type_name.clone()))?;
        factory(entry)
    }
}

impl Default for ActionDeserializerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ActionDeserializerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionDeserializerRegistry")
            .field("registered_types", &self.factories.keys().collect::<Vec<_>>())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// PendingWriteStore
// ---------------------------------------------------------------------------

/// Durable store for per-tool pending writes.
///
/// Separated from `StateCommitter` — no version precondition, no conflict
/// with the batch commit path. Implementations can be backed by the
/// filesystem, a database, or in-memory (for tests).
#[async_trait]
pub trait PendingWriteStore: Send + Sync {
    /// Persist a single pending-write entry (called immediately after a tool completes).
    async fn write(&self, entry: PendingWriteEntry) -> Result<(), PendingWriteError>;

    /// Read all pending writes for a given run (called during recovery).
    async fn read(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<Vec<PendingWriteEntry>, PendingWriteError>;

    /// Read all pending writes for a thread, across all runs.
    ///
    /// Used during recovery to find pending writes from previously crashed
    /// runs when the new run has a different `run_id`.
    async fn read_by_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingWriteEntry>, PendingWriteError> {
        let _ = thread_id;
        Ok(Vec::new())
    }

    /// Acknowledge (delete) all pending writes for a run after successful batch commit.
    async fn acknowledge(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<(), PendingWriteError>;
}

// ---------------------------------------------------------------------------
// InMemoryPendingWriteStore (test / development use)
// ---------------------------------------------------------------------------

/// In-memory implementation of [`PendingWriteStore`] for testing.
#[derive(Debug, Default)]
pub struct InMemoryPendingWriteStore {
    /// Key: `(thread_id, run_id)` → entries
    entries: Mutex<HashMap<(String, String), Vec<PendingWriteEntry>>>,
}

impl InMemoryPendingWriteStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PendingWriteStore for InMemoryPendingWriteStore {
    async fn write(&self, entry: PendingWriteEntry) -> Result<(), PendingWriteError> {
        let key = (entry.thread_id.clone(), entry.run_id.clone());
        self.entries
            .lock()
            .map_err(|e| PendingWriteError::Store(e.to_string()))?
            .entry(key)
            .or_default()
            .push(entry);
        Ok(())
    }

    async fn read(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<Vec<PendingWriteEntry>, PendingWriteError> {
        let key = (thread_id.to_owned(), run_id.to_owned());
        Ok(self
            .entries
            .lock()
            .map_err(|e| PendingWriteError::Store(e.to_string()))?
            .get(&key)
            .cloned()
            .unwrap_or_default())
    }

    async fn read_by_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<PendingWriteEntry>, PendingWriteError> {
        let guard = self
            .entries
            .lock()
            .map_err(|e| PendingWriteError::Store(e.to_string()))?;
        Ok(guard
            .iter()
            .filter(|((tid, _), _)| tid == thread_id)
            .flat_map(|(_, entries)| entries.iter().cloned())
            .collect())
    }

    async fn acknowledge(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<(), PendingWriteError> {
        let key = (thread_id.to_owned(), run_id.to_owned());
        self.entries
            .lock()
            .map_err(|e| PendingWriteError::Store(e.to_string()))?
            .remove(&key);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Recover state from a pre-fetched list of pending-write entries.
///
/// This is the synchronous core of recovery: it deserializes actions through
/// the [`ActionDeserializerRegistry`] and reduces them against `base_state`.
///
/// Returns:
/// - `Vec<TrackedPatch>` — patches to apply to the base state to catch up
/// - `HashSet<String>` — call IDs of tools that completed before the crash
pub fn recover_pending_writes_from_entries(
    entries: &[PendingWriteEntry],
    registry: &ActionDeserializerRegistry,
    base_state: &Value,
) -> Result<(Vec<TrackedPatch>, HashSet<String>), PendingWriteError> {
    if entries.is_empty() {
        return Ok((Vec::new(), HashSet::new()));
    }

    let completed_call_ids: HashSet<String> = entries.iter().map(|e| e.call_id.clone()).collect();

    let mut all_actions = Vec::new();
    for entry in entries {
        for sa in &entry.actions {
            all_actions.push(registry.deserialize(sa)?);
        }
    }

    let patches =
        reduce_state_actions(all_actions, base_state, "recovery", &ScopeContext::run())
            .map_err(|e| PendingWriteError::Reduce(e.to_string()))?;

    Ok((patches, completed_call_ids))
}

/// Recover state from pending writes after a crash.
///
/// Reads all pending-write entries for the given run, deserializes their
/// actions through the [`ActionDeserializerRegistry`], and reduces them
/// against `base_state` using rolling-snapshot semantics.
///
/// Returns:
/// - `Vec<TrackedPatch>` — patches to apply to the base state to catch up
/// - `HashSet<String>` — call IDs of tools that completed before the crash
///
/// The caller should apply the patches, then skip re-execution of any tool
/// whose `call_id` is in the returned set.
pub async fn recover_pending_writes(
    thread_id: &str,
    run_id: &str,
    store: &dyn PendingWriteStore,
    registry: &ActionDeserializerRegistry,
    base_state: &Value,
) -> Result<(Vec<TrackedPatch>, HashSet<String>), PendingWriteError> {
    let entries = store.read(thread_id, run_id).await?;
    recover_pending_writes_from_entries(&entries, registry, base_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use tirea_state::{
        apply_patch, DocCell, PatchSink, Path, State, TireaResult,
    };

    // -- Test state type --

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct TestCounter {
        value: i64,
    }

    struct TestCounterRef;

    impl State for TestCounter {
        type Ref<'a> = TestCounterRef;
        const PATH: &'static str = "test_counter";

        fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
            TestCounterRef
        }

        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }

        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    enum TestCounterAction {
        Increment(i64),
        Reset,
    }

    impl StateSpec for TestCounter {
        type Action = TestCounterAction;

        fn reduce(&mut self, action: TestCounterAction) {
            match action {
                TestCounterAction::Increment(n) => self.value += n,
                TestCounterAction::Reset => self.value = 0,
            }
        }
    }

    #[test]
    fn to_serialized_action_roundtrip() {
        let original = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(42));
        let serialized = original.to_serialized_action().expect("Typed → Some");

        assert!(serialized.state_type_name.contains("TestCounter"));
        assert_eq!(serialized.base_path, "test_counter");
        assert_eq!(serialized.scope, StateScope::Run);
        assert!(serialized.call_id_override.is_none());
        assert_eq!(serialized.payload, json!({"Increment": 42}));
    }

    #[test]
    fn to_serialized_action_returns_none_for_patch() {
        let raw = AnyStateAction::Patch(tirea_state::TrackedPatch::new(
            tirea_state::Patch::default(),
        ));
        assert!(raw.to_serialized_action().is_none());
    }

    #[test]
    fn registry_deserialize_and_reduce_roundtrip() {
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        // Create original action, serialize, then deserialize through registry
        let original = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(7));
        let serialized = original.to_serialized_action().unwrap();

        let reconstructed = registry.deserialize(&serialized).unwrap();

        // Reduce both against the same base state
        let base = json!({});
        let original_patches = reduce_state_actions(
            vec![original],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .unwrap();
        let reconstructed_patches = reduce_state_actions(
            vec![reconstructed],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .unwrap();

        // Both should produce identical results
        let result_a = apply_patch(&base, original_patches[0].patch()).unwrap();
        let result_b = apply_patch(&base, reconstructed_patches[0].patch()).unwrap();
        assert_eq!(result_a, result_b);
        assert_eq!(result_a["test_counter"]["value"], 7);
    }

    #[test]
    fn registry_unknown_type_returns_error() {
        let registry = ActionDeserializerRegistry::new();
        let entry = SerializedAction {
            state_type_name: "unknown::Type".into(),
            base_path: "x".into(),
            scope: StateScope::Run,
            call_id_override: None,
            payload: json!(null),
        };
        let err = registry.deserialize(&entry).unwrap_err();
        assert!(matches!(err, PendingWriteError::UnknownStateType(_)));
    }

    #[test]
    fn registry_bad_payload_returns_deserialization_error() {
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        let entry = SerializedAction {
            state_type_name: std::any::type_name::<TestCounter>().into(),
            base_path: "test_counter".into(),
            scope: StateScope::Run,
            call_id_override: None,
            payload: json!({"BadVariant": 99}),
        };
        let err = registry.deserialize(&entry).unwrap_err();
        assert!(matches!(err, PendingWriteError::DeserializationFailed { .. }));
    }

    #[test]
    fn tool_call_scoped_roundtrip() {
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        let original = AnyStateAction::new_for_call::<TestCounter>(
            TestCounterAction::Increment(3),
            "call_99",
        );
        let serialized = original.to_serialized_action().unwrap();
        assert_eq!(serialized.scope, StateScope::ToolCall);
        assert_eq!(serialized.call_id_override, Some("call_99".into()));

        let reconstructed = registry.deserialize(&serialized).unwrap();

        let base = json!({});
        let patches = reduce_state_actions(
            vec![reconstructed],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .unwrap();
        let result = apply_patch(&base, patches[0].patch()).unwrap();
        assert_eq!(
            result["__tool_call_scope"]["call_99"]["test_counter"]["value"],
            3
        );
    }

    #[tokio::test]
    async fn in_memory_store_write_read_acknowledge() {
        let store = InMemoryPendingWriteStore::new();
        let entry = PendingWriteEntry {
            run_id: "run_1".into(),
            thread_id: "thread_1".into(),
            call_id: "call_1".into(),
            tool_name: "my_tool".into(),
            actions: vec![SerializedAction {
                state_type_name: "TestCounter".into(),
                base_path: "test_counter".into(),
                scope: StateScope::Run,
                call_id_override: None,
                payload: json!({"Increment": 1}),
            }],
            created_at: 1000,
        };

        store.write(entry).await.unwrap();

        let entries = store.read("thread_1", "run_1").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].call_id, "call_1");
        assert_eq!(entries[0].actions.len(), 1);

        // Unrelated run returns empty
        let empty = store.read("thread_1", "run_other").await.unwrap();
        assert!(empty.is_empty());

        // Acknowledge removes entries
        store.acknowledge("thread_1", "run_1").await.unwrap();
        let after = store.read("thread_1", "run_1").await.unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn in_memory_store_multiple_entries() {
        let store = InMemoryPendingWriteStore::new();

        for i in 0..3 {
            store
                .write(PendingWriteEntry {
                    run_id: "run_1".into(),
                    thread_id: "t1".into(),
                    call_id: format!("call_{i}"),
                    tool_name: "tool".into(),
                    actions: vec![],
                    created_at: i as u64,
                })
                .await
                .unwrap();
        }

        let entries = store.read("t1", "run_1").await.unwrap();
        assert_eq!(entries.len(), 3);
    }

    // -- Recovery tests --

    #[tokio::test]
    async fn recover_empty_store_returns_empty() {
        let store = InMemoryPendingWriteStore::new();
        let registry = ActionDeserializerRegistry::new();
        let base = json!({});

        let (patches, completed) =
            recover_pending_writes("t1", "run_1", &store, &registry, &base)
                .await
                .unwrap();

        assert!(patches.is_empty());
        assert!(completed.is_empty());
    }

    #[tokio::test]
    async fn recover_single_tool_produces_correct_state() {
        let store = InMemoryPendingWriteStore::new();
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        // Simulate a tool that emitted Increment(10)
        let action = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(10));
        let serialized = action.to_serialized_action().unwrap();

        store
            .write(PendingWriteEntry {
                run_id: "run_1".into(),
                thread_id: "t1".into(),
                call_id: "call_a".into(),
                tool_name: "my_tool".into(),
                actions: vec![serialized],
                created_at: 100,
            })
            .await
            .unwrap();

        let base = json!({});
        let (patches, completed) =
            recover_pending_writes("t1", "run_1", &store, &registry, &base)
                .await
                .unwrap();

        assert_eq!(completed.len(), 1);
        assert!(completed.contains("call_a"));
        assert_eq!(patches.len(), 1);

        let result = apply_patch(&base, patches[0].patch()).unwrap();
        assert_eq!(result["test_counter"]["value"], 10);
    }

    #[tokio::test]
    async fn recover_multiple_tools_with_rolling_snapshot() {
        let store = InMemoryPendingWriteStore::new();
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        // Two tools each incremented the counter
        for (call_id, amount) in [("call_a", 7), ("call_b", 3)] {
            let action = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(amount));
            store
                .write(PendingWriteEntry {
                    run_id: "run_1".into(),
                    thread_id: "t1".into(),
                    call_id: call_id.into(),
                    tool_name: "tool".into(),
                    actions: vec![action.to_serialized_action().unwrap()],
                    created_at: 100,
                })
                .await
                .unwrap();
        }

        let base = json!({});
        let (patches, completed) =
            recover_pending_writes("t1", "run_1", &store, &registry, &base)
                .await
                .unwrap();

        assert_eq!(completed.len(), 2);
        assert!(completed.contains("call_a"));
        assert!(completed.contains("call_b"));

        // Apply all patches to get final state
        let mut state = base;
        for p in &patches {
            state = apply_patch(&state, p.patch()).unwrap();
        }
        assert_eq!(state["test_counter"]["value"], 10); // 7 + 3
    }

    #[tokio::test]
    async fn recover_unknown_type_returns_error() {
        let store = InMemoryPendingWriteStore::new();
        let registry = ActionDeserializerRegistry::new(); // empty — no types registered

        store
            .write(PendingWriteEntry {
                run_id: "run_1".into(),
                thread_id: "t1".into(),
                call_id: "call_a".into(),
                tool_name: "tool".into(),
                actions: vec![SerializedAction {
                    state_type_name: "unknown::Type".into(),
                    base_path: "x".into(),
                    scope: StateScope::Run,
                    call_id_override: None,
                    payload: json!(null),
                }],
                created_at: 100,
            })
            .await
            .unwrap();

        let err = recover_pending_writes("t1", "run_1", &store, &registry, &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, PendingWriteError::UnknownStateType(_)));
    }

    #[tokio::test]
    async fn recover_matches_original_reduce() {
        // End-to-end: original reduce vs recovery produce identical state.
        let store = InMemoryPendingWriteStore::new();
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        let base = json!({"test_counter": {"value": 5}});

        // Original reduce path
        let original_action =
            AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(20));
        let serialized = original_action.to_serialized_action().unwrap();
        let original_patches = reduce_state_actions(
            vec![AnyStateAction::new::<TestCounter>(
                TestCounterAction::Increment(20),
            )],
            &base,
            "original",
            &ScopeContext::run(),
        )
        .unwrap();
        let original_result = apply_patch(&base, original_patches[0].patch()).unwrap();

        // Simulate crash: persist serialized action, then recover
        store
            .write(PendingWriteEntry {
                run_id: "run_1".into(),
                thread_id: "t1".into(),
                call_id: "call_a".into(),
                tool_name: "tool".into(),
                actions: vec![serialized],
                created_at: 100,
            })
            .await
            .unwrap();

        let (recovered_patches, _) =
            recover_pending_writes("t1", "run_1", &store, &registry, &base)
                .await
                .unwrap();
        let recovered_result = apply_patch(&base, recovered_patches[0].patch()).unwrap();

        assert_eq!(original_result, recovered_result);
        assert_eq!(recovered_result["test_counter"]["value"], 25);
    }

    #[tokio::test]
    async fn read_by_thread_returns_entries_across_runs() {
        let store = InMemoryPendingWriteStore::new();

        // Write entries for two different runs on the same thread
        for (run_id, call_id) in [("run_1", "call_a"), ("run_2", "call_b")] {
            store
                .write(PendingWriteEntry {
                    run_id: run_id.into(),
                    thread_id: "t1".into(),
                    call_id: call_id.into(),
                    tool_name: "tool".into(),
                    actions: vec![],
                    created_at: 100,
                })
                .await
                .unwrap();
        }

        // Write entry for a different thread
        store
            .write(PendingWriteEntry {
                run_id: "run_3".into(),
                thread_id: "t2".into(),
                call_id: "call_c".into(),
                tool_name: "tool".into(),
                actions: vec![],
                created_at: 100,
            })
            .await
            .unwrap();

        let entries = store.read_by_thread("t1").await.unwrap();
        assert_eq!(entries.len(), 2);
        let call_ids: HashSet<String> = entries.iter().map(|e| e.call_id.clone()).collect();
        assert!(call_ids.contains("call_a"));
        assert!(call_ids.contains("call_b"));

        // Different thread should not include t1 entries
        let t2_entries = store.read_by_thread("t2").await.unwrap();
        assert_eq!(t2_entries.len(), 1);
        assert_eq!(t2_entries[0].call_id, "call_c");

        // Unknown thread returns empty
        let empty = store.read_by_thread("t_unknown").await.unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn recover_from_entries_produces_correct_state() {
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        let entries = vec![
            PendingWriteEntry {
                run_id: "run_old".into(),
                thread_id: "t1".into(),
                call_id: "call_a".into(),
                tool_name: "tool".into(),
                actions: vec![AnyStateAction::new::<TestCounter>(
                    TestCounterAction::Increment(7),
                )
                .to_serialized_action()
                .unwrap()],
                created_at: 100,
            },
            PendingWriteEntry {
                run_id: "run_old".into(),
                thread_id: "t1".into(),
                call_id: "call_b".into(),
                tool_name: "tool".into(),
                actions: vec![AnyStateAction::new::<TestCounter>(
                    TestCounterAction::Increment(3),
                )
                .to_serialized_action()
                .unwrap()],
                created_at: 200,
            },
        ];

        let base = json!({});
        let (patches, completed) =
            recover_pending_writes_from_entries(&entries, &registry, &base).unwrap();

        assert_eq!(completed.len(), 2);
        assert!(completed.contains("call_a"));
        assert!(completed.contains("call_b"));

        let mut state = base;
        for p in &patches {
            state = apply_patch(&state, p.patch()).unwrap();
        }
        assert_eq!(state["test_counter"]["value"], 10);
    }

    #[test]
    fn recover_from_entries_empty_returns_empty() {
        let registry = ActionDeserializerRegistry::new();
        let (patches, completed) =
            recover_pending_writes_from_entries(&[], &registry, &json!({})).unwrap();
        assert!(patches.is_empty());
        assert!(completed.is_empty());
    }
}
