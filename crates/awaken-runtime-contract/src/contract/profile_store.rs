//! Profile storage types for cross-run, scoped key-value persistence.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::storage::StorageError;
use crate::error::StateError;
use crate::model::{JsonValue, decode_json, encode_json};

/// Identifies who owns a profile entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProfileOwner {
    Agent(String),
    System,
}

impl fmt::Display for ProfileOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileOwner::Agent(name) => write!(f, "agent:{name}"),
            ProfileOwner::System => write!(f, "system"),
        }
    }
}

/// A typed key for profile storage.
pub trait ProfileKey: 'static + Send + Sync {
    const KEY: &'static str;
    type Value: Clone + Default + Serialize + DeserializeOwned + Send + Sync + 'static;

    fn encode(value: &Self::Value) -> Result<JsonValue, StateError> {
        encode_json(Self::KEY, value)
    }

    fn decode(value: JsonValue) -> Result<Self::Value, StateError> {
        decode_json(Self::KEY, value)
    }
}

/// A single profile entry stored in the profile store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub key: String,
    pub value: Value,
    pub updated_at: u64,
}

/// Raw profile storage backend.
///
/// Stores opaque JSON values keyed by (owner, key). Type safety is provided
/// by `ProfileAccess` in the runtime layer.
#[async_trait]
pub trait ProfileStore: Send + Sync {
    /// Read a single entry. Returns `None` if not set.
    async fn get(
        &self,
        owner: &ProfileOwner,
        key: &str,
    ) -> Result<Option<ProfileEntry>, StorageError>;

    /// Write a single entry (upsert). Implementation sets `updated_at`.
    async fn set(&self, owner: &ProfileOwner, key: &str, value: Value) -> Result<(), StorageError>;

    /// Delete a single entry. Idempotent — no error if absent.
    async fn delete(&self, owner: &ProfileOwner, key: &str) -> Result<(), StorageError>;

    /// List all entries for an owner, sorted by key.
    async fn list(&self, owner: &ProfileOwner) -> Result<Vec<ProfileEntry>, StorageError>;

    /// Delete all entries for an owner. Idempotent.
    async fn clear_owner(&self, owner: &ProfileOwner) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_owner_display() {
        assert_eq!(ProfileOwner::Agent("foo".into()).to_string(), "agent:foo");
        assert_eq!(ProfileOwner::System.to_string(), "system");
    }

    #[test]
    fn profile_owner_equality() {
        let a = ProfileOwner::Agent("x".into());
        let b = ProfileOwner::Agent("x".into());
        let c = ProfileOwner::Agent("y".into());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(ProfileOwner::System, a);
    }

    #[test]
    fn profile_owner_serde_roundtrip() {
        let variants = vec![ProfileOwner::Agent("alice".into()), ProfileOwner::System];
        for owner in variants {
            let json = serde_json::to_string(&owner).expect("serialize");
            let back: ProfileOwner = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(owner, back);
        }
    }

    #[test]
    fn profile_entry_serde_roundtrip() {
        let entry = ProfileEntry {
            key: "lang".into(),
            value: serde_json::json!("en"),
            updated_at: 1234567890,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: ProfileEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry.key, back.key);
        assert_eq!(entry.value, back.value);
        assert_eq!(entry.updated_at, back.updated_at);
    }

    struct Locale;

    impl ProfileKey for Locale {
        const KEY: &'static str = "locale";
        type Value = String;
    }

    #[test]
    fn profile_key_encode_decode_roundtrip() {
        let value = "en-US".to_string();
        let encoded = Locale::encode(&value).expect("encode");
        let decoded = Locale::decode(encoded).expect("decode");
        assert_eq!(value, decoded);
    }

    #[test]
    fn profile_key_decode_missing_returns_default() {
        let empty = serde_json::json!("");
        let decoded = Locale::decode(empty).expect("decode");
        assert_eq!(decoded, String::default());
    }

    // ── Mock ProfileStore ──

    use std::collections::HashMap;
    use std::sync::RwLock;

    const MOCK_UPDATED_AT: u64 = 1000;

    #[derive(Debug, Default)]
    struct MockProfileStore {
        data: RwLock<HashMap<(ProfileOwner, String), ProfileEntry>>,
    }

