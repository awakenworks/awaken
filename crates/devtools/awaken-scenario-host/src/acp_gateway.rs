//! Hermetic cloud-managed ACP gateway resolver fixtures.

use std::sync::Arc;

/// Explicit environment fixture for dev-only live scenarios. Product startup
/// uses publication-pinned database access instead.
pub(super) struct ScenarioEnvAcpModel;

struct ScenarioEnvSecretBroker;

#[async_trait::async_trait]
impl awaken_run_executor_acp::SecretBroker for ScenarioEnvSecretBroker {
    async fn materialize(
        &self,
        _reference: &str,
    ) -> Result<Vec<u8>, awaken_run_executor_acp::SandboxError> {
        Err(awaken_run_executor_acp::SandboxError::unsupported(
            "scenario broker has no file credential",
        ))
    }

    async fn materialize_process(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_run_executor_acp::SandboxError> {
        if reference != "scenario-env://acp-credential" {
            return Err(awaken_run_executor_acp::SandboxError::not_found(
                "unknown scenario credential reference",
            ));
        }
        std::env::var("AWAKEN_ACP_LEASE_TOKEN")
            .ok()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .map(String::into_bytes)
            .ok_or_else(|| {
                awaken_run_executor_acp::SandboxError::unsupported("dev scenario has no credential")
            })
    }

    async fn write_back(
        &self,
        _target: awaken_run_executor_acp::SecretWritebackTarget<'_>,
        _bytes: Vec<u8>,
    ) -> Result<(), awaken_run_executor_acp::SandboxError> {
        Err(awaken_run_executor_acp::SandboxError::unsupported(
            "scenario credentials are read-only",
        ))
    }
}

#[async_trait::async_trait]
impl awaken_run_executor_acp::LaunchResolver for ScenarioEnvAcpModel {
    async fn model(
        &self,
        activation: &awaken_runtime_contract::activation::RunActivation,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<awaken_run_executor_acp::ResolvedModel, awaken_run_executor_acp::OpenError> {
        Ok(awaken_run_executor_acp::ResolvedModel::managed(
            std::env::var("AWAKEN_ACP_GATEWAY_URL")
                .ok()
                .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok())
                .ok_or_else(|| {
                    awaken_run_executor_acp::OpenError("dev scenario has no endpoint".into())
                })?,
            activation.effective_model_ref().to_string(),
            Some(awaken_run_executor_acp::ProcessSecretRequirement::new(
                "scenario-env://acp-credential",
            )),
            None,
        ))
    }

    fn secret_broker(&self) -> Option<Arc<dyn awaken_run_executor_acp::SecretBroker>> {
        Some(Arc::new(ScenarioEnvSecretBroker))
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        awaken_credential_contract::CredentialRealizationCapabilities {
            holders: [awaken_agent_contract::credential::PlaintextHolder::new(
                awaken_agent_contract::credential::PlaintextBoundary::Workload,
                awaken_agent_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources: [
                awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
            ]
            .into_iter()
            .collect(),
            realization_kinds: [
                awaken_credential_contract::CredentialRealizationKind::ProcessSecretEnvironment,
            ]
            .into_iter()
            .collect(),
            recipient_bound_envelopes: false,
            envelope_required: false,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        }
    }
}
