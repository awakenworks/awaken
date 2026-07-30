//! The shared credential-materialization store subset a shared-store **worker**
//! opens, exactly as the Serve composition opens it.
//!
//! Publication has already pinned endpoint + credential access into the executable
//! snapshot, so inference execution needs only the **credential** repo and sealed
//! **secret** store. Resource-capable workers may additionally open the narrow
//! Resource Catalog validation port; they never open config authoring or Session
//! stores and never evaluate IAM policy. This is a transitional deployment
//! composition, not strict process-level Credential authority isolation.

use std::path::Path;
use std::sync::Arc;

use crate::control_stores::{ControlStoreConfig, StoreBackend};

/// The two ports needed to materialize snapshot-pinned inference access.
#[derive(Clone)]
pub struct InferenceMaterializationStores {
    pub credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    pub secrets: Arc<dyn awaken_credential_vault::SecretStore>,
}

/// Open the credential vault + sealed secret store from `cfg`, sealing secrets
/// under `key`. The credential component honors `AWAKEN_CREDENTIAL_DB` (a SQLite
/// file or shared Postgres); no catalog/config/admin backend is opened.
pub async fn open_inference_materialization_stores(
    cfg: &ControlStoreConfig,
    key: &[u8; 32],
) -> InferenceMaterializationStores {
    fn ensure_parent(backend: &StoreBackend) {
        if let StoreBackend::Sqlite(path) = backend
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).expect("create control-store directory");
        }
    }
    let path = |p: &Path| p.to_string_lossy().into_owned();

    // The credential repo and its sealed-secret blobs share the one credential backend.
    ensure_parent(&cfg.credential);
    let (credentials, secrets): (
        Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        Arc<dyn awaken_credential_vault::SecretStore>,
    ) = match &cfg.credential {
        StoreBackend::Sqlite(p) => {
            let file = path(p);
            let (creds, blobs) = awaken_credential_vault::sqlite::open_migrated_pair(&file)
                .expect("open credential sqlite");
            (
                Arc::new(creds),
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
        StoreBackend::Postgres(url) => {
            let creds = Arc::new(
                awaken_credential_vault::PostgresCredentialRepo::connect(url)
                    .await
                    .expect("connect credential postgres"),
            );
            let blobs = awaken_credential_vault::PostgresSealedBlobStore::connect(url)
                .await
                .expect("connect credential sealed-blob postgres");
            (
                creds,
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
    };

    InferenceMaterializationStores {
        credentials,
        secrets,
    }
}

/// Open only the shared Resource Catalog validation port required by a remote
/// execution worker. The worker never opens authoring/session stores and never
/// evaluates IAM policy; it checks intrinsic Workspace ownership, lifecycle state,
/// and the config version already frozen in the dispatch manifest.
/// Open the narrow Resource Catalog validation port from an explicitly
/// resolved control-store backend.
pub async fn open_shared_resource_validator(
    backend: Option<&StoreBackend>,
) -> Result<
    Option<Arc<dyn awaken_protocol_managed::resource_plane::ResourceBindingValidator>>,
    String,
> {
    let Some(backend) = backend else {
        return Ok(None);
    };
    let StoreBackend::Postgres(url) = backend else {
        return Err(
            "a remote resource worker requires AWAKEN_ADMIN_DB to be a shared postgres URL"
                .to_string(),
        );
    };
    let url = url.clone();
    let store = tokio::task::spawn_blocking(move || {
        awaken_admin_config_api::PostgresAdminStore::connect(&url)
    })
    .await
    .map_err(|error| format!("join shared Resource Catalog connect: {error}"))?
    .map_err(|error| format!("connect shared Resource Catalog: {error}"))?;
    Ok(Some(Arc::new(store)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::SecretRef;

    /// A [`ControlStoreConfig`] whose sqlite files live under a *nested,
    /// not-yet-existing* directory, so opening must exercise `ensure_parent`.
    fn nested_sqlite_cfg(root: &Path) -> ControlStoreConfig {
        let dir = root.join("deep").join("nested");
        ControlStoreConfig {
            catalog: StoreBackend::Sqlite(dir.join("catalog.db")),
            credential: StoreBackend::Sqlite(dir.join("credential.db")),
            config: StoreBackend::Sqlite(dir.join("config.db")),
            admin: StoreBackend::Sqlite(dir.join("admin.db")),
            sessions: StoreBackend::Sqlite(dir.join("sessions.db")),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sqlite_backends_open_under_a_missing_parent_and_the_seal_round_trips() {
        // The worker's model-resolution store subset must open cleanly from a
        // durable sqlite config (creating the missing parent dir), migrate, and
        // seal/reveal a secret under the provided key — the end-to-end wiring a
        // drained run relies on, with no live DB.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = nested_sqlite_cfg(tmp.path());
        let key = [7u8; 32];

        let stores = open_inference_materialization_stores(&cfg, &key).await;

        // The nested parent was created by ensure_parent (else the opens panic).
        assert!(tmp.path().join("deep").join("nested").is_dir());

        // The credential repo reads an empty workspace without error.
        assert!(
            stores
                .credentials
                .list("wrkspc_default")
                .await
                .expect("credential list")
                .is_empty()
        );

        // The sealed-AEAD secret store round-trips through the credential backend
        // under the supplied key: put ciphertext, reveal the original plaintext.
        let secret_ref = SecretRef("worker-seal-probe".to_string());
        stores
            .secrets
            .put(&secret_ref, RedactedString::new("s3cr3t-value"))
            .await
            .expect("seal a secret");
        let revealed = stores.secrets.get(&secret_ref).await.expect("reveal");
        assert_eq!(revealed.expose_secret(), "s3cr3t-value");
    }
}
