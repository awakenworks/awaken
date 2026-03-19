use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Type-keyed extension map for [`StepContext`](super::phase::StepContext).
///
/// Each slot is keyed by `TypeId` and holds one value of that type.
/// Plugins insert domain-specific state (e.g. `InferenceContext`, `ToolGate`)
/// via the [`Action::apply`](super::Action::apply) method; the loop reads
/// them back after a phase completes.
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Extensions {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Get a shared reference to the value of type `T`.
    pub fn get<T: 'static + Send + Sync>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<T>())
    }

    /// Get a mutable reference to the value of type `T`.
    pub fn get_mut<T: 'static + Send + Sync>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_mut::<T>())
    }

    /// Get a mutable reference, inserting `T::default()` if absent.
    pub fn get_or_default<T: 'static + Send + Sync + Default>(&mut self) -> &mut T {
        self.map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut::<T>()
            .expect("type mismatch in Extensions (impossible)")
    }

    /// Insert a value, returning any previous value of the same type.
    pub fn insert<T: 'static + Send + Sync>(&mut self, val: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(val))
            .and_then(|prev| prev.downcast::<T>().ok())
            .map(|boxed| *boxed)
    }

    /// Remove all entries.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Check if the map contains a value of type `T`.
    pub fn contains<T: 'static + Send + Sync>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }
}

impl Default for Extensions {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let mut ext = Extensions::new();
        ext.insert(42u32);
        assert_eq!(ext.get::<u32>(), Some(&42));
    }

    #[test]
    fn get_mut() {
        let mut ext = Extensions::new();
        ext.insert(String::from("hello"));
        ext.get_mut::<String>().unwrap().push_str(" world");
        assert_eq!(ext.get::<String>().unwrap(), "hello world");
    }

    #[test]
    fn get_or_default() {
        let mut ext = Extensions::new();
        let val: &mut Vec<i32> = ext.get_or_default();
        val.push(1);
        assert_eq!(ext.get::<Vec<i32>>().unwrap(), &vec![1]);
    }

    #[test]
    fn insert_replaces() {
        let mut ext = Extensions::new();
        ext.insert(1u32);
        let prev = ext.insert(2u32);
        assert_eq!(prev, Some(1));
        assert_eq!(ext.get::<u32>(), Some(&2));
    }

    #[test]
    fn clear_removes_all() {
        let mut ext = Extensions::new();
        ext.insert(1u32);
        ext.insert(String::from("x"));
        ext.clear();
        assert!(!ext.contains::<u32>());
        assert!(!ext.contains::<String>());
    }

    #[test]
    fn different_types_coexist() {
        let mut ext = Extensions::new();
        ext.insert(42u32);
        ext.insert(String::from("hello"));
        ext.insert(true);
        assert_eq!(ext.get::<u32>(), Some(&42));
        assert_eq!(ext.get::<String>().unwrap(), "hello");
        assert_eq!(ext.get::<bool>(), Some(&true));
    }

    #[test]
    fn missing_type_returns_none() {
        let ext = Extensions::new();
        assert!(ext.get::<u32>().is_none());
    }
}
