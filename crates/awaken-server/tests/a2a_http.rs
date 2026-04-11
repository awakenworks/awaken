//! A2A HTTP integration tests for the current A2A v1.0 surface.

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, RunStore, ThreadRunStore, ThreadStore};
use awaken_contract::registry_spec::AgentSpec;
use awaken_contract::thread::Thread;
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::routes::build_router;
use awaken_stores::memory::InMemoryStore;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

struct ImmediateExecutor;

#[async_trait]
impl awaken_contract::contract::executor::LlmExecutor for ImmediateExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "immediate"
    }
}

struct DelayedExecutor;

#[async_trait]
impl LlmExecutor for DelayedExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "delayed"
    }
}

fn build_test_fixture<E>(
    agent_ids: &[&str],
    executor: Arc<E>,
    config: ServerConfig,
) -> (axum::Router, Arc<InMemoryStore>)
where
    E: LlmExecutor + 'static,
{
    let mut builder = AgentRuntimeBuilder::new()
        .with_model_binding(
            "test-model",
            ModelBinding {
                provider_id: "mock".into(),
                upstream_model: "mock-model".into(),
            },
        )
        .with_provider("mock", executor);

    for agent_id in agent_ids {
        builder = builder.with_agent_spec(AgentSpec {
            id: (*agent_id).to_string(),
            model_id: "test-model".into(),
            system_prompt: "test".into(),
            max_rounds: 0,
            ..Default::default()
        });
    }

    let store = Arc::new(InMemoryStore::new());
    builder = builder.with_thread_run_store(store.clone());
    let runtime = Arc::new(builder.build().expect("build runtime"));
    let mailbox_store = Arc::new(awaken_stores::InMemoryMailboxStore::new());
    let mailbox = Arc::new(awaken_server::mailbox::Mailbox::new(
        runtime.clone(),
        mailbox_store,
        "test".to_string(),
        awaken_server::mailbox::MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime.clone(),
        mailbox,
        store.clone(),
        runtime.resolver_arc(),
        config,
    );
    (build_router().with_state(state), store)
}

fn build_test_app<E>(agent_ids: &[&str], executor: Arc<E>, config: ServerConfig) -> axum::Router
where
    E: LlmExecutor + 'static,
{
    build_test_fixture(agent_ids, executor, config).0
}

fn make_test_app(agent_ids: &[&str]) -> axum::Router {
    build_test_app(
        agent_ids,
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    )
}

async fn request_json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }

    let req = req
        .body(match body {
            Some(body) => axum::body::Body::from(body.to_string()),
            None => axum::body::Body::empty(),
        })
        .expect("request build");

    let resp = app
        .clone()
        .oneshot(req)
        .await
        .expect("app should handle request");
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    let body = String::from_utf8(body.to_vec()).expect("utf-8");
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body).expect("valid json")
    };

    (status, json)
}

async fn request_text(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, String, String) {
    let mut req = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }

    let req = req
        .body(match body {
            Some(body) => axum::body::Body::from(body.to_string()),
            None => axum::body::Body::empty(),
        })
        .expect("request build");

    let resp = app
        .clone()
        .oneshot(req)
        .await
        .expect("app should handle request");
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body readable");
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).expect("utf-8"),
    )
}

fn send_message_payload(task_id: &str, context_id: &str, message_id: &str, text: &str) -> Value {
    json!({
        "message": {
            "taskId": task_id,
            "contextId": context_id,
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": [{"text": text}]
        }
    })
}

