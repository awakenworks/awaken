use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;

use crate::StateError;

use super::{MergeStrategy, Snapshot, StateKey, StateMap};

/// A type-erased state mutation operation.
pub trait MutationOp: Send {
    /// Apply this mutation to the given snapshot.
    fn apply(self: Box<Self>, state: &mut Snapshot);
}

/// Target for a typed state mutation.
pub trait MutationTarget {
    type Update: Send + 'static;

    fn apply(state: &mut Snapshot, update: Self::Update);
}

impl<K> MutationTarget for K
where
    K: StateKey,
{
    type Update = K::Update;

    fn apply(state: &mut Snapshot, update: Self::Update) {
        let value = std::sync::Arc::make_mut(&mut state.ext).get_or_insert_default::<K>();
        K::apply(value, update);
    }
}

struct KeyPatch<S: MutationTarget> {
    update: Option<S::Update>,
    _marker: PhantomData<S>,
}

impl<S> KeyPatch<S>
where
    S: MutationTarget,
{
    fn new(update: S::Update) -> Self {
        Self {
            update: Some(update),
            _marker: PhantomData,
        }
    }
}

impl<S> MutationOp for KeyPatch<S>
where
    S: MutationTarget + Send,
{
    fn apply(mut self: Box<Self>, state: &mut Snapshot) {
        let update = self.update.take().expect("key patch already applied");
        S::apply(state, update);
    }
}

struct ClearKeyMutation {
    clear: fn(&mut StateMap),
}

impl ClearKeyMutation {
    fn new(clear: fn(&mut StateMap)) -> Self {
        Self { clear }
    }
}

impl MutationOp for ClearKeyMutation {
    fn apply(self: Box<Self>, state: &mut Snapshot) {
        (self.clear)(std::sync::Arc::make_mut(&mut state.ext));
    }
}

/// A batch of state mutation operations.
///
/// Collects typed key updates and applies them atomically to a [`Snapshot`].
pub struct MutationBatch {
    pub base_revision: Option<u64>,
    pub ops: Vec<Box<dyn MutationOp>>,
    pub touched_keys: Vec<String>,
}

impl fmt::Debug for MutationBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MutationBatch")
            .field("base_revision", &self.base_revision)
            .field("ops_len", &self.ops.len())
            .field("touched_keys", &self.touched_keys)
            .finish()
    }
}

impl MutationBatch {
    pub fn new() -> Self {
        Self {
            base_revision: None,
            ops: Vec::new(),
            touched_keys: Vec::new(),
        }
    }

    pub fn with_base_revision(mut self, revision: u64) -> Self {
        self.base_revision = Some(revision);
        self
    }

    pub fn base_revision(&self) -> Option<u64> {
        self.base_revision
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn update<K>(&mut self, update: K::Update)
    where
        K: StateKey,
    {
        self.ops.push(Box::new(KeyPatch::<K>::new(update)));
        self.touched_keys.push(K::KEY.to_string());
    }

    pub fn clear_extension_with(&mut self, key: impl Into<String>, clear: fn(&mut StateMap)) {
        self.ops.push(Box::new(ClearKeyMutation::new(clear)));
        self.touched_keys.push(key.into());
    }

    pub fn extend(&mut self, mut other: Self) -> Result<(), StateError> {
        self.base_revision = match (self.base_revision, other.base_revision) {
            (Some(left), Some(right)) if left != right => {
                return Err(StateError::MutationBaseRevisionMismatch { left, right });
            }
            (Some(left), _) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        };

        self.ops.append(&mut other.ops);
        self.touched_keys.append(&mut other.touched_keys);
        Ok(())
    }

    pub fn op_len(&self) -> usize {
        self.ops.len()
    }

    /// Merge two batches produced by parallel execution.
    ///
    /// - Disjoint keys: always merged.
    /// - Overlapping keys with `Commutative` strategy: merged (order irrelevant).
    /// - Overlapping keys with `Exclusive` strategy: returns `ParallelMergeConflict`.
    pub fn merge_parallel<F>(mut self, mut other: Self, strategy: F) -> Result<Self, StateError>
    where
        F: Fn(&str) -> MergeStrategy,
    {
        // Reconcile base revisions
        self.base_revision = match (self.base_revision, other.base_revision) {
            (Some(left), Some(right)) if left != right => {
                return Err(StateError::MutationBaseRevisionMismatch { left, right });
            }
            (Some(left), _) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        };

        // Check overlapping keys
        let self_keys: HashSet<&str> = self.touched_keys.iter().map(|s| s.as_str()).collect();
        for key in &other.touched_keys {
            if self_keys.contains(key.as_str()) && strategy(key) == MergeStrategy::Exclusive {
                return Err(StateError::ParallelMergeConflict { key: key.clone() });
            }
        }

        // Merge ops and keys
        self.ops.append(&mut other.ops);
        self.touched_keys.append(&mut other.touched_keys);
        Ok(self)
    }
}

impl Default for MutationBatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter;

