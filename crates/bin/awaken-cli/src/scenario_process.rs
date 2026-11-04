//! Scenario-only SDK customization, owned by the launcher that links execution.
use awaken_session_executor::SessionExecutorBuilder;
use std::sync::Arc;

fn customized_execution(
    deployment: &awaken_deployment_config::DeploymentConfig,
    customize: impl FnOnce(SessionExecutorBuilder) -> SessionExecutorBuilder + Send + 'static,
) -> Result<Box<dyn awaken_process_startup::ColocatedExecution>, String> {
    if deployment.disable_local_pool {
        return Err(
            "a registered-Worker Coordinator cannot install co-located executor customization"
                .into(),
        );
    }
    Ok(Box::new(
        crate::LocalExecution::default().with_host_customizer(customize),
    ))
}

#[cfg(feature = "test-support")]
pub async fn build_all_in_one_service_with_host_customizer(
    model: Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    deployment: awaken_deployment_config::DeploymentConfig,
    web_search: Option<awaken_ext_builtin_tools::WebSearchProviderRegistry>,
    customize: impl FnOnce(SessionExecutorBuilder) -> SessionExecutorBuilder + Send + 'static,
    trace_file_present: bool,
) -> (axum::Router, awaken_service_lifecycle::ServiceLifecycle) {
    let execution = customized_execution(&deployment, customize)
        .expect("prepare test-support customized execution");
    awaken_process_startup::build_all_in_one_service_with_execution_inputs(
        model,
        binding,
        deployment,
        web_search,
        execution,
        trace_file_present,
    )
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn registered_process_refuses_executor_customization_before_execution() {
        // Causes: C1 registered Worker deployment; C2 an SDK customizer is
        // supplied. R1=C1+C2 => same constructor refusal, zero customization
        // calls; shared startup never receives the SDK callback. Local backend
        // scenarios retain their original positive execution assertions.
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let witness = invoked.clone();
        let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
        deployment.disable_local_pool = true;
        let error = super::customized_execution(&deployment, move |host| {
            witness.store(true, std::sync::atomic::Ordering::SeqCst);
            host
        })
        .err()
        .expect("registered placement refuses SDK customization");
        assert!(error.contains("registered-Worker Coordinator"));
        assert!(!invoked.load(std::sync::atomic::Ordering::SeqCst));
    }
}
