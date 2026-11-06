// @formal surface obligation=config.inference_access_resolved_once_at_publication
// @formal reason: Exact scenario-host binding is publication-time inference access resolution.
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

#[expect(
    clippy::disallowed_methods,
    reason = "the process composition root reads its host configuration from the environment"
)]
fn cargo_test_namespace_hand_path() -> Option<std::path::PathBuf> {
    select_test_namespace_hand_path(
        std::env::var_os("CARGO_BIN_EXE_awaken-sandbox"),
        std::env::var_os("CARGO_BIN_EXE_awaken"),
    )
}

/// The process inputs of a local test composition: a Local-tier deployment
/// whose Sessions run on this process's claim pool through `execution`.
pub(super) fn local_test_process_options(
    stores: &super::ProcessStores,
    execution: Box<dyn crate::ColocatedExecution>,
) -> super::ProcessStartup {
    let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
    let work = stores
        .coordinator
        .as_ref()
        .expect("local test startup owns Managed Execution")
        .environment_work
        .clone();
    let worker_directory = awaken_coordinator::test_worker_directory();
    super::ProcessStartup {
        // This fixed in-process fixture installs no exporter. Custom deployment
        // seams replace both values together before constructing either role.
        content_capture_ceiling: deployment.content_capture.level,
        trace_file_present: false,
        deployment: Some(deployment),
        namespace_hand_path: cargo_test_namespace_hand_path(),
        colocated_execution: Some(execution),
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
        _workspace: &awaken_agent_contract::ScopeId,
        selection: &awaken_agent_config::ModelSelection,
        candidates: &[awaken_runtime_contract::resolved::ModelBinding],
    ) -> Result<
        awaken_config_service::ResolvedPublicationModels,
        awaken_config_service::PublicationResolutionError,
    > {
        if let Some(authored) = selection.resolved() {
            // A pinned binding always names an executable backend (the binding
            // admission refuses an empty one), so only the provider identity
            // may be left to the host: an external executor owns its account.
            let matches_host = authored.identity().1 == self.binding.identity().1
                && (authored.identity().0.is_empty()
                    || authored.identity().0 == self.binding.identity().0)
                && authored.identity().2 == self.binding.identity().2;
            if !matches_host {
                return Err(format!(
                    "scenario host executor `{}` cannot publish model `{}`",
                    self.binding.identity().1,
                    authored.identity().1
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
        let sandbox = std::ffi::OsString::from("/published/hand/exact-companion");
        let directory = std::path::PathBuf::from("/final/debug");
        let awaken = directory
            .join(format!("awaken{}", std::env::consts::EXE_SUFFIX))
            .into_os_string();
        assert_eq!(
            select_test_namespace_hand_path(Some(sandbox.clone()), Some(awaken.clone())),
            Some(sandbox.into())
        );
        assert_eq!(
            select_test_namespace_hand_path(None, Some(awaken)),
            Some(directory.join(format!("awaken-sandbox{}", std::env::consts::EXE_SUFFIX)))
        );
        assert_eq!(select_test_namespace_hand_path(None, None), None);
    }
}
