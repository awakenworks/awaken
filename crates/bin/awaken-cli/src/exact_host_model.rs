//! Exact host-executor publication resolution for deterministic deployments.
//!
//! Provider-backed and hosted deployments use their own injected resolvers.
//! This adapter exists only for the one-host-executor scenario path.

fn select_test_namespace_hand_path(
    sandbox: Option<std::ffi::OsString>,
    awaken: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    sandbox.map(Into::into).or_else(|| {
        awaken.map(|path| {
            std::path::PathBuf::from(path)
                .with_file_name(format!("awaken-sandbox{}", std::env::consts::EXE_SUFFIX))
        })
    })
}

fn cargo_test_namespace_hand_path() -> Option<std::path::PathBuf> {
    select_test_namespace_hand_path(
        std::env::var_os("CARGO_BIN_EXE_awaken-sandbox"),
        std::env::var_os("CARGO_BIN_EXE_awaken"),
    )
}

pub(super) fn local_test_process_options(stores: &super::ProcessStores) -> super::ProcessStartup {
    let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_session_executor::SandboxTier::Local;
    let work = stores
        .coordinator
        .as_ref()
        .expect("local test startup owns Managed Execution")
        .environment_work
        .clone();
    let worker_directory = awaken_coordinator::test_worker_directory();
    super::ProcessStartup {
        deployment: Some(deployment),
        namespace_hand_path: cargo_test_namespace_hand_path(),
        worker_directory: Some(worker_directory.clone()),
        runtime_authority: None,
        worker_observations: Some(
            super::worker_observation_wiring::WorkerObservationWiring::local(worker_directory),
        ),
        worker_transport: Some(super::worker_transport_security::WorkerTransport::unbound(
            std::sync::Arc::new(awaken_worker_transport_security::HeaderWorkerAuthenticator),
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

#[cfg(test)]
mod tests {
    use super::select_test_namespace_hand_path;

    #[test]
    fn cargo_test_companion_uses_exact_artifact_coordinates() {
        // Cause/effect table: C1 Cargo provides the sandbox binary -> use it
        // exactly; C2 only the package binary is provided -> use its installed
        // sibling; C3 neither exists -> preserve production resolution. R1 has
        // precedence over R2, and no intermediate/build-dir search is added.
        let sandbox = std::ffi::OsString::from("/final/debug/awaken-sandbox");
        let awaken = std::ffi::OsString::from("/final/debug/awaken");
        assert_eq!(
            select_test_namespace_hand_path(Some(sandbox.clone()), Some(awaken.clone())),
            Some(sandbox.into())
        );
        assert_eq!(
            select_test_namespace_hand_path(None, Some(awaken)),
            Some(std::path::PathBuf::from("/final/debug/awaken-sandbox"))
        );
        assert_eq!(select_test_namespace_hand_path(None, None), None);
    }
}
