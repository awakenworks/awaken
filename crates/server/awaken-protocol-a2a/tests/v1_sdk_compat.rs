//! Wire checks against the official `@a2a-js/sdk@1.0.0-beta.0` transport.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_protocol_a2a::router;
use awaken_runtime_contract::runs::{Pending, RunError, RunResume, Runs, StepOutcome};
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Runtime;

#[async_trait]
impl Runs for Runtime {
    async fn run(
        &self,
        _operation_id: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }
    async fn resume(
        &self,
        _operation_id: &str,
        _: &str,
        _: &str,
        _: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn pending(&self, _: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }
    async fn history(&self, _: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }
    fn model(&self) -> String {
        "v1-compat".into()
    }
}

async fn call(
    app: Router,
    method: &str,
    uri: &str,
    body: Value,
    version: &str,
) -> (u16, axum::http::HeaderMap, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(
                    "content-type",
                    if version == "1.0" && uri != "/v1/a2a" {
                        "application/a2a+json"
                    } else {
                        "application/json"
                    },
                )
                .header("A2A-Version", version)
                .body(Body::from(if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn params(tenant: &str, context: &str) -> Value {
    json!({
        "tenant": tenant,
        "message": {
            "messageId": "v1-message", "contextId": context, "role": "ROLE_USER",
            "parts": [{ "text": "hello", "mediaType": "text/plain" }]
        }
    })
}

#[tokio::test]
async fn card_and_unknown_version_are_negotiated_explicitly() {
    let app = router(Arc::new(Runtime), awaken_time::system_clock());
    let (_, headers, body) = call(
        app.clone(),
        "GET",
        "/.well-known/agent-card.json",
        Value::Null,
        "1.0",
    )
    .await;
    let card: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(headers["vary"], "A2A-Version");
    assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "1.0");
    assert!(
        card.get("protocolVersion").is_none(),
        "v1 has per-interface versions"
    );

    let (_, _, body) = call(
        app,
        "POST",
        "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 9, "method": "SendMessage", "params": params("", "c") }),
        "2.0",
    )
    .await;
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["code"], -32009);
}

#[tokio::test]
async fn v1_pascal_case_send_stream_and_list_tasks_match_protojson() {
    let app = router(Arc::new(Runtime), awaken_time::system_clock());
    let (_, _, body) = call(
        app.clone(),
        "POST",
        "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": params("tenant-a", "context-a") }),
        "1.0",
    )
    .await;
    let sent: Value = serde_json::from_str(&body).unwrap();
    assert!(sent["result"]["task"].is_object(), "{sent}");
    assert!(sent["result"]["task"].get("kind").is_none());
    assert_eq!(
        sent["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );

    let (_, _, body) = call(
        app.clone(),
        "POST",
        "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "ListTasks", "params": { "tenant": "tenant-a", "pageSize": 50 } }),
        "1.0",
    )
    .await;
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["result"]["totalSize"], 1, "{listed}");

    let (_, headers, body) = call(
        app,
        "POST",
        "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 3, "method": "SendStreamingMessage", "params": params("tenant-a", "context-b") }),
        "1.0",
    )
    .await;
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let first = body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let first: Value = serde_json::from_str(first).unwrap();
    assert!(first["result"]["task"].is_object(), "{first}");
    assert!(first["result"]["task"].get("kind").is_none());
}

#[tokio::test]
async fn v1_flat_push_configuration_crud_is_tenant_fenced() {
    let app = router(Arc::new(Runtime), awaken_time::system_clock());
    let (_, _, body) = call(
        app.clone(), "POST", "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": params("tenant-a", "push-context") }),
        "1.0",
    ).await;
    let sent: Value = serde_json::from_str(&body).unwrap();
    let task_id = sent["result"]["task"]["id"].as_str().unwrap();
    let (_, _, body) = call(
        app.clone(), "POST", "/v1/a2a",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "CreateTaskPushNotificationConfig",
            "params": { "tenant": "tenant-a", "taskId": task_id, "id": "push-a", "url": "https://example.invalid/hook", "authentication": { "scheme": "Bearer", "credentials": "secret" } }
        }), "1.0",
    ).await;
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["result"]["id"], "push-a", "{created}");
    assert_eq!(created["result"]["authentication"]["scheme"], "Bearer");
    assert!(
        created["result"]["authentication"]
            .get("credentials")
            .is_none()
    );

    let (_, _, body) = call(
        app, "POST", "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 3, "method": "ListTaskPushNotificationConfigs", "params": { "tenant": "tenant-b", "taskId": task_id } }),
        "1.0",
    ).await;
    let foreign: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        foreign["error"]["code"], -32001,
        "cross-tenant access is hidden"
    );
}

