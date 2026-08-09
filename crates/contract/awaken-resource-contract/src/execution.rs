//! Protocol-neutral execution ports into the Resources authority.
//!
//! `C` is the caller-owned fencing context. The Resources contract never knows
//! whether it is a durable Run claim, an embedded marker, or another transport
//! capability; remote adapters interpret it and local adapters may ignore it.

use async_trait::async_trait;

use crate::{ConfigVersion, FileRecord, ResourceAccess, content_id, harvest_idempotency_key};

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("file content source: {0}")]
pub struct FileContentSourceError(String);

impl FileContentSourceError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait]
pub trait FileContentSource<C: Sync = ()>: Send + Sync {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        fence: Option<&C>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError>;
}

pub struct UnavailableFileContentSource;

#[async_trait]
impl<C: Sync> FileContentSource<C> for UnavailableFileContentSource {
    async fn read(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _fence: Option<&C>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError> {
        Err(FileContentSourceError::new(
            "File content source is not configured by the composition root",
        ))
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("Repository binding verifier: {0}")]
pub struct RepositoryBindingVerifierError(String);

impl RepositoryBindingVerifierError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait]
pub trait RepositoryBindingVerifier<C: Sync = ()>: Send + Sync {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        fence: Option<&C>,
    ) -> Result<(), RepositoryBindingVerifierError>;
}

#[derive(Debug, Clone)]
pub struct ArtifactPublication<C> {
    /// Stable effect identity authored by the harvesting boundary before I/O.
    pub effect_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub logical_path: String,
    pub mime_type: String,
    /// Digest observed while the Sandbox was still fenced and live.
    pub content_id: String,
    pub bytes: Vec<u8>,
    pub fence: Option<C>,
}

impl<C> ArtifactPublication<C> {
    pub fn verify(&self) -> Result<(), ArtifactPublicationError> {
        let actual = content_id(&self.bytes);
        let expected_effect =
            harvest_idempotency_key(&self.session_id, &self.logical_path, &self.content_id);
        if self.content_id != actual {
            return Err(ArtifactPublicationError::new(
                "artifact bytes do not match the asserted content id",
            ));
        }
        if self.effect_id != expected_effect {
            return Err(ArtifactPublicationError::new(
                "artifact effect id does not match its exact publication intent",
            ));
        }
        Ok(())
    }
}

/// Durable Resources-owned evidence for one artifact publication effect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactPublicationReceipt {
    pub effect_id: String,
    pub content_id: String,
    pub record: FileRecord,
}

impl ArtifactPublicationReceipt {
    pub fn verify<C>(
        &self,
        publication: &ArtifactPublication<C>,
    ) -> Result<(), ArtifactPublicationError> {
        publication.verify()?;
        if self.effect_id == publication.effect_id
            && self.content_id == publication.content_id
            && self.record.blob_id == publication.content_id
            && self.record.workspace_id == publication.workspace_id
            && self.record.scope_id.as_deref() == Some(publication.session_id.as_str())
            && self.record.logical_path.as_deref() == Some(publication.logical_path.as_str())
            && self.record.harvest_key.as_deref() == Some(publication.effect_id.as_str())
        {
            Ok(())
        } else {
            Err(ArtifactPublicationError::new(
                "artifact receipt does not match its exact publication intent",
            ))
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("artifact publication error: {0}")]
pub struct ArtifactPublicationError(String);

impl ArtifactPublicationError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait]
pub trait ArtifactPublisher<C: Send + Sync + 'static = ()>: Send + Sync {
    async fn publish(
        &self,
        publication: ArtifactPublication<C>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError>;
}

pub struct UnavailableArtifactPublisher;

#[async_trait]
impl<C: Send + Sync + 'static> ArtifactPublisher<C> for UnavailableArtifactPublisher {
    async fn publish(
        &self,
        _publication: ArtifactPublication<C>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        Err(ArtifactPublicationError::new(
            "artifact publisher is not configured by the composition root",
        ))
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("Memory materialization reference: {0}")]
pub struct MemoryMaterializationReferenceError(String);

impl MemoryMaterializationReferenceError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Encode one process-local Memory mount reference from a caller-owned fence.
/// Only a transport adapter defines its wire representation.
pub trait MemoryMaterializationReferenceEncoder<C>: Send + Sync {
    fn encode(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        config_version: ConfigVersion,
        access: ResourceAccess,
        fence: &C,
    ) -> Result<String, MemoryMaterializationReferenceError>;
}
