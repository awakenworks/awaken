// @formal surface obligation=session_environment.source_disposal_requires_a_verified_checkpoint
// @formal reason: The adapter publishes checkpoint bytes and their receipt together. The Session continuation owns admission to source disposal.
//! S3 byte custody for the existing Session Environment continuation.
//!
//! A conditional create publishes the bytes and their receipt together. A
//! conditional replacement retires the bytes while keeping the effect identity
//! occupied, so a late retry cannot recreate a deleted checkpoint.

use std::sync::Arc;

use awaken_deployment_config::SandboxCheckpointS3Config;
use awaken_provisioning_contract::{
    CheckpointObjectMetadata, SandboxCheckpointStore, SandboxError, StoredCheckpointObject,
};
use awaken_resource_contract::content_id;
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::path::Path;
use object_store::{
    Attribute, Attributes, GetOptions, GetResult, ObjectStore, ObjectStoreExt, PutMode, PutOptions,
    UpdateVersion,
};

const DIGEST: Attribute = Attribute::Metadata(std::borrow::Cow::Borrowed("awaken-digest"));
const METADATA: Attribute = Attribute::Metadata(std::borrow::Cow::Borrowed("awaken-metadata"));
const RETIRED: Attribute = Attribute::Metadata(std::borrow::Cow::Borrowed("awaken-retired"));

pub(super) struct S3CheckpointStore {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
}

impl S3CheckpointStore {
    pub(super) fn from_config(config: &SandboxCheckpointS3Config) -> Result<Self, String> {
        let prefix = Path::parse(&config.prefix).map_err(|error| error.to_string())?;
        if prefix.as_ref().is_empty()
            || prefix.as_ref() != config.prefix
            || config.bucket.trim().is_empty()
            || config.region.trim().is_empty()
        {
            return Err(
                "sandbox_checkpoint_s3 requires a bucket, region and normalized prefix".into(),
            );
        }
        let store = AmazonS3Builder::from_env()
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .map_err(|error| format!("configure Sandbox checkpoint S3: {error}"))?;
        Ok(Self {
            store: Arc::new(store),
            prefix,
        })
    }

    fn key(&self, id: &str) -> Result<Path, SandboxError> {
        let parts = id.split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            [
                "v1",
                "scopes",
                scope,
                "sessions",
                session,
                "sandboxes",
                generation,
                "checkpoints",
                effect,
            ] if [scope, session, generation, effect]
                .into_iter()
                .all(|value| digest_id(value)) =>
            {
                Path::parse(format!("{}/{id}", self.prefix.as_ref()))
                    .map_err(|_| SandboxError::invalid("invalid Sandbox checkpoint object id"))
            }
            _ => Err(SandboxError::invalid(
                "invalid Sandbox checkpoint object id",
            )),
        }
    }

    async fn lookup(&self, key: &Path) -> Result<Option<GetResult>, SandboxError> {
        match self
            .store
            .get_opts(key, GetOptions::new().with_head(true))
            .await
        {
            Ok(object) => Ok(Some(object)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(SandboxError::backend("observe Sandbox checkpoint", error)),
        }
    }
}

fn digest_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn checkpoint_id(metadata: &CheckpointObjectMetadata) -> Result<String, SandboxError> {
    let [scope, session, generation, effect] = [
        &metadata.workspace_id,
        &metadata.session_id,
        &metadata.generation_id,
        &metadata.suspend_effect_id,
    ];
    if [scope, session, generation, effect]
        .into_iter()
        .any(|id| id.trim().is_empty())
        || metadata.created_at_unix_ms >= metadata.expires_at_unix_ms
    {
        return Err(SandboxError::invalid(
            "incomplete Sandbox checkpoint metadata",
        ));
    }
    Ok(format!(
        "v1/scopes/{}/sessions/{}/sandboxes/{}/checkpoints/{}",
        content_id(scope.as_bytes()),
        content_id(session.as_bytes()),
        content_id(generation.as_bytes()),
        content_id(effect.as_bytes()),
    ))
}

fn retired(object: &GetResult) -> bool {
    object
        .attributes
        .get(&RETIRED)
        .is_some_and(|value| value.as_ref() == "1")
}