    impl StateKey for Counter {
        const KEY: &'static str = "counter";
        type Value = usize;
        type Update = usize;

        fn apply(value: &mut Self::Value, update: Self::Update) {
            *value += update;
        }
    }

    #[test]
    fn mutation_batch_merges_matching_base_revisions() {
        let mut left = MutationBatch::new().with_base_revision(3);
        left.update::<Counter>(1);

        let mut right = MutationBatch::new().with_base_revision(3);
        right.update::<Counter>(2);

        left.extend(right)
            .expect("matching base revisions should merge");
        assert_eq!(left.base_revision(), Some(3));
        assert_eq!(left.op_len(), 2);
    }

    #[test]
    fn mutation_batch_rejects_mismatched_base_revisions() {
        let mut left = MutationBatch::new().with_base_revision(1);
        let right = MutationBatch::new().with_base_revision(2);

        let err = left.extend(right).expect_err("mismatch should fail");
        assert!(matches!(
            err,
            StateError::MutationBaseRevisionMismatch { left: 1, right: 2 }
        ));
    }

    #[test]
    fn mutation_ops_apply_into_snapshot() {
        let mut batch = MutationBatch::new();
        batch.update::<Counter>(4);

        let mut snapshot = Snapshot {
            revision: 0,
            ext: std::sync::Arc::new(StateMap::default()),
        };

        for op in batch.ops.drain(..) {
            op.apply(&mut snapshot);
        }

        assert_eq!(snapshot.get::<Counter>().copied(), Some(4));
    }

    #[test]
    fn mutation_batch_is_empty_when_new() {
        let batch = MutationBatch::new();
        assert!(batch.is_empty());
        assert_eq!(batch.op_len(), 0);
        assert!(batch.base_revision().is_none());
    }

    #[test]
    fn mutation_batch_not_empty_after_update() {
        let mut batch = MutationBatch::new();
        batch.update::<Counter>(1);
        assert!(!batch.is_empty());
        assert_eq!(batch.op_len(), 1);
    }

    #[test]
    fn mutation_batch_parallel_merge_commutative_overlap() {
        let mut left = MutationBatch::new();
        left.update::<Counter>(10);
        let mut right = MutationBatch::new();
        right.update::<Counter>(20);

        let merged = left
            .merge_parallel(right, |_| MergeStrategy::Commutative)
            .expect("commutative overlap should merge");
        assert_eq!(merged.op_len(), 2);
    }

    #[test]
    fn mutation_batch_parallel_merge_exclusive_conflict() {
        let mut left = MutationBatch::new();
        left.update::<Counter>(10);
        let mut right = MutationBatch::new();
        right.update::<Counter>(20);

        let result = left.merge_parallel(right, |_| MergeStrategy::Exclusive);
        assert!(result.is_err());
    }
}
