//! Type-erased lattice merge registry.
//!
//! `LatticeRegistry` maps paths to type-erased merge functions, enabling
//! `apply_patch_with_registry` to perform proper lattice merges at runtime.

use crate::{Lattice, Path, TireaError, TireaResult};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

/// Type-erased lattice merge function.
pub trait LatticeMerger: Send + Sync {
    /// Merge a delta into the current value.
    ///
    /// If `current` is `None` (field missing), the delta is returned directly.
    /// Otherwise, deserializes both, calls `Lattice::merge`, and serializes the result.
    fn merge(&self, current: Option<&Value>, delta: &Value) -> TireaResult<Value>;
}

/// Blanket `LatticeMerger` implementation for any `Lattice + Serialize + DeserializeOwned` type.
struct LatticeAdapter<T>(PhantomData<T>);

impl<T> LatticeMerger for LatticeAdapter<T>
where
    T: Lattice + Serialize + DeserializeOwned + Send + Sync,
{
    fn merge(&self, current: Option<&Value>, delta: &Value) -> TireaResult<Value> {
        let d: T = serde_json::from_value(delta.clone()).map_err(TireaError::from)?;
        let merged = match current {
            None => d,
            Some(v) => {
                let c: T = serde_json::from_value(v.clone()).map_err(TireaError::from)?;
                Lattice::merge(&c, &d)
            }
        };
        serde_json::to_value(&merged).map_err(TireaError::from)
    }
}

/// Registry that maps paths to type-erased lattice merge functions.
pub struct LatticeRegistry {
    entries: HashMap<Path, Arc<dyn LatticeMerger>>,
}

impl LatticeRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a lattice type at the given path.
    pub fn register<T>(&mut self, path: impl Into<Path>)
    where
        T: Lattice + Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        self.entries
            .insert(path.into(), Arc::new(LatticeAdapter::<T>(PhantomData)));
    }

    /// Look up the merger for a path.
    pub fn get(&self, path: &Path) -> Option<&dyn LatticeMerger> {
        self.entries.get(path).map(|arc| arc.as_ref())
    }
}

impl Default for LatticeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{path, GCounter};

    #[test]
    fn test_register_and_merge() {
        let mut registry = LatticeRegistry::new();
        registry.register::<GCounter>(path!("counter"));

        let merger = registry.get(&path!("counter")).unwrap();

        // Merge into None (missing field)
        let delta = serde_json::to_value(GCounter::new()).unwrap();
        let result = merger.merge(None, &delta).unwrap();
        assert_eq!(result, delta);
    }

    #[test]
    fn test_merge_two_counters() {
        let mut registry = LatticeRegistry::new();
        registry.register::<GCounter>(path!("counter"));

        let mut c1 = GCounter::new();
        c1.increment("n1", 5);
        let mut c2 = GCounter::new();
        c2.increment("n2", 3);

        let v1 = serde_json::to_value(&c1).unwrap();
        let v2 = serde_json::to_value(&c2).unwrap();

        let merger = registry.get(&path!("counter")).unwrap();
        let merged = merger.merge(Some(&v1), &v2).unwrap();

        let result: GCounter = serde_json::from_value(merged).unwrap();
        assert_eq!(result.value(), 8);
    }

    #[test]
    fn test_unregistered_path_returns_none() {
        let registry = LatticeRegistry::new();
        assert!(registry.get(&path!("missing")).is_none());
    }
}