fn user_message_ids(task: &Value) -> Vec<&str> {
    task["history"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"].as_str() == Some("ROLE_USER"))
        .filter_map(|message| message["messageId"].as_str())
        .collect()
}

#[tokio::test]
async fn well_known_agent_card_returns_latest_shape() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = request_json(&app, "GET", "/.well-known/agent-card.json", &[], None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"].as_str(), Some("alpha"));
    assert_eq!(
        body["supportedInterfaces"][0]["url"].as_str(),
        Some("http://localhost/v1/a2a")
    );
    assert_eq!(
        body["supportedInterfaces"][0]["protocolBinding"].as_str(),
        Some("HTTP+JSON")
    );
    assert_eq!(
        body["supportedInterfaces"][0]["protocolVersion"].as_str(),
        Some("1.0")
    );
    assert_eq!(body["provider"]["organization"].as_str(), Some("Awaken"));
    assert_eq!(body["provider"]["url"].as_str(), Some("http://localhost"));
    assert_eq!(body["capabilities"]["streaming"].as_bool(), Some(true));
    assert_eq!(
        body["capabilities"]["pushNotifications"].as_bool(),
        Some(true)
    );
    assert_eq!(
        body["capabilities"]["extendedAgentCard"].as_bool(),
        Some(false)
    );
    assert!(
        body.get("url").is_none(),
        "top-level url must not be present"
    );
}

#[tokio::test]
async fn message_send_returns_task_wrapper_and_task_is_retrievable() {
    let app = make_test_app(&["alpha"]);
    let task_id = "task-latest-a2a";
    let context_id = "thread-latest-a2a";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(task_id, context_id, "msg-1", "hello")),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(context_id));
    assert_eq!(
        body["task"]["status"]["state"].as_str(),
        Some("TASK_STATE_COMPLETED")
    );

    let (status, task) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}?historyLength=10"),
        &[],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["id"].as_str(), Some(task_id));
    let history = task["history"].as_array().expect("history array");
    assert!(
        history.iter().any(|message| {
            message["messageId"].as_str() == Some("msg-1")
                && message["role"].as_str() == Some("ROLE_USER")
        }),
        "user message missing from history: {task}"
    );
}

#[tokio::test]
async fn tenant_message_send_is_visible_in_tenant_task_list() {
    let app = make_test_app(&["alpha"]);
    let task_id = "tenant-task-1";
    let context_id = "tenant-thread-1";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/alpha/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            task_id,
            context_id,
            "msg-tenant-1",
            "hello tenant",
        )),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(context_id));

    let (status, body) = request_json(&app, "GET", "/v1/a2a/alpha/tasks", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tasks"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tasks"][0]["id"].as_str(), Some(task_id));
    assert_eq!(body["tasks"][0]["contextId"].as_str(), Some(context_id));
}

