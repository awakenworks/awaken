//! Parallel state merge for concurrent tool execution.
//!
//! When multiple tools execute in parallel, their state mutations must be merged.
//! This module provides conflict detection and merge strategies:
//!
//! - **Exclusive** keys: only one tool may modify the key (conflict = error).
//! - **Commutative** keys: multiple tools may modify the key (merges automatically).

use std::collections::{HashMap, HashSet};

use crate::state::MergeStrategy;

/// Error from parallel state merge.
#[derive(Debug, thiserror::Error)]
pub enum ParallelMergeError {
    #[error(
        "conflicting parallel state modification on exclusive key \"{key}\" by tool calls [{left_call_id}] and [{right_call_id}]"
    )]
    ExclusiveConflict {
        key: String,
        left_call_id: String,
        right_call_id: String,
    },
}

/// A batch of state modifications from a single tool call.
#[derive(Debug, Clone)]
pub struct ToolStateBatch {
    /// Tool call ID that produced this batch.
    pub call_id: String,
    /// State keys touched by this tool call.
    pub touched_keys: Vec<String>,
}

/// Validate that parallel tool state batches do not conflict on exclusive keys.
///
/// Returns `Ok(())` if all modifications are compatible, or
/// `Err(ParallelMergeError::ExclusiveConflict)` if two tools modified the same
/// exclusive key.
pub fn validate_parallel_state_batches<F>(
    batches: &[ToolStateBatch],
    strategy: F,
) -> Result<(), ParallelMergeError>
where
    F: Fn(&str) -> MergeStrategy,
{
    // Build: key -> list of call_ids that touched it
    let mut key_owners: HashMap<&str, Vec<&str>> = HashMap::new();
    for batch in batches {
        for key in &batch.touched_keys {
            key_owners
                .entry(key.as_str())
                .or_default()
                .push(&batch.call_id);
        }
    }

    // Check for conflicts on exclusive keys
    for (key, owners) in &key_owners {
        if owners.len() > 1 && strategy(key) == MergeStrategy::Exclusive {
            return Err(ParallelMergeError::ExclusiveConflict {
                key: (*key).to_string(),
                left_call_id: owners[0].to_string(),
                right_call_id: owners[1].to_string(),
            });
        }
    }

    Ok(())
}

