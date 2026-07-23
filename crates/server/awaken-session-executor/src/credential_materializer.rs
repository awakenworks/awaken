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
        if source.workspace_id != *scope_id {
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
