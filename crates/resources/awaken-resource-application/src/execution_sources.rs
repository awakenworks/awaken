//! In-process adapters from Resources authorities to execution-facing ports.

use std::sync::Arc;

use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    FileApplicationService, FileContentSource, FileContentSourceError, RepositoryBindingVerifier,
    RepositoryBindingVerifierError, ResourceBindingValidator, SkillStore, SkillVersion, content_id,
};
use awaken_session_contract::{
    ResolvedSkillBinding, SkillBundleSource, SkillBundleSourceError, validate_skill_bundle,
};

pub struct ApplicationFileContentSource {
    application: Arc<dyn FileApplicationService>,
}

impl ApplicationFileContentSource {
    #[must_use]
    pub fn new(application: Arc<dyn FileApplicationService>) -> Self {
        Self { application }
    }
}

#[async_trait::async_trait]
impl<C: Sync> FileContentSource<C> for ApplicationFileContentSource {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        _fence: Option<&C>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError> {
        let Some((record, bytes)) = self
            .application
            .bytes(workspace_id, file_id)
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
        else {
            return Ok(None);
        };
        let actual = content_id(&bytes);
        if actual != record.blob_id {
            return Err(FileContentSourceError::new(format!(
                "File `{file_id}` content digest mismatch: expected {}, received {actual}",
                record.blob_id
            )));
        }
        Ok(Some((record.blob_id, bytes)))
    }
}

pub struct ApplicationArtifactPublisher {
    application: Arc<dyn FileApplicationService>,
}

impl ApplicationArtifactPublisher {
    #[must_use]
    pub fn new(application: Arc<dyn FileApplicationService>) -> Self {
        Self { application }
    }
}

#[async_trait::async_trait]
impl<C: Send + Sync + 'static> ArtifactPublisher<C> for ApplicationArtifactPublisher {
    async fn publish(
        &self,
        publication: ArtifactPublication<C>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        publication.verify()?;
        let record = self
            .application
            .create_artifact(
                &publication.workspace_id,
                &publication.session_id,
                publication.logical_path.clone(),
                publication.mime_type.clone(),
                &publication.bytes,
                publication.effect_id.clone(),
            )
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        let receipt = ArtifactPublicationReceipt {
            effect_id: publication.effect_id.clone(),
            content_id: publication.content_id.clone(),
            record,
        };
        receipt.verify(&publication)?;
        Ok(receipt)
    }
}

pub struct CatalogRepositoryBindingVerifier {
    validator: Arc<dyn ResourceBindingValidator>,
}

impl CatalogRepositoryBindingVerifier {
    #[must_use]
    pub fn new(validator: Arc<dyn ResourceBindingValidator>) -> Self {
        Self { validator }
    }
}

#[async_trait::async_trait]
impl<C: Sync> RepositoryBindingVerifier<C> for CatalogRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: awaken_resource_contract::ConfigVersion,
        _fence: Option<&C>,
    ) -> Result<(), RepositoryBindingVerifierError> {
        self.validator
            .validate_repository_binding(workspace_id, repository_id, config_version)
            .map_err(|error| RepositoryBindingVerifierError::new(error.to_string()))
    }
}

pub struct StoreSkillBundleSource {
    store: Arc<dyn SkillStore>,
}

impl StoreSkillBundleSource {
    #[must_use]
    pub fn new(store: Arc<dyn SkillStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl<C: Sync> SkillBundleSource<C> for StoreSkillBundleSource {
    async fn load(
        &self,
        workspace_id: &str,
        binding: &ResolvedSkillBinding,
        _fence: Option<&C>,
    ) -> Result<Option<SkillVersion>, SkillBundleSourceError> {
        if binding.kind != AgentSkillKind::Custom {
            return Err(SkillBundleSourceError::new(
                "only custom Skills are stored in the Resources context",
            ));
        }
        self.store
            .version(workspace_id, &binding.skill_id, binding.version)
            .await
            .map_err(|error| SkillBundleSourceError::new(error.to_string()))?
            .map(|version| validate_skill_bundle(binding, version))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn missing_file_application_fails_closed() {
        // Cause/effect graph: C1 no File application was injected; C2 runtime
        // requests immutable bytes. Decision U1: C1+C2 -> explicit error, with
        // no fallback to FileCatalog/FileStore or process-local state.
        let error = <awaken_resource_contract::UnavailableFileContentSource as
            awaken_resource_contract::FileContentSource<()>>::read(
                &awaken_resource_contract::UnavailableFileContentSource,
                "workspace",
                "file",
                None,
            )
            .await
            .expect_err("U1");
        assert!(error.to_string().contains("not configured"), "U1: {error}");
    }
}