#[tokio::test]
async fn shared_context_keeps_task_ids_distinct_and_histories_scoped() {
    let (app, store) = build_test_fixture(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    );
    let context_id = "thread-shared-context";

    let (status, first) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            "task-shared-1",
            context_id,
            "msg-shared-1",
            "first",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {first}");

    let (status, second) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            "task-shared-2",
            context_id,
            "msg-shared-2",
            "second",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {second}");

    let thread = store
        .load_thread(context_id)
        .await
        .expect("load thread")
        .expect("thread");
    let bindings = thread
        .metadata
        .custom
        .get("a2a.taskBindings")
        .expect("task bindings metadata");
    assert_eq!(
        bindings["tasks"]["task-shared-1"]["start_message_id"].as_str(),
        Some("msg-shared-1")
    );
    assert_eq!(
        bindings["tasks"]["task-shared-1"]["end_message_id"].as_str(),
        Some("msg-shared-2")
    );
    assert!(
        bindings["tasks"]["task-shared-1"]
            .get("history_start")
            .is_none(),
        "task binding should use message id cursor only"
    );
    assert!(
        bindings["tasks"]["task-shared-1"]
            .get("history_end")
            .is_none(),
        "task binding should use message id cursor only"
    );

    let (status, first_task) = request_json(
        &app,
        "GET",
        "/v1/a2a/tasks/task-shared-1?historyLength=10",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_task["contextId"].as_str(), Some(context_id));
    assert_eq!(user_message_ids(&first_task), vec!["msg-shared-1"]);

    let (status, second_task) = request_json(
        &app,
        "GET",
        "/v1/a2a/tasks/task-shared-2?historyLength=10",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second_task["contextId"].as_str(), Some(context_id));
    assert_eq!(user_message_ids(&second_task), vec!["msg-shared-2"]);

    let (status, tasks) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks?contextId={context_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task_ids = tasks["tasks"]
        .as_array()
        .expect("tasks array")
        .iter()
        .filter_map(|task| task["id"].as_str())
        .collect::<Vec<_>>();
    assert!(task_ids.contains(&"task-shared-1"));
    assert!(task_ids.contains(&"task-shared-2"));
}

#[tokio::test]
async fn waiting_task_id_resumes_the_same_task() {
    let (app, store) = build_test_fixture(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig::default(),
    );
    let task_id = "task-resume";
    let context_id = "thread-resume";
    let existing_messages = vec![Message::user("first turn").with_id("msg-initial".to_string())];
    let waiting_run = RunRecord {
        run_id: task_id.to_string(),
        thread_id: context_id.to_string(),
        agent_id: "alpha".to_string(),
        parent_run_id: None,
        status: RunStatus::Waiting,
        termination_code: Some("input_required".to_string()),
        created_at: 1,
        updated_at: 1,
        steps: 1,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    };
    store
        .save_thread(&Thread::with_id(context_id))
        .await
        .expect("save thread");
    store
        .checkpoint(context_id, &existing_messages, &waiting_run)
        .await
        .expect("seed waiting run");

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(send_message_payload(
            task_id,
            context_id,
            "msg-resume",
            "resumed input",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");
    assert_eq!(body["task"]["id"].as_str(), Some(task_id));
    assert_eq!(body["task"]["contextId"].as_str(), Some(context_id));

    let resumed_run = store
        .load_run(task_id)
        .await
        .expect("load resumed run")
        .expect("resumed run present");
    assert_eq!(resumed_run.run_id, task_id);
    assert_eq!(resumed_run.thread_id, context_id);
    assert_eq!(resumed_run.status, RunStatus::Done);

    let (status, task) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}?historyLength=10"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(user_message_ids(&task), vec!["msg-initial", "msg-resume"]);
}

#[tokio::test]
async fn message_stream_returns_sse_updates() {
    let app = make_test_app(&["alpha"]);
    let (status, content_type, body) = request_text(
        &app,
        "POST",
        "/v1/a2a/message:stream",
        &[("content-type", "application/a2a+json")],
        Some(send_message_payload(
            "task-stream",
            "thread-stream",
            "msg-stream",
            "hello",
        )),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.contains("text/event-stream"),
        "unexpected content type: {content_type}"
    );
    assert!(body.contains("\"task\""), "missing task payload: {body}");
    assert!(
        body.contains("TASK_STATE_COMPLETED") || body.contains("TASK_STATE_WORKING"),
        "missing task state in stream: {body}"
    );
}

#[tokio::test]
async fn subscribe_stream_returns_updates_for_existing_task() {
    let app = build_test_app(
        &["alpha"],
        Arc::new(DelayedExecutor),
        ServerConfig::default(),
    );
    let task_id = "task-subscribe";
    let context_id = "thread-subscribe";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": task_id,
                "contextId": context_id,
                "messageId": "msg-subscribe",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            },
            "configuration": {
                "returnImmediately": true
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let (status, content_type, body) = request_text(
        &app,
        "POST",
        &format!("/v1/a2a/tasks/{task_id}:subscribe"),
        &[("content-type", "application/json")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    assert!(
        body.contains("\"task\""),
        "missing initial task event: {body}"
    );
    assert!(
        body.contains("\"statusUpdate\""),
        "missing status update event: {body}"
    );
    assert!(body.contains("TASK_STATE_COMPLETED"));
}

#[tokio::test]
async fn push_notification_configs_roundtrip_and_inline_delivery_work() {
    use axum::{Json, Router, routing::post};
    use tokio::sync::oneshot;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind webhook listener");
    let webhook_addr = listener.local_addr().expect("local addr");
    let webhook_url = format!("http://{webhook_addr}/notify");
    let (tx, rx) = oneshot::channel::<Value>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let webhook = Router::new().route(
        "/notify",
        post({
            let tx = Arc::clone(&tx);
            move |Json(payload): Json<Value>| {
                let tx = Arc::clone(&tx);
                async move {
                    if let Some(sender) = tx.lock().expect("tx mutex").take() {
                        let _ = sender.send(payload.clone());
                    }
                    Json(json!({"ok": true}))
                }
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, webhook).await.expect("serve webhook");
    });

    let app = make_test_app(&["alpha"]);
    let task_id = "task-push";
    let context_id = "thread-push";

    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": task_id,
                "contextId": context_id,
                "messageId": "msg-push",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            },
            "configuration": {
                "pushNotificationConfig": {
                    "url": webhook_url,
                    "token": "push-token"
                }
            }
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "unexpected body: {body}");

    let delivered = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("webhook delivery timed out")
        .expect("webhook payload should be delivered");
    assert!(
        delivered.get("statusUpdate").is_some() || delivered.get("artifactUpdate").is_some(),
        "unexpected webhook payload: {delivered}"
    );

    let (status, list) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}/pushNotificationConfigs"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let config_id = list["configs"][0]["id"]
        .as_str()
        .expect("config id")
        .to_string();

    let (status, cfg) = request_json(
        &app,
        "GET",
        &format!("/v1/a2a/tasks/{task_id}/pushNotificationConfigs/{config_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cfg["taskId"].as_str(), Some(task_id));

    let (status, _content_type, deleted) = request_text(
        &app,
        "DELETE",
        &format!("/v1/a2a/tasks/{task_id}/pushNotificationConfigs/{config_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(deleted.is_empty());
}

#[tokio::test]
async fn extended_agent_card_requires_bearer_auth_when_configured() {
    let app = build_test_app(
        &["alpha"],
        Arc::new(ImmediateExecutor),
        ServerConfig {
            a2a_extended_card_bearer_token: Some("secret-token".into()),
            ..Default::default()
        },
    );

    let (status, body) = request_json(&app, "GET", "/.well-known/agent-card.json", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["capabilities"]["extendedAgentCard"].as_bool(),
        Some(true)
    );

    let (status, body) = request_json(&app, "GET", "/v1/a2a/extendedAgentCard", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["status"].as_str(), Some("UNAUTHENTICATED"));

    let (status, body) = request_json(
        &app,
        "GET",
        "/v1/a2a/extendedAgentCard",
        &[("authorization", "Bearer secret-token")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"].as_str(), Some("alpha"));
}

#[tokio::test]
async fn unsupported_version_returns_failed_precondition_error() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = request_json(
        &app,
        "GET",
        "/.well-known/agent-card.json",
        &[("a2a-version", "0.9")],
        None,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"]["details"][0]["reason"].as_str(),
        Some("VERSION_NOT_SUPPORTED")
    );
    assert_eq!(
        body["error"]["details"][0]["metadata"]["requestedVersion"].as_str(),
        Some("0.9")
    );
}

#[tokio::test]
async fn invalid_inbound_message_role_is_rejected() {
    let app = make_test_app(&["alpha"]);
    let (status, body) = request_json(
        &app,
        "POST",
        "/v1/a2a/message:send",
        &[("content-type", "application/json")],
        Some(json!({
            "message": {
                "taskId": "task-invalid-role",
                "contextId": "task-invalid-role",
                "messageId": "msg-invalid-role",
                "role": "ROLE_AGENT",
                "parts": [{"text": "hello"}]
            }
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["status"].as_str(), Some("INVALID_ARGUMENT"));
    assert_eq!(
        body["error"]["details"][0]["fieldViolations"][0]["field"].as_str(),
        Some("message.role")
    );
}
