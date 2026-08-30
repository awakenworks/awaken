//! Deterministic Dream scenario platform shared by Rust and TypeScript E2E.
//!
//! The scenario uses the production MemoryStore mounter and a scripted model that
//! performs a real output-file write through the ordinary tool loop.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::routing::get;

use crate::{SharedHost, build_router_and_host_with_model_publication_resolver, scenario_model};

/// Build the one Dream scenario router used by process-level SDK E2E.
pub fn build_dream_router() -> Router {
    build_dream_router_and_host().0
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
    let (host, resources) = platform.into_parts();
    let host = Arc::new(host);
    let router = crate::scenario_platform::mount_parts_with_model_publication_resolver(
        host, resources, resolver,
    );
    router.merge(dream_auxiliary_routes())
}

/// Return the same router plus its Host for focused cross-module assertions.
pub fn build_dream_router_and_host() -> (Router, Arc<SharedHost>) {
    // Keep the Dream lifecycle fixture deterministic by default, but route an
    // explicitly requested HTTP model through the same production provider
    // adapter as every other scenario. Dream used to bypass `scenario_model`,
    // so AWAKEN_MODEL_SOURCE=http silently kept the scripted executor.
    let (model, model_ref) = scenario_model(Arc::new(DreamScenarioModel), "claude-sonnet-5");
    let resolver = Arc::new(DreamScenarioModelResolver {
        model_ref: model_ref.clone(),
    });
    let (router, host) =
        build_router_and_host_with_model_publication_resolver(model, model_ref, resolver);
    (router.merge(dream_auxiliary_routes()), host)
}

fn dream_auxiliary_routes() -> Router {
    let capabilities = awaken_config_service::capabilities_router(
        awaken_runtime_host::authorable_tools(),
        awaken_runtime_host::platform_plugin_capabilities(),
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
impl awaken_session_contract::SessionModelPublicationResolver for DreamScenarioModelResolver {
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
                awaken_session_contract::SessionModelResolutionError::Invalid(format!(
                    "Dream scenario model `{}` / backend `{backend_ref}` does not match configured provider model `{}`",
                    target.model_id, self.model_ref
                )),
            );
        }
        Ok(awaken_session_contract::SessionModelPublication {
            primary: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new(
                    target
                        .provider_id
                        .clone()
                        .unwrap_or_else(|| "scenario".into()),
                    self.model_ref.clone(),
                    "genai",
                ),
            ),
            candidates: Vec::new(),
        })
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

        // The process-level SDK conformance scenario needs one deterministic
        // Running Dream so it can observe the prepared transcript and exercise
        // cancellation without racing this deliberately fast scripted model.
        // Delay only the first inference: the marker remains in conversation
        // history, so delaying every turn would turn one cause into an accidental
        // multi-turn timeout and obscure the cancellation behavior under test.
        if request
            .messages
            .last()
            .is_some_and(|message| message.role != Role::Tool)
            && request.messages.iter().any(|message| {
                awaken_runtime_host::block_text(&message.content)
                    .contains("[awaken-test:hold-dream-for-cancellation]")
            })
        {
            // Bounded below the Session terminal-quiescence deadline: a broken
            // cancellation delivery can still settle naturally instead of
            // stranding the process fixture forever.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }

        let is_dream = request.messages.iter().any(|message| {
            message.role == Role::User
                && awaken_runtime_host::block_text(&message.content).contains("[dream-job:")
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
