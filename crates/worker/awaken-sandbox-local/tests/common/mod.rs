//! Shared test support: adapt a content-addressed `FileStore` (resources tier) to
//! the provider's dependency-inverted `BlobSource` port — the same adaptation the
//! composition root performs in production.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_file_store::FileStore;
use awaken_provisioning_contract::BlobSource;

struct FileStoreBlobs(Arc<dyn FileStore>);

#[async_trait]
impl BlobSource for FileStoreBlobs {
    async fn get(&self, id: &str) -> Option<Vec<u8>> {
        self.0.get(id).await.ok().flatten()
    }
}

/// Expose a `FileStore` as a `BlobSource` for injection into a provider.
pub fn blob_source(store: Arc<dyn FileStore>) -> Arc<dyn BlobSource> {
    Arc::new(FileStoreBlobs(store))
}
