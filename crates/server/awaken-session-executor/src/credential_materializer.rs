//! Exact credential realization for publication-pinned runtime access.
//!
//! This host adapter owns no selection policy and cannot enumerate credentials. It
//! accepts one immutable [`ResolvedModelCandidate`], verifies its
//! Workspace/revision/usage pins against the persisted row, then materializes that exact secret. Native
//! provider execution and ACP provisioning share this adapter so they cannot drift.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{CredentialSourceId, CredentialStatus, SecretStore};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_contract::{CredentialInjectionKind, CredentialUsage};

/// Worker/host-side realization of one already-published credential reference.
#[derive(Clone)]
pub struct PinnedCredentialMaterializer {
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
}

impl PinnedCredentialMaterializer {
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            credentials,
            secrets,
        }
    }

    /// Materialize exactly the provider credential frozen in `access`.
    ///
    /// Catalog lookup, default selection and failover are intentionally absent.
    /// Every mismatch is terminal for this pin; the caller may only try another
    /// complete candidate that was already included in the published snapshot.
    pub async fn materialize_provider(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Result<RedactedString, String> {
        let ModelProvisioning::Provider {
            provider_ref,
            scope_id,
            credential,
            ..
        } = &candidate.provisioning
        else {
            return Err("model candidate does not require a provider credential".into());
        };
        let provider = provider_ref
            .split_once('@')
            .map(|(provider, _)| provider)
            .ok_or_else(|| "published model candidate has no versioned provider pin".to_string())?;
        let credential = credential
            .as_ref()
            .ok_or_else(|| "published model candidate has no credential pin".to_string())?;
        if credential.injection != CredentialInjectionKind::Reference
            || credential.usage != CredentialUsage::ProviderAdapter
        {
            return Err("published credential injection contract is invalid".to_string());
        }

        let source = self
            .credentials
            .get(&CredentialSourceId(credential.credential.id.clone()))
            .await
            .map_err(|error| error.to_string())?;
        let revision = u64::try_from(source.version)
            .map_err(|_| format!("credential {} has a negative version", source.id.0))?;
        if source.workspace_id != scope_id.as_str() {
            return Err("published credential Workspace owner changed".to_string());
        }
        if source.status != CredentialStatus::Active {
            return Err(format!("credential {} is not active", source.id.0));
        }
        if revision != credential.credential.revision {
            return Err(format!("credential {} revision changed", source.id.0));
        }
        if source
            .provider_id
            .as_deref()
            .is_some_and(|configured| configured != provider)
        {
            return Err(format!(
                "credential {} cannot authenticate provider {provider}",
                source.id.0
            ));
        }
        awaken_credential_vault::materialize(&source, self.secrets.as_ref())
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SecretBroker for PinnedCredentialMaterializer {
    async fn materialize(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        let source = self
            .credentials
            .get(&CredentialSourceId(reference.to_string()))
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        if source.status != CredentialStatus::Active {
            return Err(awaken_provisioning_contract::SandboxError::new(format!(
                "credential {} is not active",
                source.id.0
            )));
        }
        awaken_credential_vault::materialize(&source, self.secrets.as_ref())
            .await
            .map(|secret| secret.expose_secret().as_bytes().to_vec())
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
    }

    async fn write_back(
        &self,
        reference: &str,
        bytes: Vec<u8>,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let source = self
            .credentials
            .get(&CredentialSourceId(reference.to_string()))
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        if source.status != CredentialStatus::Active {
            return Err(awaken_provisioning_contract::SandboxError::new(format!(
                "credential {} is not active",
                source.id.0
            )));
        }
        let material_ref = source.material_ref.as_ref().ok_or_else(|| {
            awaken_provisioning_contract::SandboxError::new(format!(
                "credential {} has no material",
                source.id.0
            ))
        })?;
        let material = String::from_utf8(bytes).map_err(|_| {
            awaken_provisioning_contract::SandboxError::new(
                "credential write-back is not valid UTF-8",
            )
        })?;
        self.secrets
            .put(material_ref, RedactedString::new(material))
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::SecretBroker;

    #[tokio::test]
    async fn credential_file_broker_reuses_the_persisted_vault() {
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let source = awaken_credential_vault::repo::enter_credential(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: Some("tool".into()),
                env_key: None,
                secret: Some(RedactedString::new("before")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let broker = PinnedCredentialMaterializer::new(credentials, secrets);

        assert_eq!(broker.materialize(&source.id.0).await.unwrap(), b"before");
        broker
            .write_back(&source.id.0, b"after".to_vec())
            .await
            .unwrap();
        assert_eq!(broker.materialize(&source.id.0).await.unwrap(), b"after");
    }
}
