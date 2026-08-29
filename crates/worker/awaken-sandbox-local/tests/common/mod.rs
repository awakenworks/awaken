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

// Every integration target compiles `common` independently; only the local and
// namespace provider targets consume these Phase-A fixtures (R11/R12, R14/R15).
#[allow(dead_code)]
pub fn add_future_restoration(
    handle: &awaken_provisioning_contract::SandboxHandle,
) -> awaken_provisioning_contract::SandboxHandle {
    let mut wire = serde_json::to_value(handle).unwrap();
    wire.as_object_mut().unwrap().insert(
        "restoration".into(),
        serde_json::json!({
            "effect_id": "effect-a",
            "generation_id": "generation-a",
            "checkpoint_id": "checkpoint-a",
            "checkpoint_digest": "sha256:digest-a",
            "sandbox_spec_fingerprint": "spec-a",
            "checkpoint_exclusions_fingerprint": "exclusions-a"
        }),
    );
    serde_json::from_value(wire).unwrap()
}

#[allow(dead_code)]
pub fn future_host_bind_handle() -> awaken_provisioning_contract::SandboxHandle {
    serde_json::from_value(serde_json::json!({
        "sandbox_id": "future-container",
        "payload": {
            "schema": "container_v1",
            "container_id": "future-container",
            "outputs_path": "/outputs",
            "base_env": [],
            "live_input_projection": false,
            "continuation_excluded_paths": [],
            "runtime_handle": {
                "kind": "host_bind_restoration",
                "staging_root": "/provider/staging/a"
            }
        }
    }))
    .unwrap()
}
