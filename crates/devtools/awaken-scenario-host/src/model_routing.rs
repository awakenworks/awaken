use std::sync::Arc;

use awaken_provider_genai::{AdapterKind, GenaiExecutor};
use awaken_runtime_contract::inference::InferenceExecutorMaterializer;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::ModelProvisioning;
use axum::Router;

use super::{
    LabelModel, anthropic_messages_executor, default_anthropic_compatible_model, resource_host,
};

/// Resolves the scenario model source once, returning its executor and advertised
/// reference. Tests default to the deterministic in-process executor; explicit
/// HTTP or Gemini modes use the same provider adapters as production.
pub fn scenario_model(
    in_process: Arc<dyn LlmExecutor>,
    default_ref: &str,
) -> (Arc<dyn LlmExecutor>, String) {
    match std::env::var("AWAKEN_MODEL_SOURCE").as_deref() {
        Ok("http") => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .or_else(|_| std::env::var("KIMI_API_KEY"))
                .expect("AWAKEN_MODEL_SOURCE=http requires ANTHROPIC_API_KEY");
            let base = std::env::var("ANTHROPIC_BASE_URL")
                .or_else(|_| std::env::var("KIMI_BASE_URL"))
                .expect("AWAKEN_MODEL_SOURCE=http requires ANTHROPIC_BASE_URL");
            let model = std::env::var("ANTHROPIC_MODEL")
                .or_else(|_| std::env::var("KIMI_MODEL"))
                .unwrap_or_else(|_| default_anthropic_compatible_model(&base).to_string());
            (anthropic_messages_executor(&base, key), model)
        }
        Ok("gemini") => {
            let key = std::env::var("GEMINI_API_KEY")
                .or_else(|_| std::env::var("GOOGLE_API_KEY"))
                .expect("AWAKEN_MODEL_SOURCE=gemini requires GEMINI_API_KEY/GOOGLE_API_KEY");
            let model =
                std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".to_string());
            (
                Arc::new(GenaiExecutor::from_resolved(AdapterKind::Gemini, None, key)),
                model,
            )
        }
        _ => (in_process, default_ref.to_string()),
    }
}

/// Maps a pinned model reference to the deterministic executor carrying that
/// label, while preserving the authoritative resolved route.
struct RouteProvider;

impl InferenceExecutorMaterializer for RouteProvider {
    fn materialize_pinned(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        if !matches!(candidate.provisioning(), ModelProvisioning::HostExecutor) {
            return None;
        }
        let model_ref = candidate.binding().model_ref.as_str();
        let labeled: Arc<dyn LlmExecutor> = match model_ref {
            "fast" => Arc::new(LabelModel("fast")),
            "slow" => Arc::new(LabelModel("slow")),
            "default" => Arc::new(LabelModel("default")),
            _ => return None,
        };
        // Over the real wire the label rides in the model name the session bound.
        // Keep the resolved route ref and only replace the deterministic executor.
        Some(scenario_model(labeled, model_ref).0)
    }
}

struct RoutePublicationResolver;

#[async_trait::async_trait]
impl awaken_session_contract::SessionModelPublicationResolver for RoutePublicationResolver {
    async fn resolve_session_model(
        &self,
        _workspace_id: &str,
        model_reference: &str,
    ) -> Result<
        awaken_session_contract::SessionModelPublication,
        awaken_session_contract::SessionModelResolutionError,
    > {
        let selection =
            awaken_config_service::parse_managed_model_id(model_reference).map_err(|error| {
                awaken_session_contract::SessionModelResolutionError::Invalid(error.to_string())
            })?;
        let (target, backend_ref) = selection.target().ok_or_else(|| {
            awaken_session_contract::SessionModelResolutionError::Invalid(
                "model-route requires an explicit model target".into(),
            )
        })?;
        if !matches!(target.model_id.as_str(), "fast" | "slow" | "default")
            || !matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(backend_ref),
                awaken_runtime_contract::resolved::Backend::Native
            )
        {
            return Err(
                awaken_session_contract::SessionModelResolutionError::Invalid(format!(
                    "model-route cannot realize model {} on backend {backend_ref}",
                    target.model_id
                )),
            );
        }
        let provider = target
            .provider_id
            .clone()
            .unwrap_or_else(|| "scenario".into());
        let candidate = |model: &str| {
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new(
                    provider.clone(),
                    model,
                    backend_ref,
                ),
            )
        };
        Ok(awaken_session_contract::SessionModelPublication {
            primary: candidate(&target.model_id),
            candidates: ["fast", "slow", "default"]
                .into_iter()
                .filter(|model| *model != target.model_id)
                .map(candidate)
                .collect(),
        })
    }
}

/// A router whose Session-scoped selection routes to distinct labeled
/// executors and whose Managed event schema remains strict (R1/R2/R5/R6).
/// `AWAKEN_MODEL_MODE=model-route`.
pub fn build_model_route_router() -> Router {
    let (default_model, _) = scenario_model(Arc::new(LabelModel("default")), "default");
    let candidate = |model: &str| {
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::new("scenario", model, "genai"),
        )
    };
    let publication = super::scenario_platform::fixed_host_model_publication(
        "assistant",
        candidate("default"),
        vec![candidate("fast"), candidate("slow")],
    );
    let platform = resource_host(default_model, "default")
        .map_host(|host| host.with_agent_publications(publication.clone()))
        .map_host(|host| host.with_inference_materializer(Arc::new(RouteProvider)));
    let (host, resources) = platform.into_parts();
    let host = Arc::new(host);
    let catalog = resources.authorities().resource_registry();
    let managed =
        awaken_coordinator::local_managed_state_with_agent_source_and_model_publication_resolver(
            host.clone(),
            catalog.clone(),
            publication,
            Arc::new(RoutePublicationResolver),
        );
    awaken_coordinator::mount_with_managed_and_resource_registry(host, managed, catalog)
}