/// Collect all unique touched keys from a set of batches.
pub fn collect_all_touched_keys(batches: &[ToolStateBatch]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for batch in batches {
        for key in &batch.touched_keys {
            if seen.insert(key.as_str()) {
                keys.push(key.clone());
            }
        }
    }
    keys
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_keys_always_merge() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["alpha".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["beta".into()],
            },
        ];

        let result = validate_parallel_state_batches(&batches, |_| MergeStrategy::Exclusive);
        assert!(result.is_ok());
    }

    #[test]
    fn overlapping_exclusive_keys_conflict() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["shared".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["shared".into()],
            },
        ];

        let err =
            validate_parallel_state_batches(&batches, |_| MergeStrategy::Exclusive).unwrap_err();
        match err {
            ParallelMergeError::ExclusiveConflict {
                key,
                left_call_id,
                right_call_id,
            } => {
                assert_eq!(key, "shared");
                assert_eq!(left_call_id, "call_1");
                assert_eq!(right_call_id, "call_2");
            }
        }
    }

    #[test]
    fn overlapping_commutative_keys_merge() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["counter".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["counter".into()],
            },
        ];

        let result = validate_parallel_state_batches(&batches, |_| MergeStrategy::Commutative);
        assert!(result.is_ok());
    }

    #[test]
    fn mixed_strategies() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["exclusive_key".into(), "commutative_key".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["commutative_key".into()],
            },
        ];

        let result = validate_parallel_state_batches(&batches, |key| {
            if key == "commutative_key" {
                MergeStrategy::Commutative
            } else {
                MergeStrategy::Exclusive
            }
        });
        assert!(result.is_ok());
    }

    #[test]
    fn mixed_strategies_with_exclusive_conflict() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["exclusive_key".into(), "commutative_key".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["commutative_key".into(), "exclusive_key".into()],
            },
        ];

        let err = validate_parallel_state_batches(&batches, |key| {
            if key == "commutative_key" {
                MergeStrategy::Commutative
            } else {
                MergeStrategy::Exclusive
            }
        })
        .unwrap_err();
        assert!(matches!(
            err,
            ParallelMergeError::ExclusiveConflict { ref key, .. } if key == "exclusive_key"
        ));
    }

    #[test]
    fn empty_batches_always_ok() {
        let result: Result<(), ParallelMergeError> =
            validate_parallel_state_batches(&[], |_| MergeStrategy::Exclusive);
        assert!(result.is_ok());
    }

    #[test]
    fn single_batch_always_ok() {
        let batches = vec![ToolStateBatch {
            call_id: "call_1".into(),
            touched_keys: vec!["any_key".into()],
        }];
        let result = validate_parallel_state_batches(&batches, |_| MergeStrategy::Exclusive);
        assert!(result.is_ok());
    }

    #[test]
    fn collect_keys_deduplicates() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["a".into(), "b".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["b".into(), "c".into()],
            },
        ];
        let keys = collect_all_touched_keys(&batches);
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"a".to_string()));
        assert!(keys.contains(&"b".to_string()));
        assert!(keys.contains(&"c".to_string()));
    }

    #[test]
    fn three_way_exclusive_conflict() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["shared".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["shared".into()],
            },
            ToolStateBatch {
                call_id: "call_3".into(),
                touched_keys: vec!["shared".into()],
            },
        ];

        let err =
            validate_parallel_state_batches(&batches, |_| MergeStrategy::Exclusive).unwrap_err();
        assert!(matches!(err, ParallelMergeError::ExclusiveConflict { .. }));
    }

    #[test]
    fn error_display() {
        let err = ParallelMergeError::ExclusiveConflict {
            key: "my_key".into(),
            left_call_id: "a".into(),
            right_call_id: "b".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("my_key"));
        assert!(msg.contains("a"));
        assert!(msg.contains("b"));
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional parallel merge tests
    // -----------------------------------------------------------------------

    #[test]
    fn many_batches_disjoint() {
        let batches: Vec<ToolStateBatch> = (0..10)
            .map(|i| ToolStateBatch {
                call_id: format!("call_{i}"),
                touched_keys: vec![format!("key_{i}")],
            })
            .collect();
        let result = validate_parallel_state_batches(&batches, |_| MergeStrategy::Exclusive);
        assert!(result.is_ok());
    }

    #[test]
    fn batch_with_multiple_keys_one_conflicting() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["a".into(), "b".into(), "c".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["d".into(), "b".into(), "e".into()],
            },
        ];

        // "b" is shared, exclusive => conflict
        let err =
            validate_parallel_state_batches(&batches, |_| MergeStrategy::Exclusive).unwrap_err();
        match err {
            ParallelMergeError::ExclusiveConflict { key, .. } => {
                assert_eq!(key, "b");
            }
        }
    }

    #[test]
    fn per_key_strategy_allows_mixed() {
        let batches = vec![
            ToolStateBatch {
                call_id: "call_1".into(),
                touched_keys: vec!["counter".into(), "config".into()],
            },
            ToolStateBatch {
                call_id: "call_2".into(),
                touched_keys: vec!["counter".into()],
            },
        ];

        // counter is commutative, config is exclusive but not shared => OK
        let result = validate_parallel_state_batches(&batches, |key| {
            if key == "counter" {
                MergeStrategy::Commutative
            } else {
                MergeStrategy::Exclusive
            }
        });
        assert!(result.is_ok());
    }

    #[test]
    fn collect_keys_empty_batches() {
        let keys = collect_all_touched_keys(&[]);
        assert!(keys.is_empty());
    }

    #[test]
    fn collect_keys_preserves_first_occurrence_order() {
        let batches = vec![
            ToolStateBatch {
                call_id: "c1".into(),
                touched_keys: vec!["z".into(), "a".into()],
            },
            ToolStateBatch {
                call_id: "c2".into(),
                touched_keys: vec!["m".into(), "a".into()],
            },
        ];
        let keys = collect_all_touched_keys(&batches);
        // z, a, m — "a" only appears once
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], "z");
        assert_eq!(keys[1], "a");
        assert_eq!(keys[2], "m");
    }

    #[test]
    fn single_batch_with_many_keys() {
        let batch = ToolStateBatch {
            call_id: "c1".into(),
            touched_keys: (0..20).map(|i| format!("key_{i}")).collect(),
        };
        let result = validate_parallel_state_batches(&[batch], |_| MergeStrategy::Exclusive);
        assert!(result.is_ok(), "single batch never conflicts with itself");
    }

    #[test]
    fn commutative_three_way_overlap() {
        let batches = vec![
            ToolStateBatch {
                call_id: "c1".into(),
                touched_keys: vec!["shared".into()],
            },
            ToolStateBatch {
                call_id: "c2".into(),
                touched_keys: vec!["shared".into()],
            },
            ToolStateBatch {
                call_id: "c3".into(),
                touched_keys: vec!["shared".into()],
            },
        ];
        let result = validate_parallel_state_batches(&batches, |_| MergeStrategy::Commutative);
        assert!(result.is_ok(), "commutative allows any number of writers");
    }

    #[test]
    fn collect_keys_single_batch() {
        let batches = vec![ToolStateBatch {
            call_id: "c1".into(),
            touched_keys: vec!["a".into(), "b".into()],
        }];
        let keys = collect_all_touched_keys(&batches);
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn exclusive_conflict_first_pair_reported() {
        let batches = vec![
            ToolStateBatch {
                call_id: "c1".into(),
                touched_keys: vec!["shared".into()],
            },
            ToolStateBatch {
                call_id: "c2".into(),
                touched_keys: vec!["shared".into()],
            },
            ToolStateBatch {
                call_id: "c3".into(),
                touched_keys: vec!["shared".into()],
            },
        ];
        let err =
            validate_parallel_state_batches(&batches, |_| MergeStrategy::Exclusive).unwrap_err();
        match err {
            ParallelMergeError::ExclusiveConflict {
                left_call_id,
                right_call_id,
                ..
            } => {
                assert_eq!(left_call_id, "c1");
                assert_eq!(right_call_id, "c2");
            }
        }
    }
}
