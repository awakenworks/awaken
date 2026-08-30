//! End-to-end BackgroundTask completion attention through the product
//! Coordinator composition. The deterministic case runs in CI; the ignored
//! case swaps only the model executor for a live configured provider endpoint.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::content::extract_text;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_runtime_contract::agent_bindings::{
    AgentBindings, ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride,
    ToolsetPolicy, ToolsetSource,
};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_scenario_host::build_router_and_host_with_agent_publications;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const AGENT_ID: &str = "background-attention-agent";
const OBSERVED: &str = "BACKGROUND_COMPLETION_OBSERVED";

fn task_id_from_attention(request: &ChatRequest) -> Option<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(|message| extract_text(&message.content))
        .find_map(|text| {
            text.split_once("Background task ")
                .and_then(|(_, suffix)| suffix.split_whitespace().next())
                .map(str::to_string)
        })
}

struct DeterministicBackgroundAttentionModel;

#[async_trait::async_trait]
impl LlmExecutor for DeterministicBackgroundAttentionModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let visible = request
            .tools
            .iter()
            .map(|tool| tool.id.as_str())
            .collect::<Vec<_>>();
        assert!(
            visible.iter().all(|id| {
                matches!(
                    *id,
                    "run_in_background"
                        | "get_background_task"
                        | "list_background_tasks"
                        | "cancel_background_task"
                )
            }),
            "the Agent sees only BackgroundTask tools, got {visible:?}"
        );
        assert!(
            !visible.contains(&"bash"),
            "configured target stays internal"
        );
        let last_role = request.messages.last().map(|message| message.role);
        let attention_task = task_id_from_attention(&request);
        let output = match (attention_task, last_role) {
            (Some(task_id), Some(Role::System)) => {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "inspect-background-completion".into(),
                    tool_id: "get_background_task".into(),
                    arguments: json!({"task_id": task_id}),
                }])
            }
            (Some(_), Some(Role::Tool)) => AssistantOutput::text(OBSERVED),
            (None, Some(Role::Tool)) => AssistantOutput::text("BACKGROUND_STARTED"),
            (None, _) => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "start-background-bash".into(),
                tool_id: "run_in_background".into(),
                arguments: json!({
                    "tool": "bash",
                    "arguments": {"command": "printf 'BACKGROUND_E2E_RESULT\\n'"}
                }),
            }]),
            (Some(_), _) => AssistantOutput::text(OBSERVED),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

fn background_snapshot(model_ref: &str) -> ExecutableAgentSnapshot {
    let ordinary_tools = awaken_runtime_host::authorable_tools()
        .into_iter()
        .filter(|tool| tool.id == "bash")
        .collect::<Vec<_>>();
    let always_allow = ToolExecutionPolicy {
        enabled: true,
        permission: ToolPermissionRequirement::AlwaysAllow,
    };
    let disabled = ToolExecutionPolicy {
        enabled: false,
        permission: ToolPermissionRequirement::AlwaysAllow,
    };
    ExecutableAgentSnapshot::builder(AGENT_ID)
        .instructions(
            "For the first user request, call run_in_background exactly once with the requested \
             bash command, then end that Run. When a System message reports a terminal background \
             completion candidate, call get_background_task with its task_id exactly once. After \
             reading the tool result, reply with BACKGROUND_COMPLETION_OBSERVED. Never call bash \
             directly and never invent a task result.",
        )
        .model(ModelBinding::new("background-e2e", model_ref, "default"))
        .tools(ordinary_tools)
        .plugins([awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string()])
        .plugin_config([(
            awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string(),
            json!({"tools": ["bash"]}),
        )])
        .agent_bindings(AgentBindings {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: Default::default(),
                overrides: vec![
                    ToolPolicyOverride::new("run_in_background", always_allow),
                    ToolPolicyOverride::new("get_background_task", always_allow),
                    ToolPolicyOverride::new("bash", always_allow),
                    ToolPolicyOverride::new("list_background_tasks", disabled),
                    ToolPolicyOverride::new("cancel_background_task", disabled),
                ],
            }],
            ..Default::default()
        })
        .max_steps(8)
        .build()
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> Value {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).expect("serialize request"))
        })
        .expect("build request");
    let response = app.clone().oneshot(request).await.expect("route request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect response")
        .to_bytes();
    let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|error| {
        panic!(
            "{method} {uri} returned non-JSON ({error}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {value}");
    value
}

