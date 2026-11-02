// @formal surface requirement=model.routing.exact_binding
// @formal reason: The original co-located model factory selects its original concrete realization;
// @formal reason: publication remains Control-owned and no alternative routing law is added.
//! Runtime realization selected only by the co-located execution root.
use awaken_process_startup::InferenceSelection;
use awaken_runtime_contract::llm::LlmExecutor;
use std::sync::Arc;

pub(super) struct RuntimeModelWiring {
    pub(super) executor: Arc<dyn LlmExecutor>,
    pub(super) model_ref: String,
    pub(super) materializer:
        Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
}

pub(super) fn runtime_model_wiring(
    runtime: InferenceSelection,
    credential_materializer: Option<&awaken_credential_materializer::PinnedCredentialMaterializer>,
    cloud_models_enabled: bool,
    brokered_client: Option<
        &Arc<dyn awaken_inference_endpoint::brokered_inference::BrokeredInferenceClient>,
    >,
) -> RuntimeModelWiring {
    runtime.resolve_with(
        || RuntimeModelWiring {
            executor: Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            model_ref: awaken_dispatch_execution::session_dispatch::UNCONFIGURED_MODEL_REF
                .to_string(),
            materializer: credential_materializer.map(|credential_materializer| {
                Arc::new({
                    let materializer =
                    awaken_credential_materializer::CredentialInferenceMaterializer::from_pinned(
                        credential_materializer.clone(),
                    )
                    .with_brokered_mode(cloud_models_enabled);
                    match brokered_client {
                        Some(client) => materializer.with_brokered_client(client.clone()),
                        None => materializer,
                    }
                })
                    as Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>
            }),
        },
        || RuntimeModelWiring {
            executor: Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            model_ref: awaken_dispatch_execution::session_dispatch::UNCONFIGURED_MODEL_REF
                .to_string(),
            materializer: None,
        },
        |executor, model_ref| RuntimeModelWiring {
            executor,
            model_ref,
            materializer: None,
        },
    )
}