#[async_trait::async_trait]
impl SandboxCheckpointStore for S3CheckpointStore {
    async fn put(
        &self,
        metadata: &CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<StoredCheckpointObject, SandboxError> {
        let id = checkpoint_id(metadata)?;
        let key = self.key(&id)?;
        let digest = content_id(&bytes);
        let size_bytes = bytes.len() as u64;
        // The other metadata coordinates are already bound by the object key.
        let metadata_digest = content_id(
            format!(
                "{}:{}",
                metadata.created_at_unix_ms, metadata.expires_at_unix_ms
            )
            .as_bytes(),
        );
        let mut attributes = Attributes::new();
        attributes.insert(DIGEST, digest.clone().into());
        attributes.insert(METADATA, metadata_digest.clone().into());
        let options = PutOptions {
            mode: PutMode::Create,
            attributes,
            ..Default::default()
        };
        if let Err(error) = self.store.put_opts(&key, bytes.into(), options).await {
            // Reconcile a precondition failure or a lost successful response.
            let existing = self
                .lookup(&key)
                .await?
                .ok_or_else(|| SandboxError::backend("publish Sandbox checkpoint", error))?;
            if retired(&existing) {
                return Err(SandboxError::gone("Sandbox checkpoint was retired"));
            }
            if existing.meta.size != size_bytes
                || existing.attributes.get(&DIGEST).map(AsRef::as_ref) != Some(digest.as_str())
                || existing.attributes.get(&METADATA).map(AsRef::as_ref)
                    != Some(metadata_digest.as_str())
            {
                return Err(SandboxError::conflict(
                    "Sandbox checkpoint effect is already bound to different bytes or metadata",
                ));
            }
        }
        Ok(StoredCheckpointObject {
            id,
            digest,
            size_bytes,
        })
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>, SandboxError> {
        let key = self.key(id)?;
        let object = self.store.get(&key).await.map_err(|error| match error {
            object_store::Error::NotFound { .. } => {
                SandboxError::not_found("Sandbox checkpoint is absent")
            }
            other => SandboxError::backend("read Sandbox checkpoint", other),
        })?;
        if retired(&object) {
            return Err(SandboxError::not_found("Sandbox checkpoint was retired"));
        }
        let digest = object.attributes.get(&DIGEST).cloned().ok_or_else(|| {
            SandboxError::conflict("Sandbox checkpoint is missing its content digest")
        })?;
        let size = object.meta.size;
        let bytes = object
            .bytes()
            .await
            .map_err(|error| SandboxError::backend("read Sandbox checkpoint", error))?;
        if size != bytes.len() as u64 || digest.as_ref() != content_id(&bytes) {
            return Err(SandboxError::conflict(
                "Sandbox checkpoint bytes failed verification",
            ));
        }
        Ok(bytes.to_vec())
    }

    async fn delete(&self, id: &str) -> Result<(), SandboxError> {
        let key = self.key(id)?;
        for _ in 0..4 {
            let mode = match self.lookup(&key).await? {
                Some(object) if retired(&object) => return Ok(()),
                Some(object) => {
                    let etag = object.meta.e_tag.ok_or_else(|| {
                        SandboxError::conflict("Sandbox checkpoint is missing its object version")
                    })?;
                    PutMode::Update(UpdateVersion {
                        e_tag: Some(etag),
                        version: object.meta.version,
                    })
                }
                None => PutMode::Create,
            };
            let mut attributes = Attributes::new();
            attributes.insert(RETIRED, "1".into());
            let options = PutOptions {
                mode,
                attributes,
                ..Default::default()
            };
            match self.store.put_opts(&key, Default::default(), options).await {
                Ok(_) => return Ok(()),
                Err(
                    object_store::Error::Precondition { .. }
                    | object_store::Error::AlreadyExists { .. },
                ) => {}
                Err(error) => {
                    return Err(SandboxError::backend("retire Sandbox checkpoint", error));
                }
            }
        }
        Err(SandboxError::not_ready(
            "Sandbox checkpoint changed during retirement",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkpoint_replay_preserves_bytes_and_retirement_prevents_resurrection() {
        let store = S3CheckpointStore {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: Path::from("deployment"),
        };
        let metadata = CheckpointObjectMetadata {
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            generation_id: "generation".into(),
            suspend_effect_id: "effect".into(),
            created_at_unix_ms: 10,
            expires_at_unix_ms: 20,
        };
        let bytes = b"filesystem".to_vec();
        let receipt = store.put(&metadata, bytes.clone()).await.unwrap();
        assert_eq!(store.put(&metadata, bytes.clone()).await.unwrap(), receipt);
        let altered = CheckpointObjectMetadata {
            expires_at_unix_ms: 30,
            ..metadata.clone()
        };
        assert!(matches!(
            store.put(&altered, bytes.clone()).await,
            Err(SandboxError::Conflict(_))
        ));
        assert!(matches!(
            store.put(&metadata, b"other".to_vec()).await,
            Err(SandboxError::Conflict(_))
        ));
        assert_eq!(store.get(&receipt.id).await.unwrap(), bytes);
        assert!(matches!(
            store.get("../foreign").await,
            Err(SandboxError::Invalid(_))
        ));
        store.delete(&receipt.id).await.unwrap();
        store.delete(&receipt.id).await.unwrap();
        assert!(matches!(
            store.get(&receipt.id).await,
            Err(SandboxError::NotFound(_))
        ));
        assert!(matches!(
            store.put(&metadata, bytes).await,
            Err(SandboxError::Gone(_))
        ));

        let uncommitted = CheckpointObjectMetadata {
            suspend_effect_id: "uncommitted-effect".into(),
            ..metadata
        };
        let uncommitted_id = checkpoint_id(&uncommitted).unwrap();
        store.delete(&uncommitted_id).await.unwrap();
        assert!(matches!(
            store.put(&uncommitted, b"late".to_vec()).await,
            Err(SandboxError::Gone(_))
        ));
    }
}
