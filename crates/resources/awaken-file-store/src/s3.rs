//! Object-store backend (`s3` feature): S3 / MinIO / GCS / Azure via `object_store`.
//! The object **key is the content id**, so writes are naturally immutable and
//! idempotent. Unit-tested against the in-memory object store; real S3 needs creds.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use object_store::{ObjectStore, PutPayload, path::Path as ObjPath};

use crate::{FileStore, FileStoreError, content_id};

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

/// An object-store-backed [`FileStore`]. `prefix` namespaces the keys (e.g. `blobs`).
pub struct S3FileStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3FileStore {
    /// Wrap any `object_store` implementation (real S3, MinIO, or the in-memory one).
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            store,
            prefix: prefix.into(),
        }
    }

    fn key(&self, id: &str) -> ObjPath {
        ObjPath::from(format!("{}/{}", self.prefix, id))
    }
}

#[async_trait]
impl FileStore for S3FileStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_id(bytes);
        // Content-addressed key ⇒ an identical overwrite is a harmless no-op.
        self.store
            .put(&self.key(&id), PutPayload::from(bytes.to_vec()))
            .await
            .map_err(e)?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        match self.store.get(&self.key(id)).await {
            Ok(result) => Ok(Some(result.bytes().await.map_err(e)?.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(e(err)),
        }
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        let prefix = ObjPath::from(self.prefix.clone());
        let mut stream = self.store.list(Some(&prefix));
        let mut ids = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(e)?;
            if let Some(name) = meta.location.filename() {
                ids.push(name.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    async fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        // object-store `delete` is idempotent (won't report prior existence), so
        // probe with `head` first to honor the "did it exist?" contract.
        let key = self.key(id);
        match self.store.head(&key).await {
            Ok(_) => {
                self.store.delete(&key).await.map_err(e)?;
                Ok(true)
            }
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(e(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    #[tokio::test]
    async fn round_trips_over_the_in_memory_object_store() {
        let store = S3FileStore::new(Arc::new(InMemory::new()), "blobs");
        let id = store.put(b"hello object store").await.unwrap();
        assert_eq!(
            id,
            store.put(b"hello object store").await.unwrap(),
            "idempotent"
        );
        assert_eq!(
            store.get(&id).await.unwrap().as_deref(),
            Some(&b"hello object store"[..])
        );
        assert!(store.get("blobs/missing").await.unwrap().is_none());
        assert!(store.list().await.unwrap().contains(&id));
        assert!(store.delete(&id).await.unwrap());
        assert!(!store.delete(&id).await.unwrap());
    }
}
