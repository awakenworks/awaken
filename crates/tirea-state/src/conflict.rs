//! Touched path computation and conflict detection.
//!
//! This module provides utilities for analyzing patches to determine
//! which paths are modified and detecting conflicts between patches.

use crate::{LatticeRegistry, Op, Patch, Path};
use std::collections::BTreeSet;

/// Conflict information between two patches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The path where the conflict occurs.
    pub path: Path,
    /// The type of conflict.
    pub kind: ConflictKind,
}

/// Types of conflicts that can occur between patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both patches modify the exact same path.
    ExactMatch,
    /// One patch modifies a parent of the other's path.
    /// E.g., patch A modifies "user" while patch B modifies "user.name".
    PrefixConflict,
}

/// Compute the set of paths touched by a patch.
///
/// # Arguments
///
/// * `patch` - The patch to analyze
/// * `include_parents` - If true, include all parent paths. For example,
///   if the patch modifies "a.b.c", the result will include "a", "a.b", and "a.b.c".
///
/// # Examples
///
/// ```
/// use tirea_state::{Patch, Op, path, compute_touched};
///
/// let patch = Patch::new()
///     .with_op(Op::set(path!("user", "name"), serde_json::json!("Alice")))
///     .with_op(Op::set(path!("user", "age"), serde_json::json!(30)));
///
/// // Without parents
/// let touched = compute_touched(&patch, false);
/// assert!(touched.contains(&path!("user", "name")));
/// assert!(touched.contains(&path!("user", "age")));
/// assert!(!touched.contains(&path!("user")));
///
/// // With parents
/// let touched_with_parents = compute_touched(&patch, true);
/// assert!(touched_with_parents.contains(&path!("user")));
/// assert!(touched_with_parents.contains(&path!("user", "name")));
/// ```
pub fn compute_touched(patch: &Patch, include_parents: bool) -> BTreeSet<Path> {
    let mut touched = BTreeSet::new();

    for op in patch.ops() {
        let path = op.path();

        if include_parents {
            // Add all parent paths
            let mut current = Path::root();
            for seg in path.segments() {
                current = current.with_segment(seg.clone());
                touched.insert(current.clone());
            }
        } else {
            touched.insert(path.clone());
        }
    }

    touched
}

/// Detect conflicts between two sets of touched paths.
///
/// A conflict occurs when:
/// - Both sets contain the exact same path (ExactMatch)
/// - One path is a prefix of another (PrefixConflict)
///
/// # Examples
///
/// ```
/// use tirea_state::{path, compute_touched, detect_conflicts, Patch, Op, ConflictKind};
/// use serde_json::json;
///
/// let patch_a = Patch::new()
///     .with_op(Op::set(path!("user"), json!({"name": "Alice"})));
///
/// let patch_b = Patch::new()
///     .with_op(Op::set(path!("user", "name"), json!("Bob")));
///
/// let touched_a = compute_touched(&patch_a, false);
/// let touched_b = compute_touched(&patch_b, false);
///
/// let conflicts = detect_conflicts(&touched_a, &touched_b);
/// assert!(!conflicts.is_empty());
/// assert!(conflicts.iter().any(|c| c.kind == ConflictKind::PrefixConflict));
/// ```
pub fn detect_conflicts(a: &BTreeSet<Path>, b: &BTreeSet<Path>) -> Vec<Conflict> {
    let mut conflicts = Vec::new();

    for path_a in a {
        for path_b in b {
            if path_a == path_b {
                conflicts.push(Conflict {
                    path: path_a.clone(),
                    kind: ConflictKind::ExactMatch,
                });
            } else if path_a.is_prefix_of(path_b) || path_b.is_prefix_of(path_a) {
                // Use the shorter path (the prefix) as the conflict path
                let conflict_path = if path_a.len() < path_b.len() {
                    path_a.clone()
                } else {
                    path_b.clone()
                };
                conflicts.push(Conflict {
                    path: conflict_path,
                    kind: ConflictKind::PrefixConflict,
                });
            }
        }
    }

    conflicts
}

/// Check whether all ops in `patch` that touch `path` are `LatticeMerge`.
fn is_lattice_only_at(patch: &Patch, path: &Path) -> bool {
    patch
        .ops()
        .iter()
        .filter(|op| op.path() == path)
        .all(|op| matches!(op, Op::LatticeMerge { .. }))
}

/// Registry-aware conflict detection between two patches.
///
/// Behaves like [`PatchExt::conflicts_with`], but suppresses conflicts
/// at paths where both patches use only `LatticeMerge` ops AND the
/// path is registered in the [`LatticeRegistry`].
pub fn conflicts_with_registry(a: &Patch, b: &Patch, registry: &LatticeRegistry) -> Vec<Conflict> {
    let touched_a = compute_touched(a, false);
    let touched_b = compute_touched(b, false);
    detect_conflicts(&touched_a, &touched_b)
        .into_iter()
        .filter(|conflict| {
            !(registry.get(&conflict.path).is_some()
                && is_lattice_only_at(a, &conflict.path)
                && is_lattice_only_at(b, &conflict.path))
        })
        .collect()
}

/// Extension trait for Patch to compute touched paths.
pub trait PatchExt {
    /// Compute the paths touched by this patch.
    fn touched(&self, include_parents: bool) -> BTreeSet<Path>;

