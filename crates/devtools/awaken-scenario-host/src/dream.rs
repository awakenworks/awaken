//! Deterministic Dream scenario platform shared by Rust and TypeScript E2E.
//!
//! The scenario uses the production MemoryStore mounter and a scripted model that
//! performs a real output-file write through the ordinary tool loop.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::routing::get;

use crate::{SharedHost, build_router_and_host_with_model_publication_resolver, scenario_model};
use awaken_dream_application::{
    DreamCancellation, DreamExecutor, DreamFailure, DreamPreparation, DreamRequest,
};

/// Build the one Dream scenario router used by process-level SDK E2E.
pub async fn build_dream_router() -> Router {
    build_dream_router_and_host().await.0
}

/// Process-level Dream route. Unlike the pure in-process fixture above, this
/// realizes the configured Session runtime tier so live Native/provider runs
/// exercise the same Namespace/Container mount semantics as production.
pub async fn build_dream_runtime_router() -> Router {
    let (model, model_ref) = scenario_model(Arc::new(DreamScenarioModel), "claude-sonnet-5");
    let resolver = Arc::new(DreamScenarioModelResolver {
        model_ref: model_ref.clone(),
    });
    let platform = crate::deployment::runtime_resource_host_with_deployment(
        model,
        model_ref,
        crate::scenario_deployment(),
    )
    .await;
    let (host, resources) = platform.into_dependencies();
    let barrier = std::env::var_os("AWAKEN_TEST_DREAM_PRE_EXECUTE_BARRIER_DIR").map(|directory| {
        Arc::new(move |inner: Arc<dyn DreamExecutor>| {
            Arc::new(PreparedDreamBarrier {
                inner,
                directory: PathBuf::from(&directory),
            }) as Arc<dyn DreamExecutor>
        }) as awaken_coordinator::ScenarioDreamExecutorDecorator
    });
    let router = crate::scenario_platform::mount_parts_with_model_publication_resolver(
        host, resources, resolver, barrier,
    )
    .await
    .0;
    router.merge(dream_auxiliary_routes())
}

/// Test-only decorator on the EXISTING DreamExecutor port. The real executor
/// owns preparation and cleanup; only a marked scenario waits before its first
/// call to execute. No second Dream store, Session path or model result exists.
struct PreparedDreamBarrier {
    inner: Arc<dyn DreamExecutor>,
    directory: PathBuf,
}

#[async_trait::async_trait]
impl DreamExecutor for PreparedDreamBarrier {
    async fn validate_inputs(&self, request: &DreamRequest) -> Result<(), DreamFailure> {
        self.inner.validate_inputs(request).await
    }

    async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure> {
        self.inner.prepare(request).await
    }