    #[async_trait]
    impl ProfileStore for MockProfileStore {
        async fn get(
            &self,
            owner: &ProfileOwner,
            key: &str,
        ) -> Result<Option<ProfileEntry>, StorageError> {
            let guard = self
                .data
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            Ok(guard.get(&(owner.clone(), key.to_owned())).cloned())
        }

        async fn set(
            &self,
            owner: &ProfileOwner,
            key: &str,
            value: Value,
        ) -> Result<(), StorageError> {
            let mut guard = self
                .data
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            guard.insert(
                (owner.clone(), key.to_owned()),
                ProfileEntry {
                    key: key.to_owned(),
                    value,
                    updated_at: MOCK_UPDATED_AT,
                },
            );
            Ok(())
        }

        async fn delete(&self, owner: &ProfileOwner, key: &str) -> Result<(), StorageError> {
            let mut guard = self
                .data
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            guard.remove(&(owner.clone(), key.to_owned()));
            Ok(())
        }

        async fn list(&self, owner: &ProfileOwner) -> Result<Vec<ProfileEntry>, StorageError> {
            let guard = self
                .data
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            let mut entries: Vec<ProfileEntry> = guard
                .iter()
                .filter(|((o, _), _)| o == owner)
                .map(|(_, v)| v.clone())
                .collect();
            entries.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(entries)
        }

        async fn clear_owner(&self, owner: &ProfileOwner) -> Result<(), StorageError> {
            let mut guard = self
                .data
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            guard.retain(|(o, _), _| o != owner);
            Ok(())
        }
    }

    #[tokio::test]
    async fn profile_store_set_and_get() {
        let store = MockProfileStore::default();
        let owner = ProfileOwner::Agent("alice".into());
        store
            .set(&owner, "lang", serde_json::json!("en"))
            .await
            .unwrap();
        let entry = store.get(&owner, "lang").await.unwrap().unwrap();
        assert_eq!(entry.key, "lang");
        assert_eq!(entry.value, serde_json::json!("en"));
        assert_eq!(entry.updated_at, MOCK_UPDATED_AT);
    }

    #[tokio::test]
    async fn profile_store_get_missing_returns_none() {
        let store = MockProfileStore::default();
        let result = store
            .get(&ProfileOwner::System, "nonexistent")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn profile_store_delete_is_idempotent() {
        let store = MockProfileStore::default();
        let owner = ProfileOwner::Agent("bob".into());
        store.delete(&owner, "missing").await.unwrap();
        store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
        store.delete(&owner, "k").await.unwrap();
        assert!(store.get(&owner, "k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn profile_store_list_returns_sorted() {
        let store = MockProfileStore::default();
        let owner = ProfileOwner::System;
        store
            .set(&owner, "b", serde_json::json!("second"))
            .await
            .unwrap();
        store
            .set(&owner, "a", serde_json::json!("first"))
            .await
            .unwrap();
        let entries = store.list(&owner).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[1].key, "b");
    }

    #[tokio::test]
    async fn profile_store_list_isolates_owners() {
        let store = MockProfileStore::default();
        let alice = ProfileOwner::Agent("alice".into());
        let bob = ProfileOwner::Agent("bob".into());
        store.set(&alice, "x", serde_json::json!(1)).await.unwrap();
        store.set(&bob, "y", serde_json::json!(2)).await.unwrap();
        assert_eq!(store.list(&alice).await.unwrap().len(), 1);
        assert_eq!(store.list(&bob).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn profile_store_clear_owner() {
        let store = MockProfileStore::default();
        let alice = ProfileOwner::Agent("alice".into());
        let bob = ProfileOwner::Agent("bob".into());
        store.set(&alice, "a", serde_json::json!(1)).await.unwrap();
        store.set(&alice, "b", serde_json::json!(2)).await.unwrap();
        store.set(&bob, "c", serde_json::json!(3)).await.unwrap();
        store.clear_owner(&alice).await.unwrap();
        assert!(store.list(&alice).await.unwrap().is_empty());
        assert_eq!(store.list(&bob).await.unwrap().len(), 1);
    }
}
