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

/// Whether this scenario process configures its model through ambient
/// provider-key variables. The shipped Worker refuses them, so such a process
/// keeps its Sessions on its own claim pool until its e2e author provider
/// connections through the config-plane API (w5-slices.md §11). Every other
/// scenario process runs the shipped co-located registered Worker.
pub fn scenario_requires_local_placement() -> bool {
    awaken_deployment_config::reject_ambient_api_key_environment().is_err()
}

/// The co-located registered Worker of a volatile test process: it reads the
/// process's own credential stores and, when the process publishes one
/// deterministic host executor, materializes exactly that executor.
#[cfg(feature = "test-support")]
pub fn colocated_test_worker(
    inputs: awaken_process_startup::ColocatedTestWorkerInputs,
) -> Arc<dyn awaken_process_startup::ColocatedWorker> {
    let mut worker = crate::PreparedLocalWorker::over_stores(inputs.stores);
    if let Some(inference) = inputs.host_inference {
        worker = worker.with_inference_materializer(inference);
    }
    Arc::new(worker)
}

/// Start an AllInOne test process whose Sessions run on its co-located
/// registered Worker over loopback. `model` publishes one deterministic host
/// executor under its model reference; `None` resolves publications from the
/// authored catalog.
#[cfg(feature = "test-support")]
pub async fn start_test_all_in_one(
    stores: awaken_process_startup::ColocatedTestStores<'_>,
    model: Option<(Arc<dyn awaken_runtime_contract::llm::LlmExecutor>, &str)>,
) -> awaken_process_startup::ColocatedTestProcess {
    let model = model.map(|(executor, model_ref)| {
        let binding =
            awaken_runtime_contract::resolved::ModelBinding::parse("default", model_ref, "genai")
                .expect("a deterministic test model reference is a valid native binding");
        (executor, binding)
    });
    awaken_process_startup::start_all_in_one_with_worker(stores, model, colocated_test_worker)
        .await
        .expect("start the AllInOne test process with its co-located Worker")
}

/// The launcher's own co-located Worker preparation, with the Worker-side
/// inference a deterministic scenario process publishes.
#[cfg(feature = "test-support")]
pub fn scenario_worker_preparation(
    inference: Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
) -> Arc<dyn awaken_process_startup::ColocatedWorkerPreparation> {
    Arc::new(crate::LocalWorkerPreparation { inference })
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
