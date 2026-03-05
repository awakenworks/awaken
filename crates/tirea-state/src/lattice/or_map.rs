use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::Lattice;

/// Internal entry pairing a value with its insertion timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry<V> {
    value: V,
    timestamp: u64,
}

/// An observed-remove map (OR-Map) with put-wins semantics and recursive value merge.
///
/// Each key maps to an [`Entry`] containing a value `V: Lattice` and an insertion timestamp.
/// Removals record a tombstone timestamp. A key is considered present when its entry timestamp
/// is greater than or equal to its tombstone timestamp (put-wins: concurrent put and
/// remove resolve in favor of put).
///
/// When both replicas have a present entry for the same key, the values are merged using
/// the `V::merge` lattice operation, providing recursive conflict resolution.
///
/// The internal clock advances automatically on mutation and is bumped to
/// `max(self, other)` on merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ORMap<K: Ord, V: Lattice> {
    entries: BTreeMap<K, Entry<V>>,
    tombstones: BTreeMap<K, u64>,
    #[serde(skip)]
    clock: u64,
}

impl<'de, K, V> Deserialize<'de> for ORMap<K, V>
where
    K: Ord + DeserializeOwned,
    V: Lattice + DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw<K: Ord, V> {
            entries: BTreeMap<K, Entry<V>>,
            tombstones: BTreeMap<K, u64>,
        }

        let raw = Raw::<K, V>::deserialize(deserializer)?;
        let max_entry = raw.entries.values().map(|e| e.timestamp).max().unwrap_or(0);
        let max_tomb = raw.tombstones.values().copied().max().unwrap_or(0);
        let clock = max_entry.max(max_tomb);

        Ok(Self {
            entries: raw.entries,
            tombstones: raw.tombstones,
            clock,
        })
    }
}

impl<K: Ord, V: Lattice> ORMap<K, V> {
    /// Create an empty OR-Map.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            clock: 0,
        }
    }

    /// Put a key-value pair. Overwrites any existing value for the key.
    pub fn put(&mut self, key: K, value: V) {
        self.clock += 1;
        self.entries.insert(
            key,
            Entry {
                value,
                timestamp: self.clock,
            },
        );
    }

    /// Remove a key by recording a tombstone at the current clock.
    pub fn remove(&mut self, key: &K)
    where
        K: Clone,
    {
        if self.entries.contains_key(key) {
            self.clock += 1;
            self.tombstones.insert(key.clone(), self.clock);
        }
    }

    /// Returns a reference to the value for the key, if present.
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).and_then(|entry| {
            let tomb_ts = self.tombstones.get(key).copied().unwrap_or(0);
            if entry.timestamp >= tomb_ts {
                Some(&entry.value)
            } else {
                None
            }
        })
    }

    /// Returns `true` if the key is present.
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Returns a sorted vector of references to all present keys.
    pub fn keys(&self) -> Vec<&K> {
        self.entries
            .iter()
            .filter(|(k, entry)| {
                let tomb_ts = self.tombstones.get(*k).copied().unwrap_or(0);
                entry.timestamp >= tomb_ts
            })
            .map(|(k, _)| k)
            .collect()
    }

    /// Returns a sorted vector of `(key, value)` pairs for all present entries.
    pub fn entries(&self) -> Vec<(&K, &V)> {
        self.entries
            .iter()
            .filter(|(k, entry)| {
                let tomb_ts = self.tombstones.get(*k).copied().unwrap_or(0);
                entry.timestamp >= tomb_ts
            })
            .map(|(k, entry)| (k, &entry.value))
            .collect()
    }

    /// Returns the number of present entries.
    pub fn len(&self) -> usize {
        self.keys().len()
    }

    /// Returns `true` if no entries are present.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn max_observed_ts(&self) -> u64 {
        let max_entry = self
            .entries
            .values()
            .map(|e| e.timestamp)
            .max()
            .unwrap_or(0);
        let max_tomb = self.tombstones.values().copied().max().unwrap_or(0);
        max_entry.max(max_tomb)
    }
}

