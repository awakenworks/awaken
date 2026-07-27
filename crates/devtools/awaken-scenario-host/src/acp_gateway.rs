//! Hermetic cloud-managed ACP gateway scenario assembly.

use std::sync::Arc;

use axum::Router;

use super::{
    EchoModel, FAKE_ACP_CLI, SharedHost, mount_with_host_backend_publication,
    scenario_host_acp_cli, scenario_model, scenario_storage_dir,
};

/// Drive the fake CLI through the projecting launch path with an explicit
/// scenario-only resolver. `AWAKEN_MODEL_MODE=acp-gateway` selects this router.
pub fn build_acp_gateway_router() -> Router {
    let store_dir = scenario_storage_dir();
    let (model, model_ref) = scenario_model(Arc::new(EchoModel), "awaken");
    mount_with_host_backend_publication(
        SharedHost::new(model, model_ref).with_projected_acp(
            scenario_host_acp_cli(FAKE_ACP_CLI),
            Arc::new(ScenarioEnvAcpModel),
            store_dir,
        ),
        "acp-agent",
        "acp:fake",
    )
}

/// Explicit environment fixture for dev-only live scenarios. Product composition
/// uses publication-pinned database access instead.
pub(super) struct ScenarioEnvAcpModel;

struct ScenarioEnvSecretBroker;

#[async_trait::async_trait]
impl awaken_run_executor_acp::SecretBroker for ScenarioEnvSecretBroker {
    async fn materialize(
        &self,
        _reference: &str,
    ) -> Result<Vec<u8>, awaken_run_executor_acp::SandboxError> {
        Err(awaken_run_executor_acp::SandboxError::new(
            "scenario broker has no file credential",
        ))
    }

    async fn materialize_process(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_run_executor_acp::SandboxError> {
        if reference != "scenario-env://acp-credential" {
            return Err(awaken_run_executor_acp::SandboxError::new(
                "unknown scenario credential reference",
            ));
        }
        std::env::var("AWAKEN_ACP_LEASE_TOKEN")
            .ok()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .map(String::into_bytes)
            .ok_or_else(|| {
                awaken_run_executor_acp::SandboxError::new("dev scenario has no credential")
            })
    }

    async fn write_back(
        &self,
        _reference: &str,
        _bytes: Vec<u8>,
    ) -> Result<(), awaken_run_executor_acp::SandboxError> {
        Err(awaken_run_executor_acp::SandboxError::new(
            "scenario credentials are read-only",
        ))
    }
}

impl awaken_run_executor_acp::LaunchResolver for ScenarioEnvAcpModel {
    fn model(
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
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources: [
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            ]
            .into_iter()
            .collect(),
            realization_kinds: [
                awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment,
            ]
            .into_iter()
            .collect(),
            recipient_bound_envelopes: false,
            alternatives: Vec::new(),
        }
    }
}