fn event_text(event: &Value) -> String {
    event["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect()
}

fn completion_is_observed(events: &[Value]) -> bool {
    let Some(observed) = events
        .iter()
        .position(|event| event["type"] == "agent.message" && event_text(event).contains(OBSERVED))
    else {
        return false;
    };
    events[observed..]
        .iter()
        .any(|event| event["type"] == "session.status_idle")
}

async fn wait_for_completion(app: &Router, session_id: &str, timeout: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    let uri = format!("/v1/sessions/{session_id}/events?limit=500");
    loop {
        let events = call(app, "GET", &uri, Value::Null).await;
        if events["data"]
            .as_array()
            .is_some_and(|events| completion_is_observed(events))
        {
            return events;
        }
        if let Some(error) = events["data"].as_array().and_then(|events| {
            events
                .iter()
                .rev()
                .find(|event| event["type"] == "session.error")
        }) {
            panic!(
                "model-backed Session failed before completion attention: {}",
                error["error"]
            );
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for BackgroundTask completion attention: {events}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn exercise_background_attention(
    model: Arc<dyn LlmExecutor>,
    model_ref: &str,
    timeout: Duration,
) -> Value {
    let (app, host) = build_router_and_host_with_agent_publications(
        model,
        model_ref,
        [background_snapshot(model_ref)],
    );
    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        json!({
            "agent": AGENT_ID,
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await["id"]
        .as_str()
        .expect("Session id")
        .to_string();
    call(
        &app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        json!({"events": [{
            "type": "user.message",
            "content": [{
                "type": "text",
                "text": "Call run_in_background exactly once with tool=\"bash\" and arguments={\"command\":\"printf 'BACKGROUND_E2E_RESULT\\n'\"}. Do not call any other tool. After it starts, end this Run and wait for the System completion message."
            }]
        }]}),
    )
    .await;
    let events = wait_for_completion(&app, &session, timeout).await;
    assert!(
        host.drain_runtime(Duration::from_secs(2)).await,
        "BackgroundTask execution and attention publication drain"
    );
    events
}

fn assert_completion_chain(events: &Value) {
    let events = events["data"].as_array().expect("Managed Events");
    let tool_names = events
        .iter()
        .filter(|event| event["type"] == "agent.tool_use")
        .filter_map(|event| event["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        tool_names,
        ["run_in_background", "get_background_task"],
        "the Agent starts once and inspects only after the system wake"
    );
    let results = events
        .iter()
        .filter(|event| event["type"] == "agent.tool_result")
        .map(event_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(results.contains("\"state\":\"completed\""), "{results}");
    assert!(results.contains("BACKGROUND_E2E_RESULT"), "{results}");
}

#[tokio::test]
async fn background_completion_folds_before_inference_and_commits_before_next_effect() {
    // Cause/effect decision table:
    // R1 foreground committed Running task -> post-commit target executes once;
    // R2 fenced process completion -> deterministic System attention Run enters
    // the same Session; R3 StepStart sees the matching completion -> folds Ended
    // into the inference view, then the Requested get_background_task batch
    // commits that state before its tool effect; R4 attention settles -> Session
    // returns to Idle. A text-only attention response would commit at its normal
    // terminal boundary; no notification table or test-only commit path exists.
    let events = exercise_background_attention(
        Arc::new(DeterministicBackgroundAttentionModel),
        "background-scripted",
        Duration::from_secs(10),
    )
    .await;
    assert_completion_chain(&events);
}

#[tokio::test]
#[ignore = "hits a live model endpoint; run with KIMI_API_KEY, DEEPSEEK_API_KEY, or ANTHROPIC_API_KEY and --ignored"]
async fn live_model_observes_background_completion_through_the_system_attention_run() {
    // Live rule L1: the same R1-R4 chain above is driven by an external model
    // that must select both typed tools from their schemas. Constraint L0: the
    // frozen publication exposes only the BackgroundTask lifecycle tools; bash
    // remains executable solely behind their typed launcher schema, while
    // read-all/cancel and unrelated hand tools are disabled so the canary tests
    // this causal path rather than model exploration. This supplements, but
    // never replaces, deterministic lifecycle evidence. Provider/network failure
    // is an external test failure and creates no alternate Runtime path.
    let (adapter, key, base, model_ref) = match std::env::var("KIMI_API_KEY") {
        Ok(key) if !key.trim().is_empty() => (
            awaken_provider_genai::AdapterKind::Anthropic,
            key,
            std::env::var("KIMI_BASE_URL")
                .unwrap_or_else(|_| "https://api.kimi.com/coding/v1/".to_string()),
            std::env::var("KIMI_MODEL").unwrap_or_else(|_| "kimi-for-coding".to_string()),
        ),
        _ => match std::env::var("DEEPSEEK_API_KEY") {
            Ok(key) if !key.trim().is_empty() => (
                awaken_provider_genai::AdapterKind::OpenAI,
                key,
                "https://api.deepseek.com/v1".to_string(),
                std::env::var("AWAKEN_GENAI_MODEL")
                    .unwrap_or_else(|_| "deepseek-v4-flash".to_string()),
            ),
            _ => (
                awaken_provider_genai::AdapterKind::Anthropic,
                std::env::var("ANTHROPIC_API_KEY").expect(
                    "set KIMI_API_KEY, DEEPSEEK_API_KEY, or ANTHROPIC_API_KEY to run this test",
                ),
                std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com/v1/".to_string()),
                std::env::var("ANTHROPIC_MODEL")
                    .unwrap_or_else(|_| "claude-3-5-haiku-latest".to_string()),
            ),
        },
    };
    let model: Arc<dyn LlmExecutor> = Arc::new(
        awaken_provider_genai::GenaiExecutor::from_materialized_endpoint(adapter, base, key),
    );
    let events = exercise_background_attention(model, &model_ref, Duration::from_secs(90)).await;
    assert_completion_chain(&events);
}
