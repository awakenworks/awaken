//! Exact host-executor publication resolution for deterministic deployments.
//!
//! Provider-backed and hosted deployments use their own injected resolvers.
//! This adapter exists only for the one-host-executor scenario path.

pub(super) fn local_test_process_options(stores: &super::ProcessStores) -> super::ProcessStartup {
    let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
    let work = stores
        .coordinator
        .as_ref()
        .expect("local test startup owns Managed Execution")
        .environment_work
        .clone();
    let worker_directory = awaken_coordinator::test_worker_directory();
    super::ProcessStartup {
        deployment: Some(deployment),
        worker_directory: Some(worker_directory.clone()),
        runtime_authority: None,
        worker_observations: Some(
            super::worker_observation_wiring::WorkerObservationWiring::local(worker_directory),
        ),
        worker_authenticator: Some(std::sync::Arc::new(
            awaken_worker_transport_security::HeaderWorkerAuthenticator,
        )),
        executable_agent_wiring: Some(
            super::executable_agent_registration::ExecutableAgentWiring::local(),
        ),
        executable_environment_wiring: Some(
            super::executable_environment_registration::ExecutableEnvironmentWiring::local(work)
                .expect("configure local executable Environment wiring"),
        ),
        ..Default::default()
    }
}

pub(super) struct ExactHostModelPublicationResolver {
    pub(super) binding: awaken_runtime_contract::resolved::ModelBinding,
}

#[async_trait::async_trait]
impl awaken_config_service::ModelPublicationResolver for ExactHostModelPublicationResolver {
    async fn resolve_models(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        selection: &awaken_agent_config::ModelSelection,
        candidates: &[awaken_runtime_contract::resolved::ModelBinding],
    ) -> Result<
        awaken_config_service::ResolvedPublicationModels,
        awaken_config_service::PublicationResolutionError,
    > {
        if let Some(authored) = selection.resolved() {
            let matches_host = authored.model_ref == self.binding.model_ref
                && (authored.provider_identity_ref.is_empty()
                    || authored.provider_identity_ref == self.binding.provider_identity_ref)
                && (authored.backend_ref.is_empty()
                    || authored.backend_ref == self.binding.backend_ref);
            if !matches_host {
                return Err(format!(
                    "scenario host executor `{}` cannot publish model `{}`",
                    self.binding.model_ref, authored.model_ref
                )
                .into());
            }
        }
        if !candidates.is_empty() {
            return Err("a single host executor cannot publish fallback candidates".into());
        }
        Ok(awaken_config_service::ResolvedPublicationModels::host(
            self.binding.clone(),
            Vec::new(),
            None,
            None,
        ))
    }
}
