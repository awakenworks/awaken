//! Production startup closure: a Deployment run creates and drives a real
//! Session through the same application service as `/v1/sessions`, and the
//! Sessions of the shipped co-located registered Worker share their
//! Environment and the process's Web provider catalog.

use std::sync::Arc;

use awaken_protocol_managed_testkit::DurableFact;
use awaken_scenario_host::EchoModel;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread")]
async fn deployment_run_creates_session_and_executes_initial_events() {
    // Production-startup cause/effect rules:
    // C1 published Agent + valid Deployment -> Agent latest version is frozen;
    // C2 manual run -> ordinary Session with Deployment id;
    // C3 initial user Event -> ordinary Event executor commits the model response.
    // C4 a projection read may observe the exact committed-prefix conflict;
    // C5 lifecycle admission reconciles Resources on the default Tokio worker
    // stack. Effects: E1-E3 are the respective durable results above; E4 retries
    // only the unchanged GET; E5 the complete production recovery/admission chain
    // returns without aborting its worker for stack exhaustion. K1 the sole
    // lifecycle recovery cycle enters
    // through a production task boundary, never a test-only stack override; K2
    // C4 retries only the unchanged GET. D1: C1+C2+C3+C4+C5 -> E1+E2+E3+E4+E5.
    // The deterministic router owns an explicit
    // local sandbox, independent of optional host capabilities such as bwrap.
    // Sessions run on the co-located registered Worker over loopback, the
    // topology the `awaken` launcher ships.
    let process = awaken_cli::start_test_all_in_one(
        awaken_process_startup::ColocatedTestStores::Volatile,
        Some((Arc::new(EchoModel), "echo")),
    )
    .await;
    let app = process.public_router.clone();
    let (status, agent) = call(
        &app,
        "POST",
        "/v1/agents",
        Some(json!({"name":"release-agent", "model":"echo"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C1: {agent}");
    let agent_id = agent["id"].as_str().unwrap();
    let (status, deployment) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent": agent_id,
            "environment_id": "env_local",
            "name": "release-check",
            "initial_events": [{
                "type": "user.message",
                "content": [{ "type": "text", "text": "verify release" }]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C1: {deployment}");
    assert_eq!(deployment["agent"]["version"], agent["version"], "C1");

    let deployment_id = deployment["id"].as_str().unwrap();
    let (status, run) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/run"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C2");
    assert!(run["error"].is_null(), "launch failed: {run}");
    let session_id = run["session_id"]
        .as_str()
        .expect("a successful deployment run returns its Session");

    let (status, session) = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "C2");
    assert_eq!(session["deployment_id"], deployment_id);

    let uri = format!("/v1/sessions/{session_id}/events");
    let observed = awaken_protocol_managed_testkit::wait_for_session_events(
        DurableFact::committed_event("agent.message"),
        std::time::Duration::from_secs(10),
        "Deployment initial Event response",
        || async {
            let (status, events) = call(&app, "GET", &uri, None).await;
            (status.as_u16(), events)
        },
        |events| {
            events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"] == "Echo: verify release"
            })
        },
    )
    .await;
    assert!(
        observed["data"].is_array(),
        "C3 initial Event eventually completes"
    );
    // The co-located Worker drains and deregisters before its private surface
    // stops; the process lifecycle reports no failed task.
    process
        .service_lifecycle
        .shutdown(std::time::Duration::from_secs(10))
        .await
        .expect("the AllInOne test process drains cleanly");
}

/// Echo, except a Run whose latest User text is [`UNKNOWN_OUTCOME`] returns
/// a provider error that carries no execution evidence from an opaque model:
/// its outcome is unknown, so the attempt slot stays occupied.
struct UnknownOutcomeModel;

const UNKNOWN_OUTCOME: &str = "return an unknown outcome";

#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for UnknownOutcomeModel {
    async fn infer(
        &self,
        request: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == awaken_agent_contract::agent::message::Role::User)
            .map(|message| awaken_session_executor::block_text(&message.content))
            .unwrap_or_default();
        if last_user == UNKNOWN_OUTCOME {
            return Err(awaken_runtime_contract::llm::Error::Provider(
                "the provider response could not be decoded".into(),
            ));
        }
        EchoModel.infer(request).await
    }
}