impl<K: Ord, V: Lattice> Default for ORMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone + PartialEq, V: Lattice> Lattice for ORMap<K, V> {
    fn merge(&self, other: &Self) -> Self {
        let mut entries: BTreeMap<K, Entry<V>> = BTreeMap::new();
        let mut tombstones: BTreeMap<K, u64> = BTreeMap::new();

        // Merge tombstones: per-key max
        for (k, &ts) in &self.tombstones {
            tombstones.insert(k.clone(), ts);
        }
        for (k, &ts) in &other.tombstones {
            let entry = tombstones.entry(k.clone()).or_insert(0);
            *entry = (*entry).max(ts);
        }

        // Merge entries
        // Collect all keys that have entries in either side
        let mut all_entry_keys: std::collections::BTreeSet<&K> = std::collections::BTreeSet::new();
        for k in self.entries.keys() {
            all_entry_keys.insert(k);
        }
        for k in other.entries.keys() {
            all_entry_keys.insert(k);
        }

        for k in all_entry_keys {
            let self_entry = self.entries.get(k);
            let other_entry = other.entries.get(k);

            let merged_entry = match (self_entry, other_entry) {
                (Some(a), Some(b)) => {
                    // Both sides have the entry; merge values and take max timestamp
                    let max_ts = a.timestamp.max(b.timestamp);
                    Entry {
                        value: a.value.merge(&b.value),
                        timestamp: max_ts,
                    }
                }
                (Some(a), None) => a.clone(),
                (None, Some(b)) => b.clone(),
                (None, None) => unreachable!(),
            };

            // Always keep entries so future merges can compare timestamps
            entries.insert(k.clone(), merged_entry);
        }

        let clock = self.max_observed_ts().max(other.max_observed_ts());

        Self {
            entries,
            tombstones,
            clock,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lattice::{assert_lattice_laws, GCounter, MaxReg};

    #[test]
    fn new_is_empty() {
        let m: ORMap<String, MaxReg<i64>> = ORMap::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn put_and_get() {
        let mut m = ORMap::new();
        m.put("a".to_string(), MaxReg::new(10i64));
        m.put("b".to_string(), MaxReg::new(20i64));

        assert_eq!(m.get(&"a".to_string()).map(|r| *r.value()), Some(10));
        assert_eq!(m.get(&"b".to_string()).map(|r| *r.value()), Some(20));
        assert!(m.get(&"c".to_string()).is_none());
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn put_overwrites() {
        let mut m = ORMap::new();
        m.put("a".to_string(), MaxReg::new(10i64));
        m.put("a".to_string(), MaxReg::new(20i64));
        assert_eq!(m.get(&"a".to_string()).map(|r| *r.value()), Some(20));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn remove_key() {
        let mut m = ORMap::new();
        m.put("a".to_string(), MaxReg::new(10i64));
        m.put("b".to_string(), MaxReg::new(20i64));
        m.remove(&"a".to_string());

        assert!(!m.contains_key(&"a".to_string()));
        assert!(m.contains_key(&"b".to_string()));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut m: ORMap<String, MaxReg<i64>> = ORMap::new();
        m.put("a".to_string(), MaxReg::new(10));
        m.remove(&"b".to_string()); // no-op
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn keys_and_entries_sorted() {
        let mut m = ORMap::new();
        m.put("c".to_string(), MaxReg::new(3i64));
        m.put("a".to_string(), MaxReg::new(1i64));
        m.put("b".to_string(), MaxReg::new(2i64));

        let keys: Vec<_> = m.keys();
        assert_eq!(keys, vec!["a", "b", "c"]);

        let entries: Vec<_> = m.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0, "a");
    }

    #[test]
    fn put_wins_after_concurrent_remove() {
        let mut a: ORMap<String, MaxReg<i64>> = ORMap::new();
        a.put("x".to_string(), MaxReg::new(1));

        let mut b = a.clone();

        // A removes
        a.remove(&"x".to_string());
        assert!(!a.contains_key(&"x".to_string()));

        // B re-puts (concurrent with A's remove)
        b.put("x".to_string(), MaxReg::new(2));
        assert!(b.contains_key(&"x".to_string()));

        // Merge: put-wins
        let merged = a.merge(&b);
        assert!(
            merged.contains_key(&"x".to_string()),
            "put-wins: key should be present after merge"
        );
    }

    #[test]
    fn merge_recursive_value_merge() {
        // Both replicas have the same key with different MaxReg values
        let mut a = ORMap::new();
        a.put("k".to_string(), MaxReg::new(10i64));

        let mut b = ORMap::new();
        b.put("k".to_string(), MaxReg::new(20i64));

        let merged = a.merge(&b);
        // MaxReg merge => max(10, 20) = 20
        assert_eq!(merged.get(&"k".to_string()).map(|r| *r.value()), Some(20));
    }

    #[test]
    fn merge_recursive_value_merge_gcounter() {
        let mut c1 = GCounter::new();
        c1.increment("node-a", 5);

        let mut c2 = GCounter::new();
        c2.increment("node-b", 3);

        let mut a = ORMap::new();
        a.put("counter".to_string(), c1);

        let mut b = ORMap::new();
        b.put("counter".to_string(), c2);

        let merged = a.merge(&b);
        let counter = merged.get(&"counter".to_string()).unwrap();
        // GCounter merge: per-node max => {node-a: 5, node-b: 3}, value = 8
        assert_eq!(counter.value(), 8);
    }

    #[test]
    fn lattice_laws_maxreg() {
        let mut a: ORMap<String, MaxReg<i64>> = ORMap::new();
        a.put("x".to_string(), MaxReg::new(1));

        let mut b: ORMap<String, MaxReg<i64>> = ORMap::new();
        b.put("y".to_string(), MaxReg::new(2));

        let mut c: ORMap<String, MaxReg<i64>> = ORMap::new();
        c.put("x".to_string(), MaxReg::new(3));
        c.put("z".to_string(), MaxReg::new(4));

        assert_lattice_laws(&a, &b, &c);
    }

    #[test]
    fn lattice_laws_with_removes() {
        let mut a: ORMap<String, MaxReg<i64>> = ORMap::new();
        a.put("x".to_string(), MaxReg::new(1));
        a.put("y".to_string(), MaxReg::new(2));
        a.remove(&"x".to_string());

        let mut b: ORMap<String, MaxReg<i64>> = ORMap::new();
        b.put("y".to_string(), MaxReg::new(5));
        b.put("z".to_string(), MaxReg::new(3));

        let mut c: ORMap<String, MaxReg<i64>> = ORMap::new();
        c.put("z".to_string(), MaxReg::new(10));

        assert_lattice_laws(&a, &b, &c);
    }

    #[test]
    fn merge_empty_maps() {
        let a: ORMap<String, MaxReg<i64>> = ORMap::new();
        let b: ORMap<String, MaxReg<i64>> = ORMap::new();
        let merged = a.merge(&b);
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_one_empty() {
        let a: ORMap<String, MaxReg<i64>> = ORMap::new();
        let mut b: ORMap<String, MaxReg<i64>> = ORMap::new();
        b.put("k".to_string(), MaxReg::new(1));

        assert_eq!(a.merge(&b).len(), 1);
        assert_eq!(b.merge(&a).len(), 1);
    }

    #[test]
    fn serde_roundtrip() {
        let mut m = ORMap::new();
        m.put("a".to_string(), MaxReg::new(10i64));
        m.put("b".to_string(), MaxReg::new(20i64));
        m.remove(&"a".to_string());

        let json = serde_json::to_string(&m).unwrap();
        let back: ORMap<String, MaxReg<i64>> = serde_json::from_str(&json).unwrap();

        // Visible state should match
        assert_eq!(m.entries(), back.entries());
        assert_eq!(m.entries, back.entries);
        assert_eq!(m.tombstones, back.tombstones);
    }

    #[test]
    fn serde_preserves_tombstones() {
        let mut m: ORMap<String, MaxReg<i64>> = ORMap::new();
        m.put("a".to_string(), MaxReg::new(1));
        m.remove(&"a".to_string());

        let json = serde_json::to_value(&m).unwrap();
        assert!(json.get("entries").is_some());
        assert!(json.get("tombstones").is_some());

        let back: ORMap<String, MaxReg<i64>> = serde_json::from_value(json).unwrap();
        assert!(!back.contains_key(&"a".to_string()));
    }
}
