//! StateManager manages immutable state with patch history.
//!
//! All state changes are applied through patches, enabling:
//! - Full traceability of changes
//! - State replay to any point in history
//! - Batch application with conflict detection

use crate::apply::apply_patch_in_place_with_registry;
use crate::{
    detect_conflicts, Conflict, ConflictKind, LatticeRegistry, Op, Patch, PatchExt, Path,
    TireaError, TrackedPatch,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

use crate::Lattice;

/// Errors that can occur during state management.
#[derive(Debug, Error)]
pub enum StateError {
    /// Failed to apply a patch operation.
    #[error("Failed to apply patch: {0}")]
    ApplyFailed(#[from] TireaError),

    /// Replay index is outside the available history range.
    #[error("Invalid replay index: {index}, history length: {len}")]
    InvalidReplayIndex {
        /// Requested replay index.
        index: usize,
        /// Total number of patches in history.
        len: usize,
    },

    /// Batch commit contains conflicting patches.
    #[error("conflicting patch in batch between index {left} and {right} at {path} ({kind:?})")]
    BatchConflict {
        /// Left patch index in the batch.
        left: usize,
        /// Right patch index in the batch.
        right: usize,
        /// Path where conflict was detected.
        path: Path,
        /// Conflict classification.
        kind: ConflictKind,
    },
}

/// Result of applying patches.
#[derive(Debug, Clone)]
pub struct ApplyResult {
    /// Number of patches applied.
    pub patches_applied: usize,
    /// Number of operations applied.
    pub ops_applied: usize,
}

/// StateManager manages immutable state with patch history.
///
/// # Design
///
/// - State is immutable - all changes go through commits
/// - Full history is maintained for replay
/// - Supports batch commits and preview operations
///
/// # API Philosophy
///
/// - `commit`/`commit_batch`: Mutating operations that modify state and record history
/// - `preview_patch`: Pure operation that computes result without modifying state
/// - `replay_to`: Pure operation that reconstructs historical state
///
/// # Example
///
/// ```ignore
/// use tirea_state::{StateManager, StateContext};
/// use serde_json::json;
///
/// let manager = StateManager::new(json!({}));
///
/// // Get snapshot and create state context
/// let snapshot = manager.snapshot().await;
/// let ctx = StateContext::new(&snapshot);
///
/// // ... modify state through ctx ...
///
/// // Commit patch (modifies state and records history)
/// manager.commit(ctx.take_tracked_patch("tool:example")).await?;
///
/// // Replay to a specific point
/// let old_state = manager.replay_to(5).await?;
/// ```
pub struct StateManager {
    initial: Arc<RwLock<Value>>,
    state: Arc<RwLock<Value>>,
    history: Arc<RwLock<Vec<TrackedPatch>>>,
    registry: Arc<RwLock<LatticeRegistry>>,
}

impl StateManager {
    /// Create a new StateManager with initial state.
    pub fn new(initial: Value) -> Self {
        Self {
            initial: Arc::new(RwLock::new(initial.clone())),
            state: Arc::new(RwLock::new(initial)),
            history: Arc::new(RwLock::new(Vec::new())),
            registry: Arc::new(RwLock::new(LatticeRegistry::new())),
        }
    }

    /// Register a lattice type at the given path for automatic merge during apply.
    pub async fn register_lattice<T>(&self, path: impl Into<Path>)
    where
        T: Lattice + Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        self.registry.write().await.register::<T>(path);
    }

    /// Get a snapshot of the current state.
    pub async fn snapshot(&self) -> Value {
        self.state.read().await.clone()
    }

    /// Commit a single patch, modifying state and recording to history.
    ///
    /// This is a mutating operation that:
    /// 1. Applies the patch to current state
    /// 2. Updates the internal state
    /// 3. Records the patch in history for replay
    pub async fn commit(&self, patch: TrackedPatch) -> Result<ApplyResult, StateError> {
        let ops_count = patch.patch().len();

        let registry = self.registry.read().await;
        let mut state = self.state.write().await;
        let mut history = self.history.write().await;
        let mut new_state = state.clone();
        apply_patch_in_place_with_registry(&mut new_state, patch.patch(), &registry)?;
        *state = new_state;
        history.push(patch);

        Ok(ApplyResult {
            patches_applied: 1,
            ops_applied: ops_count,
        })
    }

    /// Commit multiple patches in batch.
    ///
    /// Patches are applied in order atomically. If any patch fails, no changes
    /// are persisted to state or history.
    pub async fn commit_batch(
        &self,
        patches: Vec<TrackedPatch>,
    ) -> Result<ApplyResult, StateError> {
        if patches.is_empty() {
            return Ok(ApplyResult {
                patches_applied: 0,
                ops_applied: 0,
            });
        }

        let registry = self.registry.read().await;

        if let Some((left, right, conflict)) = first_batch_conflict(&patches, &registry) {
            return Err(StateError::BatchConflict {
                left,
                right,
                path: conflict.path,
                kind: conflict.kind,
            });
        }

        let mut total_ops = 0;
        let mut state = self.state.write().await;
        let mut history = self.history.write().await;
        let mut new_state = state.clone();

        for patch in &patches {
            total_ops += patch.patch().len();
            apply_patch_in_place_with_registry(&mut new_state, patch.patch(), &registry)?;
        }
        *state = new_state;
        let patches_count = patches.len();
        history.extend(patches);

        Ok(ApplyResult {
            patches_applied: patches_count,
            ops_applied: total_ops,
        })
    }

    /// Preview applying a patch without modifying state (pure operation).
    ///
    /// This computes what the state would look like after applying the patch,
    /// but does not modify the actual state or record to history.
    ///
    /// Useful for validation, dry-runs, or showing users what changes would occur.
    pub async fn preview_patch(&self, patch: &Patch) -> Result<Value, StateError> {
        let registry = self.registry.read().await;
        let state = self.state.read().await;
        let mut preview = state.clone();
        apply_patch_in_place_with_registry(&mut preview, patch, &registry)?;
        Ok(preview)
    }

    /// Deprecated: Use `commit` instead.
    #[deprecated(since = "0.2.0", note = "Use `commit` instead")]
    pub async fn apply(&self, patch: TrackedPatch) -> Result<ApplyResult, StateError> {
        self.commit(patch).await
    }

    /// Deprecated: Use `commit_batch` instead.
    #[deprecated(since = "0.2.0", note = "Use `commit_batch` instead")]
    pub async fn apply_batch(&self, patches: Vec<TrackedPatch>) -> Result<ApplyResult, StateError> {
        self.commit_batch(patches).await
    }

    /// Replay state from the beginning up to (and including) the specified index.
    ///
    /// Returns the state as it was after applying patches [0..=index] to the initial state.
    pub async fn replay_to(&self, index: usize) -> Result<Value, StateError> {
        let registry = self.registry.read().await;
        let history = self.history.read().await;

        if index >= history.len() {
            return Err(StateError::InvalidReplayIndex {
                index,
                len: history.len(),
            });
        }

        let mut state = self.initial.read().await.clone();
        for patch in history.iter().take(index + 1) {
            apply_patch_in_place_with_registry(&mut state, patch.patch(), &registry)?;
        }

        Ok(state)
    }

    /// Get the full patch history.
    pub async fn history(&self) -> Vec<TrackedPatch> {
        self.history.read().await.clone()
    }

    /// Get the number of patches in history.
    pub async fn history_len(&self) -> usize {
        self.history.read().await.len()
    }

    /// Clear history (keeps current state).
    ///
    /// Use with caution - this removes the ability to replay.
    pub async fn clear_history(&self) {
        self.history.write().await.clear();
    }

    /// Prune history, keeping only the last `keep_last` patches.
    ///
    /// This is useful for long-running systems to prevent unbounded memory growth.
    /// The initial state is updated to the state before the remaining patches,
    /// so `replay_to` will continue to work correctly with the remaining patches.
    ///
    /// # Arguments
    ///
    /// - `keep_last`: Number of recent patches to keep. If 0, all patches are removed.
    ///
    /// # Returns
    ///
    /// The number of patches that were removed.
    pub async fn prune_history(&self, keep_last: usize) -> Result<usize, StateError> {
        let registry = self.registry.read().await;
        let mut history = self.history.write().await;
        let len = history.len();

        if len <= keep_last {
            return Ok(0);
        }

        let to_remove = len - keep_last;

        // Compute the new initial state by applying the patches to be removed
        let mut new_initial = self.initial.read().await.clone();
        for patch in history.iter().take(to_remove) {
            apply_patch_in_place_with_registry(&mut new_initial, patch.patch(), &registry)?;
        }

        // Update initial state and remove old patches
        *self.initial.write().await = new_initial;
        history.drain(0..to_remove);

        Ok(to_remove)
    }

    /// Get a snapshot of the initial state.
    pub async fn initial(&self) -> Value {
        self.initial.read().await.clone()
    }
}

/// Check whether all ops in `patch` that touch `path` are `LatticeMerge`.
fn is_lattice_only_at(patch: &Patch, path: &Path) -> bool {
    patch
        .ops()
        .iter()
        .filter(|op| op.path() == path)
        .all(|op| matches!(op, Op::LatticeMerge { .. }))
}

fn first_batch_conflict(
    patches: &[TrackedPatch],
    registry: &LatticeRegistry,
) -> Option<(usize, usize, Conflict)> {
    let touched: Vec<_> = patches
        .iter()
        .map(|patch| patch.patch().touched(false))
        .collect();

    for left_idx in 0..patches.len() {
        for right_idx in (left_idx + 1)..patches.len() {
            for conflict in detect_conflicts(&touched[left_idx], &touched[right_idx]) {
                // Skip conflicts where both patches only use LatticeMerge at the
                // conflicting path AND the path is registered in the registry.
                if registry.get(&conflict.path).is_some()
                    && is_lattice_only_at(patches[left_idx].patch(), &conflict.path)
                    && is_lattice_only_at(patches[right_idx].patch(), &conflict.path)
                {
                    continue;
                }
                return Some((left_idx, right_idx, conflict));
            }
        }
    }

    None
}

impl Clone for StateManager {
    fn clone(&self) -> Self {
        Self {
            initial: Arc::clone(&self.initial),
            state: Arc::clone(&self.state),
            history: Arc::clone(&self.history),
            registry: Arc::clone(&self.registry),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{path, Op, Patch};
    use serde_json::json;

    fn make_patch(ops: Vec<Op>, source: &str) -> TrackedPatch {
        TrackedPatch::new(Patch::with_ops(ops)).with_source(source)
    }

    #[tokio::test]
    async fn test_new_and_snapshot() {
        let initial = json!({"count": 0});
        let manager = StateManager::new(initial.clone());
        let snapshot = manager.snapshot().await;
        assert_eq!(snapshot, initial);
    }

    #[tokio::test]
    async fn test_commit_single() {
        let manager = StateManager::new(json!({}));

        let patch = make_patch(vec![Op::set(path!("count"), json!(10))], "test");

        let result = manager.commit(patch).await.unwrap();
        assert_eq!(result.patches_applied, 1);
        assert_eq!(result.ops_applied, 1);

        let state = manager.snapshot().await;
        assert_eq!(state["count"], 10);
    }

    #[tokio::test]
    async fn test_commit_batch() {
        let manager = StateManager::new(json!({}));

        let patches = vec![
            make_patch(vec![Op::set(path!("a"), json!(1))], "test1"),
            make_patch(vec![Op::set(path!("b"), json!(2))], "test2"),
        ];

        let result = manager.commit_batch(patches).await.unwrap();
        assert_eq!(result.patches_applied, 2);
        assert_eq!(result.ops_applied, 2);

        let state = manager.snapshot().await;
        assert_eq!(state["a"], 1);
        assert_eq!(state["b"], 2);
    }

    #[tokio::test]
    async fn test_commit_batch_rejects_exact_conflict() {
        let manager = StateManager::new(json!({"stable": true}));

        let patches = vec![
            make_patch(vec![Op::set(path!("x"), json!(1))], "left"),
            make_patch(vec![Op::set(path!("x"), json!(2))], "right"),
        ];

        let err = manager.commit_batch(patches).await.unwrap_err();
        assert!(err.to_string().contains("conflicting patch"));

        // Batch is atomic: state/history remain unchanged on conflict.
        let state = manager.snapshot().await;
        assert_eq!(state, json!({"stable": true}));
        assert_eq!(manager.history_len().await, 0);
    }

    #[tokio::test]
    async fn test_commit_batch_rejects_prefix_conflict() {
        let manager = StateManager::new(json!({"stable": true}));

        let patches = vec![
            make_patch(vec![Op::set(path!("user"), json!({"name": "A"}))], "left"),
            make_patch(vec![Op::set(path!("user", "name"), json!("B"))], "right"),
        ];

        let err = manager.commit_batch(patches).await.unwrap_err();
        assert!(err.to_string().contains("conflicting patch"));

        // Batch is atomic: state/history remain unchanged on conflict.
        let state = manager.snapshot().await;
        assert_eq!(state, json!({"stable": true}));
        assert_eq!(manager.history_len().await, 0);
    }

    #[tokio::test]
    async fn test_history() {
        let manager = StateManager::new(json!({}));

        manager
            .commit(make_patch(vec![Op::set(path!("x"), json!(1))], "s1"))
            .await
            .unwrap();

        manager
            .commit(make_patch(vec![Op::set(path!("y"), json!(2))], "s2"))
            .await
            .unwrap();

        let history = manager.history().await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].source.as_deref(), Some("s1"));
        assert_eq!(history[1].source.as_deref(), Some("s2"));
    }

    #[tokio::test]
    async fn test_replay_to() {
        let manager = StateManager::new(json!({}));

        manager
            .commit(make_patch(vec![Op::set(path!("count"), json!(1))], "s1"))
            .await
            .unwrap();

        manager
            .commit(make_patch(vec![Op::set(path!("count"), json!(2))], "s2"))
            .await
            .unwrap();

        manager
            .commit(make_patch(vec![Op::set(path!("count"), json!(3))], "s3"))
            .await
            .unwrap();

        // Replay to index 0
        let state0 = manager.replay_to(0).await.unwrap();
        assert_eq!(state0["count"], 1);

        // Replay to index 1
        let state1 = manager.replay_to(1).await.unwrap();
        assert_eq!(state1["count"], 2);

        // Current state unchanged
        let current = manager.snapshot().await;
        assert_eq!(current["count"], 3);
    }

    #[tokio::test]
    async fn test_replay_invalid_index() {
        let manager = StateManager::new(json!({}));
        let result = manager.replay_to(0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_clear_history() {
        let manager = StateManager::new(json!({}));

        manager
            .commit(make_patch(vec![Op::set(path!("x"), json!(1))], "s1"))
            .await
            .unwrap();

        assert_eq!(manager.history_len().await, 1);

        manager.clear_history().await;

        assert_eq!(manager.history_len().await, 0);

        // State should be preserved
        let state = manager.snapshot().await;
        assert_eq!(state["x"], 1);
    }

    #[tokio::test]
    async fn test_clone_shares_state() {
        let manager1 = StateManager::new(json!({}));
        let manager2 = manager1.clone();

        manager1
            .commit(make_patch(vec![Op::set(path!("x"), json!(42))], "s1"))
            .await
            .unwrap();

        let state = manager2.snapshot().await;
        assert_eq!(state["x"], 42);
    }

    #[tokio::test]
    async fn test_replay_to_preserves_initial_state() {
        // Create manager with non-empty initial state
        let initial = json!({"base_value": 100, "name": "test"});
        let manager = StateManager::new(initial.clone());

        // Verify initial() returns the initial state
        assert_eq!(manager.initial().await, initial);

        // Apply patches that modify existing and add new fields
        manager
            .commit(make_patch(vec![Op::set(path!("count"), json!(1))], "s1"))
            .await
            .unwrap();

        manager
            .commit(make_patch(vec![Op::set(path!("count"), json!(2))], "s2"))
            .await
            .unwrap();

        // Replay to index 0 should have initial state + first patch
        let state0 = manager.replay_to(0).await.unwrap();
        assert_eq!(state0["base_value"], 100); // Initial value preserved
        assert_eq!(state0["name"], "test"); // Initial value preserved
        assert_eq!(state0["count"], 1); // First patch applied

        // Replay to index 1 should have initial state + both patches
        let state1 = manager.replay_to(1).await.unwrap();
        assert_eq!(state1["base_value"], 100); // Initial value preserved
        assert_eq!(state1["name"], "test"); // Initial value preserved
        assert_eq!(state1["count"], 2); // Second patch applied
    }

    #[tokio::test]
    async fn test_replay_to_with_overwrite() {
        // Initial state with a value that will be overwritten
        let initial = json!({"count": 0});
        let manager = StateManager::new(initial);

        // Patch overwrites the initial count
        manager
            .commit(make_patch(vec![Op::set(path!("count"), json!(10))], "s1"))
            .await
            .unwrap();

        // Replay should show initial count overwritten
        let state = manager.replay_to(0).await.unwrap();
        assert_eq!(state["count"], 10);
    }

    #[tokio::test]
    async fn test_prune_history_basic() {
        let manager = StateManager::new(json!({"base": 0}));

        // Add 5 patches
        for i in 1..=5 {
            manager
                .commit(make_patch(
                    vec![Op::set(path!("count"), json!(i))],
                    &format!("s{}", i),
                ))
                .await
                .unwrap();
        }

        assert_eq!(manager.history_len().await, 5);

        // Keep last 2 patches
        let removed = manager.prune_history(2).await.unwrap();
        assert_eq!(removed, 3);
        assert_eq!(manager.history_len().await, 2);

        // Current state unchanged
        let current = manager.snapshot().await;
        assert_eq!(current["count"], 5);
        assert_eq!(current["base"], 0);
    }

    #[tokio::test]
    async fn test_prune_history_updates_initial() {
        let manager = StateManager::new(json!({"base": 0}));

        // Add 3 patches
        manager
            .commit(make_patch(vec![Op::set(path!("a"), json!(1))], "s1"))
            .await
            .unwrap();
        manager
            .commit(make_patch(vec![Op::set(path!("b"), json!(2))], "s2"))
            .await
            .unwrap();
        manager
            .commit(make_patch(vec![Op::set(path!("c"), json!(3))], "s3"))
            .await
            .unwrap();

        // Keep last 1 patch (remove first 2)
        manager.prune_history(1).await.unwrap();

        // Initial state should now include patches s1 and s2
        let initial = manager.initial().await;
        assert_eq!(initial["base"], 0);
        assert_eq!(initial["a"], 1);
        assert_eq!(initial["b"], 2);
        assert!(initial.get("c").is_none()); // Not in initial, only in remaining patch

        // Replay index 0 should give us state after s3
        let state = manager.replay_to(0).await.unwrap();
        assert_eq!(state["a"], 1);
        assert_eq!(state["b"], 2);
        assert_eq!(state["c"], 3);
    }

    #[tokio::test]
    async fn test_prune_history_keep_all() {
        let manager = StateManager::new(json!({}));

        manager
            .commit(make_patch(vec![Op::set(path!("x"), json!(1))], "s1"))
            .await
            .unwrap();

        // Keep more than we have
        let removed = manager.prune_history(10).await.unwrap();
        assert_eq!(removed, 0);
        assert_eq!(manager.history_len().await, 1);
    }

    #[tokio::test]
    async fn test_prune_history_keep_zero() {
        let manager = StateManager::new(json!({"base": 0}));

        manager
            .commit(make_patch(vec![Op::set(path!("x"), json!(1))], "s1"))
            .await
            .unwrap();
        manager
            .commit(make_patch(vec![Op::set(path!("y"), json!(2))], "s2"))
            .await
            .unwrap();

        // Keep 0 - remove all history
        let removed = manager.prune_history(0).await.unwrap();
        assert_eq!(removed, 2);
        assert_eq!(manager.history_len().await, 0);

        // Initial should now be current state
        let initial = manager.initial().await;
        assert_eq!(initial["base"], 0);
        assert_eq!(initial["x"], 1);
        assert_eq!(initial["y"], 2);
    }
}