async fn create_session(app: &Router, agent_id: &str) -> String {
    let (status, session) = call(
        app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": agent_id,
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    session["id"].as_str().unwrap().to_string()
}

async fn send_text(app: &Router, session: &str, text: &str) -> Value {
    let (status, receipt) = call(
        app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        Some(json!({
            "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    receipt
}

async fn session_events(app: &Router, session: &str) -> Vec<Value> {
    let (status, list) = call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events?limit=500"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    list["data"].as_array().cloned().unwrap_or_default()
}

fn accepted_event_id(receipt: &Value) -> String {
    receipt["data"][0]["id"].as_str().unwrap().to_string()
}

/// One Managed Event read, paced so that a long wait stays inside the
/// process's Org Read bucket: an unpaced poll exhausts it within seconds and
/// fails the wait as a rate-limit error instead of naming the missing fact.
async fn paced_events(app: &Router, session: &str) -> (u16, Value) {
    // awaken-allow(wait): pace one read under the Org Read request bucket; the settle condition stays the DurableFact
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (status, events) = call(
        app,
        "GET",
        &format!("/v1/sessions/{session}/events?limit=500"),
        None,
    )
    .await;
    (status.as_u16(), events)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_outcome_never_holds_the_environment_against_other_sessions() {
    // Every Session of the built-in local Environment shares its one active
    // Work admission on the co-located registered Worker.
    // Causes: C1 Session A's Run ends with an unknown model outcome, so its
    // attempt slot stays occupied with no stop evidence; C2 Session B, in the
    // same Environment, is sent a message after C1; C3 Session A is sent
    // another message after C1.
    // Effects: E1 A publishes one terminal unknown-execution error and keeps
    // its input unprocessed; E2 B runs to its reply; E3 A's later input is not
    // executed: A's Thread stays fenced by the occupied slot, nothing is
    // retried and no quiescence is inferred from B's progress.
    // Rules: R1 C1 => E1; R2 C1+C2 => E2 (it used to wait forever, because A
    // kept the Environment's Work while its Run could never settle);
    // R3 C1+C3 => E3, the negative control: the fix releases the Environment,
    // never the slot.
    let process = awaken_cli::start_test_all_in_one(
        awaken_process_startup::ColocatedTestStores::Volatile,
        Some((Arc::new(UnknownOutcomeModel), "unknown-outcome")),
    )
    .await;
    let app = process.public_router.clone();
    let (status, agent) = call(
        &app,
        "POST",
        "/v1/agents",
        Some(json!({"name": "unknown-outcome-agent", "model": "unknown-outcome"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    let agent_id = agent["id"].as_str().unwrap();

    let retained = create_session(&app, agent_id).await;
    let receipt = send_text(&app, &retained, UNKNOWN_OUTCOME).await;
    let observed = awaken_protocol_managed_testkit::wait_for_session_events(
        DurableFact::committed_event_after(&receipt, "session.error"),
        std::time::Duration::from_secs(20),
        "R1 the unknown outcome is published as one terminal Session error",
        || paced_events(&app, &retained),
        |events| {
            events.iter().any(|event| {
                event["type"] == "session.error"
                    && event["error"]["retry_status"]["type"] == "terminal"
                    && event["error"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("execution result is unknown"))
            })
        },
    )
    .await;
    assert!(observed["data"].is_array(), "R1");
    let unknown_input = accepted_event_id(&receipt);

    let other = create_session(&app, agent_id).await;
    let receipt = send_text(&app, &other, "after the unknown outcome").await;
    let observed = awaken_protocol_managed_testkit::wait_for_session_events(
        DurableFact::command_processed(&receipt),
        std::time::Duration::from_secs(20),
        "R2 another Session of the same Environment runs to its reply",
        || paced_events(&app, &other),
        |events| {
            events.iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"] == "Echo: after the unknown outcome"
            })
        },
    )
    .await;
    assert!(observed["data"].is_array(), "R2");

    let later = accepted_event_id(&send_text(&app, &retained, "do not run this").await);
    // B already ran to completion after A's later input was accepted, so the
    // Environment is free; only A's own occupied slot can keep it waiting.
    let receipt = send_text(&app, &other, "the Environment stays free").await;
    awaken_protocol_managed_testkit::wait_for_session_events(
        DurableFact::command_processed(&receipt),
        std::time::Duration::from_secs(20),
        "R3 the Environment keeps admitting other Sessions",
        || paced_events(&app, &other),
        |_| true,
    )
    .await;
    let events = session_events(&app, &retained).await;
    for input in [&unknown_input, &later] {
        let event = events
            .iter()
            .find(|event| event["id"].as_str() == Some(input.as_str()))
            .unwrap_or_else(|| panic!("R3 accepted input {input} is listed: {events:?}"));
        assert!(
            event["processed_at"].is_null(),
            "R3 an input behind the occupied slot is never processed: {event}"
        );
    }
    assert!(
        !events.iter().any(|event| event["type"] == "agent.message"),
        "R3 nothing behind the unknown outcome executed: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "session.error")
            .count(),
        1,
        "R3 the unknown outcome is never retried: {events:?}"
    );
    let (status, session) = call(&app, "GET", &format!("/v1/sessions/{retained}"), None).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["status"], "running", "R3 Idle stays withheld");
    process
        .service_lifecycle
        .shutdown(std::time::Duration::from_secs(10))
        .await
        .expect("the AllInOne test process drains cleanly");
}

/// The one Cloud route the Web parity test publishes.
const CLOUD_SEARCH_ROUTE: &str = "web-search:cloud-parity";
const CLOUD_SEARCH_SNIPPET: &str = "CLOUD-SEARCH-ON-THE-WORKER";

/// Resolves the published Cloud route to a loopback Gateway, the way the
/// process's brokered client resolves it to the Awaken Cloud Gateway.
struct LoopbackCloudRoutes {
    gateway: String,
}

#[async_trait::async_trait]
impl awaken_ext_builtin_tools::ManagedWebRouteResolver for LoopbackCloudRoutes {
    async fn resolve(
        &self,
        _context: &awaken_tool_contract::tool::ToolOperationContext,
        tool_id: &str,
        route_ref: &str,
    ) -> Result<
        awaken_ext_builtin_tools::ManagedWebGatewayEndpoint,
        awaken_ext_builtin_tools::ManagedWebRouteError,
    > {
        if tool_id != awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID
            || route_ref != CLOUD_SEARCH_ROUTE
        {
            return Err(awaken_ext_builtin_tools::ManagedWebRouteError::Forbidden);
        }
        Ok(awaken_ext_builtin_tools::ManagedWebGatewayEndpoint {
            gateway_base_url: self.gateway.clone(),
            route_ref: route_ref.to_owned(),
            lease_token: awaken_agent_contract::RedactedString::new("cloud-parity-lease"),
        })
    }
}

/// A loopback Gateway answering the Cloud WebSearch route with one result.
async fn loopback_cloud_gateway() -> String {
    let app = axum::Router::new().route(
        "/http/{route}",
        axum::routing::get(|| async {
            axum::Json(json!({ "web": { "results": [{
                "title": "cloud parity",
                "url": "https://cloud.test/result",
                "description": CLOUD_SEARCH_SNIPPET,
            }]}}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

/// Requests the real `web_search` tool for `web-search <query>` and reports
/// the tool result it gets back, marking whether the runtime flagged it as an
/// error. It owns no Web policy or provider behavior.
struct WebSearchDrivingModel;

#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for WebSearchDrivingModel {
    async fn infer(
        &self,
        request: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        use awaken_agent_contract::agent::content::ContentBlock;
        use awaken_agent_contract::agent::message::Role;
        use awaken_runtime_contract::llm::{AssistantOutput, ChatResponse};
        if let Some(last) = request.messages.last()
            && last.role == Role::Tool
        {
            let failed = last
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }));
            let status = if failed { "error" } else { "result" };
            return Ok(ChatResponse {
                output: AssistantOutput::text(format!(
                    "web-{status}: {}",
                    awaken_session_executor::block_text(&last.content)
                )),
                usage: None,
                stop_reason: None,
            });
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| awaken_session_executor::block_text(&message.content))
            .unwrap_or_default();
        let output = match last_user.strip_prefix("web-search ") {
            Some(query) => {
                AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                    call_id: format!("web-search-{}", request.messages.len()),
                    tool_id: awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID.into(),
                    arguments: json!({ "query": query }),
                }])
            }
            None => AssistantOutput::text(format!("Echo: {last_user}")),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cloud_web_search_provider_reaches_the_registered_worker_run() {
    // Causes: C1 the process's Web catalog holds the Awaken Cloud WebSearch
    // provider, as its brokered client installs it; C2 an Agent publishes
    // `web_search` through that provider and a Cloud route; C3 a Session Run on
    // the co-located registered Worker calls the tool.
    // Effects: E1 Control validates and installs the publication against the
    // process catalog; E2 the Worker's Run resolves the same Cloud provider and
    // the routed Gateway's result reaches the model as a successful result.
    // Rules: R1 C1+C2 => E1; R2 C1+C2+C3 => E2 (the Worker used to build its
    // Runs from the built-in catalog alone, so the Cloud provider was missing
    // there although Control had accepted it).
    let gateway = loopback_cloud_gateway().await;
    let mut providers = awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins();
    providers
        .register(Arc::new(
            awaken_ext_builtin_tools::ManagedGatewayWebProvider::new(Arc::new(
                LoopbackCloudRoutes { gateway },
            )),
        ))
        .expect("the Cloud WebSearch provider joins the built-in catalog");
    let binding =
        awaken_runtime_contract::resolved::ModelBinding::parse("default", "cloud-web", "genai")
            .expect("a deterministic test model reference is a valid native binding");
    let process = awaken_process_startup::start_all_in_one_with_worker_and_web_providers(
        awaken_process_startup::ColocatedTestStores::Volatile,
        Some((Arc::new(WebSearchDrivingModel), binding)),
        Some(providers),
        awaken_cli::colocated_test_worker,
    )
    .await
    .expect("start the AllInOne test process with its co-located Worker");
    let app = process.public_router.clone();
    let definition = json!({
        "name": "Cloud Web search",
        "model": {
            "mode": "pinned",
            "provider_identity_ref": "default",
            "model_ref": "cloud-web",
            "backend_ref": "genai"
        },
        "system": "Execute the requested Web tool and report its result.",
        "max_steps": 4,
        "tools": [{
            "type": "agent_toolset_20260401",
            "configs": [{
                "name": "web_search",
                "enabled": true,
                "permission_policy": { "type": "always_allow" }
            }]
        }],
        "plugins": ["web_search"],
        "plugin_config": {
            "web_search": {
                "provider_id": awaken_ext_builtin_tools::AWAKEN_CLOUD_PROVIDER_ID,
                "options": { "route_ref": CLOUD_SEARCH_ROUTE }
            }
        }
    });
    let (status, body) = call(
        &app,
        "PUT",
        "/v1/config/agents/cloud-web-search",
        Some(definition),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1: {body}");
    let (status, body) = call(
        &app,
        "POST",
        "/v1/config/agents/cloud-web-search/publish",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1: {body}");
    assert_eq!(body["installed"], true, "R1: {body}");

    let session = create_session(&app, "cloud-web-search").await;
    let receipt = send_text(&app, &session, "web-search cloud parity").await;
    let observed = awaken_protocol_managed_testkit::wait_for_session_events(
        DurableFact::command_processed(&receipt),
        std::time::Duration::from_secs(20),
        "R2 the Worker Run reports the Cloud WebSearch result",
        || paced_events(&app, &session),
        |events| events.iter().any(|event| event["type"] == "agent.message"),
    )
    .await;
    let events = observed["data"].as_array().unwrap();
    let result = events
        .iter()
        .find(|event| event["type"] == "agent.tool_result")
        .unwrap_or_else(|| panic!("R2 one web_search result: {events:?}"));
    assert_eq!(result["is_error"], false, "R2: {result}");
    let reply = events
        .iter()
        .find(|event| event["type"] == "agent.message")
        .unwrap();
    let reply = reply["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        reply.starts_with("web-result:") && reply.contains(CLOUD_SEARCH_SNIPPET),
        "R2 the Cloud provider's result reached the model: {reply}"
    );
    process
        .service_lifecycle
        .shutdown(std::time::Duration::from_secs(10))
        .await
        .expect("the AllInOne test process drains cleanly");
}
