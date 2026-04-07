//! Typed profile access layer over raw `ProfileStore`.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::profile_store::{
    ProfileEntry, ProfileKey, ProfileOwner, ProfileStore,
};
use awaken_contract::contract::storage::StorageError;

/// Registry of valid profile key names, built from plugin registrations.
pub struct ProfileKeyRegistry {
    keys: HashMap<String, ()>,
}

impl ProfileKeyRegistry {
    pub fn new(key_names: impl IntoIterator<Item = String>) -> Self {
        Self {
            keys: key_names.into_iter().map(|k| (k, ())).collect(),
        }
    }

    fn is_registered(&self, key: &str) -> bool {
        self.keys.contains_key(key)
    }
}

/// Typed wrapper around a `ProfileStore` that validates keys against the registry.
pub struct ProfileAccess {
    store: Arc<dyn ProfileStore>,
    registry: ProfileKeyRegistry,
}

impl ProfileAccess {
    pub fn new(store: Arc<dyn ProfileStore>, registry: ProfileKeyRegistry) -> Self {
        Self { store, registry }
    }

    /// Read a typed value. Returns `K::Value::default()` if the entry is missing.
    ///
    /// - `K::KEY` is the namespace (which data type)
    /// - `key` is the dynamic identifier (which instance)
    pub async fn read<K: ProfileKey>(&self, key: &str) -> Result<K::Value, StorageError> {
        self.ensure_registered(K::KEY)?;
        let owner = Self::key_to_owner(key);
        match self.store.get(&owner, K::KEY).await? {
            Some(entry) => K::decode(entry.value).map_err(|e| StorageError::Io(e.to_string())),
            None => Ok(K::Value::default()),
        }
    }

    /// Write a typed value.
    pub async fn write<K: ProfileKey>(
        &self,
        key: &str,
        value: &K::Value,
    ) -> Result<(), StorageError> {
        self.ensure_registered(K::KEY)?;
        let json = K::encode(value).map_err(|e| StorageError::Io(e.to_string()))?;
        self.store.set(&Self::key_to_owner(key), K::KEY, json).await
    }

    /// Delete a typed entry.
    pub async fn delete<K: ProfileKey>(&self, key: &str) -> Result<(), StorageError> {
        self.ensure_registered(K::KEY)?;
        self.store.delete(&Self::key_to_owner(key), K::KEY).await
    }

    /// List all entries for a key.
    pub async fn list(&self, key: &str) -> Result<Vec<ProfileEntry>, StorageError> {
        self.store.list(&Self::key_to_owner(key)).await
    }

    /// Delete all entries for a key.
    pub async fn clear(&self, key: &str) -> Result<(), StorageError> {
        self.store.clear_owner(&Self::key_to_owner(key)).await
    }

    /// Map a user-facing key string to a `ProfileOwner` for storage.
    fn key_to_owner(key: &str) -> ProfileOwner {
        ProfileOwner::Agent(key.to_owned())
    }

    fn ensure_registered(&self, key: &str) -> Result<(), StorageError> {
        if self.registry.is_registered(key) {
            Ok(())
        } else {
            Err(StorageError::NotFound(format!(
                "profile key not registered: {key}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    // -- Test profile keys --

    struct Locale;
    impl ProfileKey for Locale {
        const KEY: &'static str = "locale";
        type Value = String;
    }

    struct Unregistered;
    impl ProfileKey for Unregistered {
        const KEY: &'static str = "unregistered";
        type Value = String;
    }

    // -- Mock store --

    #[derive(Debug, Default)]
    struct MockStore {
        data: RwLock<HashMap<(String, String), ProfileEntry>>,
    }

    #[async_trait]
    impl ProfileStore for MockStore {
        async fn get(
            &self,
            owner: &ProfileOwner,
            key: &str,
        ) -> Result<Option<ProfileEntry>, StorageError> {
            let guard = self.data.read().await;
            Ok(guard.get(&(owner.to_string(), key.to_owned())).cloned())
        }

        async fn set(
            &self,
            owner: &ProfileOwner,
            key: &str,
            value: Value,
        ) -> Result<(), StorageError> {
            let mut guard = self.data.write().await;
            guard.insert(
                (owner.to_string(), key.to_owned()),
                ProfileEntry {
                    key: key.to_owned(),
                    value,
                    updated_at: 1000,
                },
            );
            Ok(())
        }

        async fn delete(&self, owner: &ProfileOwner, key: &str) -> Result<(), StorageError> {
            let mut guard = self.data.write().await;
            guard.remove(&(owner.to_string(), key.to_owned()));
            Ok(())
        }

        async fn list(&self, owner: &ProfileOwner) -> Result<Vec<ProfileEntry>, StorageError> {
            let guard = self.data.read().await;
            let owner_str = owner.to_string();
            let mut entries: Vec<ProfileEntry> = guard
                .iter()
                .filter(|((o, _), _)| o == &owner_str)
                .map(|(_, v)| v.clone())
                .collect();
            entries.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(entries)
        }

        async fn clear_owner(&self, owner: &ProfileOwner) -> Result<(), StorageError> {
            let mut guard = self.data.write().await;
            let owner_str = owner.to_string();
            guard.retain(|(o, _), _| o != &owner_str);
            Ok(())
        }
    }

    fn make_access(keys: &[&str]) -> ProfileAccess {
        let registry = ProfileKeyRegistry::new(keys.iter().map(|k| k.to_string()));
        let store: Arc<dyn ProfileStore> = Arc::new(MockStore::default());
        ProfileAccess::new(store, registry)
    }

    #[tokio::test]
    async fn read_missing_returns_default() {
        let access = make_access(&["locale"]);
        let val = access.read::<Locale>("system").await.unwrap();
        assert_eq!(val, String::default());
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let access = make_access(&["locale"]);
        access
            .write::<Locale>("alice", &"en-US".to_string())
            .await
            .unwrap();
        let val = access.read::<Locale>("alice").await.unwrap();
        assert_eq!(val, "en-US");
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let access = make_access(&["locale"]);
        access
            .write::<Locale>("system", &"fr".to_string())
            .await
            .unwrap();
        access.delete::<Locale>("system").await.unwrap();
        let val = access.read::<Locale>("system").await.unwrap();
        assert_eq!(val, String::default());
    }

    #[tokio::test]
    async fn unregistered_key_returns_error() {
        let access = make_access(&["locale"]);
        let err = access.read::<Unregistered>("system").await.unwrap_err();
        assert!(err.to_string().contains("not registered"));
    }

    #[tokio::test]
    async fn keys_are_isolated() {
        let access = make_access(&["locale"]);
        access
            .write::<Locale>("alice", &"en".to_string())
            .await
            .unwrap();
        access
            .write::<Locale>("bob", &"fr".to_string())
            .await
            .unwrap();
        assert_eq!(access.read::<Locale>("alice").await.unwrap(), "en");
        assert_eq!(access.read::<Locale>("bob").await.unwrap(), "fr");
    }
}