    /// Check if this patch conflicts with another patch.
    fn conflicts_with(&self, other: &Patch) -> Vec<Conflict>;
}

impl PatchExt for Patch {
    fn touched(&self, include_parents: bool) -> BTreeSet<Path> {
        compute_touched(self, include_parents)
    }

    fn conflicts_with(&self, other: &Patch) -> Vec<Conflict> {
        let touched_self = compute_touched(self, false);
        let touched_other = compute_touched(other, false);
        detect_conflicts(&touched_self, &touched_other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{path, GSet, Op};
    use serde_json::json;

    #[test]
    fn test_compute_touched_without_parents() {
        let patch = Patch::new()
            .with_op(Op::set(path!("a", "b"), json!(1)))
            .with_op(Op::set(path!("x", "y", "z"), json!(2)));

        let touched = compute_touched(&patch, false);

        assert_eq!(touched.len(), 2);
        assert!(touched.contains(&path!("a", "b")));
        assert!(touched.contains(&path!("x", "y", "z")));
        assert!(!touched.contains(&path!("a")));
    }

    #[test]
    fn test_compute_touched_with_parents() {
        let patch = Patch::new().with_op(Op::set(path!("a", "b", "c"), json!(1)));

        let touched = compute_touched(&patch, true);

        assert_eq!(touched.len(), 3);
        assert!(touched.contains(&path!("a")));
        assert!(touched.contains(&path!("a", "b")));
        assert!(touched.contains(&path!("a", "b", "c")));
    }

    #[test]
    fn test_detect_exact_conflict() {
        let mut a = BTreeSet::new();
        a.insert(path!("user", "name"));

        let mut b = BTreeSet::new();
        b.insert(path!("user", "name"));

        let conflicts = detect_conflicts(&a, &b);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::ExactMatch);
        assert_eq!(conflicts[0].path, path!("user", "name"));
    }

    #[test]
    fn test_detect_prefix_conflict() {
        let mut a = BTreeSet::new();
        a.insert(path!("user"));

        let mut b = BTreeSet::new();
        b.insert(path!("user", "name"));

        let conflicts = detect_conflicts(&a, &b);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::PrefixConflict);
        assert_eq!(conflicts[0].path, path!("user"));
    }

    #[test]
    fn test_no_conflict() {
        let mut a = BTreeSet::new();
        a.insert(path!("user", "name"));

        let mut b = BTreeSet::new();
        b.insert(path!("user", "age"));

        let conflicts = detect_conflicts(&a, &b);

        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_patch_ext_conflicts_with() {
        let patch_a = Patch::new().with_op(Op::set(path!("user"), json!({})));
        let patch_b = Patch::new().with_op(Op::set(path!("user", "name"), json!("Alice")));

        let conflicts = patch_a.conflicts_with(&patch_b);

        assert!(!conflicts.is_empty());
    }

    #[test]
    fn test_conflicts_with_registry_suppresses_lattice_only() {
        let mut registry = LatticeRegistry::new();
        registry.register::<GSet<String>>(path!("tags"));

        let patch_a = Patch::new().with_op(Op::lattice_merge(path!("tags"), json!(["a"])));
        let patch_b = Patch::new().with_op(Op::lattice_merge(path!("tags"), json!(["b"])));

        // Without registry: conflict detected
        let conflicts = patch_a.conflicts_with(&patch_b);
        assert!(!conflicts.is_empty());

        // With registry: conflict suppressed
        let conflicts = conflicts_with_registry(&patch_a, &patch_b, &registry);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_conflicts_with_registry_mixed_ops_still_conflict() {
        let mut registry = LatticeRegistry::new();
        registry.register::<GSet<String>>(path!("tags"));

        let patch_a = Patch::new()
            .with_op(Op::lattice_merge(path!("tags"), json!(["a"])))
            .with_op(Op::set(path!("tags"), json!(["override"])));
        let patch_b = Patch::new().with_op(Op::lattice_merge(path!("tags"), json!(["b"])));

        let conflicts = conflicts_with_registry(&patch_a, &patch_b, &registry);
        assert!(!conflicts.is_empty(), "mixed ops should still conflict");
    }

    #[test]
    fn test_conflicts_with_registry_unregistered_path_still_conflicts() {
        let registry = LatticeRegistry::new(); // empty registry

        let patch_a = Patch::new().with_op(Op::lattice_merge(path!("tags"), json!(["a"])));
        let patch_b = Patch::new().with_op(Op::lattice_merge(path!("tags"), json!(["b"])));

        let conflicts = conflicts_with_registry(&patch_a, &patch_b, &registry);
        assert!(
            !conflicts.is_empty(),
            "unregistered path should still conflict"
        );
    }

    #[test]
    fn test_conflicts_with_registry_disjoint_paths_no_conflict() {
        let registry = LatticeRegistry::new();

        let patch_a = Patch::new().with_op(Op::set(path!("alpha"), json!(1)));
        let patch_b = Patch::new().with_op(Op::set(path!("beta"), json!(2)));

        let conflicts = conflicts_with_registry(&patch_a, &patch_b, &registry);
        assert!(conflicts.is_empty());
    }
}