    async fn execute(
        &self,
        request: &DreamRequest,
        preparation: &DreamPreparation,
        cancellation: DreamCancellation,
    ) -> Result<(), DreamFailure> {
        if request
            .request_guidance
            .as_deref()
            .is_some_and(|guidance| guidance.contains("[awaken-test:pause-dream-before-execution]"))
        {
            // B1 prepared coordinates are committed before this method begins;
            // B2 the marker proves this decorator is entered; B3 cancel leaves
            // the real Session cleanup owner to finish. No model call can be
            // constructed before B3. Failure to cancel/release is bounded.
            if !request.job_id.starts_with("dream_")
                || !request.job_id[6..].bytes().all(|b| b.is_ascii_digit())
            {
                return Err(DreamFailure::new(
                    "fixture_invalid",
                    "invalid Dream fixture identity",
                ));
            }
            tokio::fs::create_dir_all(&self.directory)
                .await
                .map_err(|error| DreamFailure::new("fixture_io", error.to_string()))?;
            tokio::fs::write(
                self.directory.join(format!("{}.prepared", request.job_id)),
                b"ready",
            )
            .await
            .map_err(|error| DreamFailure::new("fixture_io", error.to_string()))?;
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                loop {
                    if cancellation.is_canceled() {
                        return Ok(());
                    }
                    if tokio::fs::try_exists(self.directory.join("release"))
                        .await
                        .map_err(|error| DreamFailure::new("fixture_io", error.to_string()))?
                    {
                        return Ok(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .map_err(|_| {
                DreamFailure::new("fixture_timeout", "prepared Dream was never cancelled")
            })??;
            if cancellation.is_canceled() {
                return Ok(());
            }
        }
        self.inner.execute(request, preparation, cancellation).await
    }

    async fn cleanup(
        &self,
        request: &DreamRequest,
        preparation: Option<&DreamPreparation>,
    ) -> Result<(), DreamFailure> {
        self.inner.cleanup(request, preparation).await
    }

    async fn cancel(
        &self,
        request: &DreamRequest,
        preparation: Option<&DreamPreparation>,
    ) -> Result<(), DreamFailure> {
        self.inner.cancel(request, preparation).await
    }
}

/// Return the same router plus its Host for focused cross-module assertions.
pub async fn build_dream_router_and_host() -> (Router, Arc<SharedHost>) {
    // Keep the Dream lifecycle fixture deterministic by default, but route an
    // explicitly requested HTTP model through the same production provider
    // adapter as every other scenario. Dream used to bypass `scenario_model`,
    // so AWAKEN_MODEL_SOURCE=http silently kept the scripted executor.
    let (model, model_ref) = scenario_model(Arc::new(DreamScenarioModel), "claude-sonnet-5");
    let resolver = Arc::new(DreamScenarioModelResolver {
        model_ref: model_ref.clone(),
    });
    let (router, host) =
        build_router_and_host_with_model_publication_resolver(model, model_ref, resolver).await;
    (router.merge(dream_auxiliary_routes()), host)
}

fn dream_auxiliary_routes() -> Router {
    let capabilities = awaken_config_service::capabilities_router(
        awaken_session_executor::authorable_tools(),
        awaken_session_executor::platform_plugin_capabilities(),
        vec![awaken_config_service::RuntimeCapability::native()],
    );
    let console_context = Router::new()
        .route(
            "/.well-known/awaken-suite-navigation",
            get(|| async { Json(serde_json::json!({ "hub_url": null })) }),
        )
        .route(
            "/v1/session",
            get(|| async { Json(serde_json::json!({ "authenticated": true })) }),
        )
        .route(
            "/v1/config/workspace-context",
            get(|| async { Json(serde_json::json!({ "workspace_id": "default" })) }),
        );
    capabilities.merge(console_context)
}

struct DreamScenarioModel;

struct DreamScenarioModelResolver {
    model_ref: String,
}

#[async_trait::async_trait]
impl awaken_session_configuration_contract::SessionModelPublicationResolver
    for DreamScenarioModelResolver
{
    async fn resolve_session_model(
        &self,
        _workspace_id: &str,
        model_reference: &str,
    ) -> Result<
        awaken_session_configuration_contract::SessionModelPublication,
        awaken_session_configuration_contract::SessionModelResolutionError,
    > {
        let selection =
            awaken_config_service::parse_managed_model_id(model_reference).map_err(|error| {
                awaken_session_configuration_contract::SessionModelResolutionError::Invalid(
                    error.to_string(),
                )
            })?;
        let (target, backend_ref) = selection.target().ok_or_else(|| {
            awaken_session_configuration_contract::SessionModelResolutionError::Invalid(
                "Dream scenario requires an explicit native model target".into(),
            )
        })?;
        if target.model_id != self.model_ref
            || !matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(backend_ref),
                awaken_runtime_contract::resolved::Backend::Native
            )
        {
            return Err(
                awaken_session_configuration_contract::SessionModelResolutionError::Invalid(
                    format!(
                        "Dream scenario model `{}` / backend `{backend_ref}` does not match configured provider model `{}`",
                        target.model_id, self.model_ref
                    ),
                ),
            );
        }
        let provider_identity_ref = target
            .provider_id
            .clone()
            .unwrap_or_else(|| "scenario".into());
        let binding = awaken_runtime_contract::resolved::ModelBinding::parse(
            &provider_identity_ref,
            self.model_ref.clone(),
            "genai",
        )
        .map_err(|error| {
            awaken_session_configuration_contract::SessionModelResolutionError::Invalid(format!(
                "Dream scenario provider `{provider_identity_ref}` / model `{}`: {error}",
                self.model_ref
            ))
        })?;
        Ok(
            awaken_session_configuration_contract::SessionModelPublication {
                primary: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding),
                candidates: Vec::new(),
            },
        )
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for DreamScenarioModel {
    async fn infer(
        &self,
        request: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        use awaken_agent_contract::agent::message::Role;
        use awaken_runtime_contract::llm::{AssistantOutput, ChatResponse, ToolCall};

        let is_dream = request.messages.iter().any(|message| {
            message.role == Role::User
                && awaken_session_executor::block_text(&message.content).contains("[dream-job:")
        });
        if !is_dream {
            return awaken_runtime_contract::llm::LlmExecutor::infer(&crate::EchoModel, request)
                .await;
        }
        if request
            .messages
            .last()
            .is_some_and(|message| message.role == Role::Tool)
        {
            return Ok(ChatResponse {
                output: AssistantOutput::text("Dream consolidation written."),
                usage: None,
                stop_reason: None,
            });
        }
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "dream-write-through-1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({
                    "path": "/mnt/dream/output-memory/MEMORY.md",
                    "content": "# Dream\n- Consolidated by the Dream Agent.\n"
                }),
            }]),
            usage: None,
            stop_reason: None,
        })
    }
}