#[tokio::test]
async fn v1_rest_transport_paths_media_type_and_tenant_scope_match_the_sdk() {
    let app = router(Arc::new(Runtime), awaken_time::system_clock());
    let (status, headers, body) = call(
        app.clone(),
        "POST",
        "/tenant-rest/message:send",
        params("tenant-rest", "rest-context"),
        "1.0",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/a2a+json");
    let sent: Value = serde_json::from_str(&body).unwrap();
    let task_id = sent["task"]["id"].as_str().unwrap();

    let (status, headers, body) = call(
        app.clone(),
        "GET",
        "/tenant-rest/tasks?pageSize=25&includeArtifacts=false",
        Value::Null,
        "1.0",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(headers["content-type"], "application/a2a+json");
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["totalSize"], 1, "{listed}");

    let (status, _, body) = call(
        app,
        "POST",
        &format!("/tenant-rest/tasks/{task_id}/pushNotificationConfigs"),
        json!({
            "tenant": "tenant-rest", "taskId": task_id, "id": "rest-push",
            "url": "https://example.invalid/hook"
        }),
        "1.0",
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["tenant"], "tenant-rest");
    assert_eq!(created["id"], "rest-push");
}

#[tokio::test]
async fn v1_return_immediately_replaces_the_legacy_blocking_switch() {
    let app = router(Arc::new(Runtime), awaken_time::system_clock());
    let mut immediate = params("tenant-a", "immediate-context");
    immediate["configuration"] = json!({ "returnImmediately": true });
    let (_, _, body) = call(
        app.clone(),
        "POST",
        "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": immediate }),
        "1.0",
    )
    .await;
    let immediate: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        immediate["result"]["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );

    let mut blocking = params("tenant-a", "blocking-context");
    blocking["configuration"] = json!({ "returnImmediately": false });
    let (_, _, body) = call(
        app,
        "POST",
        "/v1/a2a",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "SendMessage", "params": blocking }),
        "1.0",
    )
    .await;
    let blocking: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        blocking["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
}

#[tokio::test]
async fn missing_task_has_the_same_rest_classification_in_both_versions() {
    for version in ["0.3.0", "1.0"] {
        let app = router(Arc::new(Runtime), awaken_time::system_clock());
        let mut request = params("audit-tenant", "audit-context");
        request["message"]["taskId"] = json!("audit-nonexistent-task");
        if version == "0.3.0" {
            request["message"]["kind"] = json!("message");
            request["message"]["role"] = json!("user");
            request["message"]["parts"] = json!([{ "kind": "text", "text": "hello" }]);
        }
        let (status, _, body) =
            call(app, "POST", "/audit-tenant/message:send", request, version).await;
        println!("AUDIT missing_task version={version} status={status} body={body}");
        assert_eq!(status, 404, "missing task version={version}: {body}");
    }
}

struct AuditUnavailableRuntime;
#[async_trait]
impl Runs for AuditUnavailableRuntime {
    async fn run(
        &self,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::unavailable("audit temporary upstream outage"))
    }
    async fn resume(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: RunResume,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::unavailable("audit temporary upstream outage"))
    }
    async fn pending(&self, _: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }
    async fn history(&self, _: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }
    fn model(&self) -> String {
        "audit-unavailable".into()
    }
}

#[tokio::test]
async fn unavailable_has_the_same_rest_classification_in_both_versions() {
    for version in ["0.3.0", "1.0"] {
        let app = router(
            Arc::new(AuditUnavailableRuntime),
            awaken_time::system_clock(),
        );
        let mut request = params("audit-tenant", "audit-context");
        if version == "0.3.0" {
            request["message"]["kind"] = json!("message");
            request["message"]["role"] = json!("user");
            request["message"]["parts"] = json!([{ "kind": "text", "text": "hello" }]);
        }
        let (status, _, body) =
            call(app, "POST", "/audit-tenant/message:send", request, version).await;
        println!("AUDIT unavailable version={version} status={status} body={body}");
        assert_eq!(status, 503, "unavailable version={version}: {body}");
    }
}
