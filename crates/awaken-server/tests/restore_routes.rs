//! Integration tests for `POST /v1/config/:namespace/:id/restore`.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::{AgentSpec, ModelBindingSpec, ProviderSpec};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::registry::traits::ModelBinding;
use awaken_server::app::{AppState, ServerConfig};
use awaken_server::mailbox::{Mailbox, MailboxConfig};
use awaken_server::routes::build_router;
use awaken_server::services::audit_log::AuditLogger;
use awaken_server::services::config_runtime::{
    ConfigRuntimeError, ConfigRuntimeManager, ProviderExecutorFactory,
};
use awaken_stores::InMemoryStore;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct ImmediateExecutor;

#[async_trait]
impl LlmExecutor for ImmediateExecutor {
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

struct TestProviderFactory;

impl ProviderExecutorFactory for TestProviderFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        if spec.adapter.eq_ignore_ascii_case("stub") {
            return Ok(Arc::new(ImmediateExecutor));
        }
        Err(ConfigRuntimeError::UnsupportedProviderAdapter(
            spec.adapter.clone(),
        ))
    }
}

fn bootstrap_agent() -> AgentSpec {
    AgentSpec {
        id: "bootstrap".into(),
        model_id: "bootstrap".into(),
        system_prompt: "bootstrap".into(),
        max_rounds: 1,
        ..Default::default()
    }
}

async fn build_test_app() -> axum::Router {
    let config_store = Arc::new(InMemoryStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_model_binding(
                "bootstrap",
                ModelBinding {
                    provider_id: "bootstrap".into(),
                    upstream_model: "bootstrap-model".into(),
                },
            )
            .with_agent_spec(bootstrap_agent())
            .with_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );

    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(TestProviderFactory)),
    );
    manager
        .bootstrap_if_empty(
            &[ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }],
            &[ModelBindingSpec {
                id: "bootstrap".into(),
                provider_id: "bootstrap".into(),
                upstream_model: "bootstrap-model".into(),
                created_at: None,
                updated_at: None,
            }],
            &[bootstrap_agent()],
            &[],
        )
        .await
        .expect("bootstrap");
    manager.apply().await.expect("apply");

    let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "restore-test".into(),
        MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager)
    .with_audit_log(audit_logger);

    build_router(&state).with_state(state)
}

// ── helpers ───────────────────────────────────────────────────────────────

async fn post_json(app: &axum::Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn put_json(app: &axum::Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn delete_resource(app: &axum::Router, uri: &str) -> StatusCode {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

async fn get_audit_log(app: &axum::Router, qs: &str) -> Value {
    let uri = if qs.is_empty() {
        "/v1/audit-log".to_string()
    } else {
        format!("/v1/audit-log?{qs}")
    };
    let req = Request::builder()
        .method("GET")
        .uri(&uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

// ── tests ─────────────────────────────────────────────────────────────────

/// Restore an updated agent to its prior version.
#[tokio::test]
async fn restore_agent_to_prior_version() {
    let app = build_test_app().await;

    // Create agent v1.
    let (status, _) = post_json(
        &app,
        "/v1/config/agents",
        &json!({
            "id": "restore-agent",
            "model_id": "bootstrap",
            "system_prompt": "v1 prompt",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Fetch the create event ULID.
    let audit = get_audit_log(&app, "resource=agents/restore-agent").await;
    let items = audit["items"].as_array().expect("items");
    let create_event_id = items
        .iter()
        .find(|e| e["action"] == "create")
        .and_then(|e| e["id"].as_str())
        .expect("create event")
        .to_string();

    // Update agent to v2.
    let (status, _) = put_json(
        &app,
        "/v1/config/agents/restore-agent",
        &json!({
            "id": "restore-agent",
            "model_id": "bootstrap",
            "system_prompt": "v2 prompt",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Count audit events before restore (create + update = 2).
    let audit_before = get_audit_log(&app, "resource=agents/restore-agent").await;
    let count_before = audit_before["items"].as_array().expect("items").len();

    // Restore to v1.
    let (status, body) = post_json(
        &app,
        "/v1/config/agents/restore-agent/restore",
        &json!({ "version": create_event_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["system_prompt"], "v1 prompt");

    // Audit log must contain exactly one new event (the Restore), not two.
    let audit = get_audit_log(&app, "resource=agents/restore-agent").await;
    let items = audit["items"].as_array().expect("items");
    assert_eq!(
        items.len(),
        count_before + 1,
        "restore must emit exactly one audit event; got {} (was {})",
        items.len(),
        count_before
    );
    let restore_event = items
        .iter()
        .find(|e| e["action"] == "restore")
        .expect("restore event must be present");
    assert_eq!(
        restore_event["restored_from"].as_str(),
        Some(create_event_id.as_str()),
        "restored_from must reference the source event ULID"
    );
    assert!(
        restore_event["before"].is_object(),
        "before must contain the pre-restore spec"
    );
    assert!(
        restore_event["after"].is_object(),
        "after must contain the restored spec"
    );
}

/// Restore a deleted resource uses the `before` payload via `create`.
#[tokio::test]
async fn restore_deleted_resource_uses_before() {
    let app = build_test_app().await;

    // Create and immediately delete an agent.
    let (status, _) = post_json(
        &app,
        "/v1/config/agents",
        &json!({
            "id": "deleted-agent",
            "model_id": "bootstrap",
            "system_prompt": "original",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let del_status = delete_resource(&app, "/v1/config/agents/deleted-agent").await;
    assert_eq!(del_status, StatusCode::NO_CONTENT);

    // Get the delete event ULID.
    let audit = get_audit_log(&app, "resource=agents/deleted-agent").await;
    let items = audit["items"].as_array().expect("items");
    let delete_event_id = items
        .iter()
        .find(|e| e["action"] == "delete")
        .and_then(|e| e["id"].as_str())
        .expect("delete event")
        .to_string();

    // Restore from the delete event (should recreate using `before`).
    let (status, body) = post_json(
        &app,
        "/v1/config/agents/deleted-agent/restore",
        &json!({ "version": delete_event_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["system_prompt"], "original");
    assert_eq!(body["id"], "deleted-agent");
}

/// Attempting to restore a Restart event returns 422.
#[tokio::test]
async fn restore_restart_event_returns_422() {
    let _app = build_test_app().await;

    use awaken_contract::AuditAction;
    use awaken_contract::AuditEvent;
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_server::services::audit_log::AUDIT_NAMESPACE;

    let config_store = Arc::new(InMemoryStore::new());
    let thread_store = Arc::new(InMemoryStore::new());
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_provider("bootstrap", Arc::new(ImmediateExecutor))
            .with_model_binding(
                "bootstrap",
                ModelBinding {
                    provider_id: "bootstrap".into(),
                    upstream_model: "bootstrap-model".into(),
                },
            )
            .with_agent_spec(bootstrap_agent())
            .with_thread_run_store(thread_store.clone())
            .build()
            .expect("build runtime"),
    );
    let manager = Arc::new(
        ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
            .expect("manager")
            .with_provider_factory(Arc::new(TestProviderFactory)),
    );
    manager
        .bootstrap_if_empty(
            &[ProviderSpec {
                id: "bootstrap".into(),
                adapter: "stub".into(),
                ..Default::default()
            }],
            &[ModelBindingSpec {
                id: "bootstrap".into(),
                provider_id: "bootstrap".into(),
                upstream_model: "bootstrap-model".into(),
                created_at: None,
                updated_at: None,
            }],
            &[bootstrap_agent()],
            &[],
        )
        .await
        .expect("bootstrap");
    manager.apply().await.expect("apply");

    // Write a Restart event directly into the audit store.
    let restart_id = ulid::Ulid::new().to_string();
    let restart_event = AuditEvent {
        id: restart_id.clone(),
        ts: chrono::Utc::now().to_rfc3339(),
        actor: "anonymous".to_string(),
        action: AuditAction::Restart,
        resource: "agents/restart-target".to_string(),
        before: None,
        after: None,
        ip: None,
        request_id: None,
        restored_from: None,
    };
    config_store
        .put(
            AUDIT_NAMESPACE,
            &restart_id,
            &serde_json::to_value(&restart_event).unwrap(),
        )
        .await
        .unwrap();

    let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
    let resolver = runtime.resolver_arc();
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(awaken_stores::InMemoryMailboxStore::new()),
        thread_store.clone(),
        "restart-restore-test".into(),
        MailboxConfig::default(),
    ));
    let state = AppState::new(
        runtime,
        mailbox,
        thread_store,
        resolver,
        ServerConfig::default(),
    )
    .with_config_store(config_store)
    .with_config_runtime_manager(manager)
    .with_audit_log(audit_logger);
    let app = build_router(&state).with_state(state);

    let (status, body) = post_json(
        &app,
        "/v1/config/agents/restart-target/restore",
        &json!({ "version": restart_id }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("not restorable")),
        "body must mention 'not restorable': {body}"
    );
}

/// Cross-resource restore returns 422.
#[tokio::test]
async fn cross_resource_restore_returns_422() {
    let app = build_test_app().await;

    // Create agent A.
    let (status, _) = post_json(
        &app,
        "/v1/config/agents",
        &json!({
            "id": "agent-a",
            "model_id": "bootstrap",
            "system_prompt": "for agent a",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Get its create event ULID.
    let audit = get_audit_log(&app, "resource=agents/agent-a").await;
    let items = audit["items"].as_array().expect("items");
    let agent_a_event_id = items[0]["id"].as_str().expect("event id").to_string();

    // Attempt to restore agent-a's event to agent-b.
    let (status, body) = post_json(
        &app,
        "/v1/config/agents/agent-b/restore",
        &json!({ "version": agent_a_event_id }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("cross-resource")),
        "body must mention cross-resource: {body}"
    );
}

/// Unknown version ULID returns 404 with reason "unknown".
#[tokio::test]
async fn unknown_version_returns_404_unknown() {
    let app = build_test_app().await;

    let (status, body) = post_json(
        &app,
        "/v1/config/agents/some-agent/restore",
        &json!({ "version": "01DOESNOTEXIST0000000000000" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(body["error"], "version not found");
    assert_eq!(body["reason"], "unknown");
}

/// Restore failure when the referenced model no longer exists returns 422.
#[tokio::test]
async fn restore_fails_when_referenced_model_is_gone() {
    let app = build_test_app().await;

    // Create provider + model + agent referencing the model.
    let (status, _) = post_json(
        &app,
        "/v1/config/providers",
        &json!({"id": "prov-restore-test", "adapter": "stub"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = post_json(
        &app,
        "/v1/config/models",
        &json!({
            "id": "model-restore-test",
            "provider_id": "prov-restore-test",
            "upstream_model": "gpt-4"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = post_json(
        &app,
        "/v1/config/agents",
        &json!({
            "id": "agent-orphan",
            "model_id": "model-restore-test",
            "system_prompt": "original",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Update agent to use bootstrap model.
    let (status, _) = put_json(
        &app,
        "/v1/config/agents/agent-orphan",
        &json!({
            "id": "agent-orphan",
            "model_id": "bootstrap",
            "system_prompt": "updated",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Force-delete model that the original version referenced.
    let del_status = delete_resource(&app, "/v1/config/models/model-restore-test?force=true").await;
    assert_eq!(del_status, StatusCode::NO_CONTENT);
    let del_status =
        delete_resource(&app, "/v1/config/providers/prov-restore-test?force=true").await;
    assert_eq!(del_status, StatusCode::NO_CONTENT);

    // Get create event (references deleted model).
    let audit = get_audit_log(&app, "resource=agents/agent-orphan").await;
    let items = audit["items"].as_array().expect("items");
    let create_event_id = items
        .iter()
        .find(|e| e["action"] == "create")
        .and_then(|e| e["id"].as_str())
        .expect("create event")
        .to_string();

    // Restore to original version — should fail because model is gone.
    let (status, body) = post_json(
        &app,
        "/v1/config/agents/agent-orphan/restore",
        &json!({ "version": create_event_id }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(
        body["error"].is_string(),
        "body must contain error message: {body}"
    );
}

/// The restore audit event has `restored_from` populated.
#[tokio::test]
async fn restore_audit_event_has_restored_from() {
    let app = build_test_app().await;

    let (status, _) = post_json(
        &app,
        "/v1/config/agents",
        &json!({
            "id": "audit-check-agent",
            "model_id": "bootstrap",
            "system_prompt": "initial",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let audit = get_audit_log(&app, "resource=agents/audit-check-agent").await;
    let items = audit["items"].as_array().expect("items");
    let create_id = items[0]["id"].as_str().expect("event id").to_string();

    // Update so current != initial.
    let (status, _) = put_json(
        &app,
        "/v1/config/agents/audit-check-agent",
        &json!({
            "id": "audit-check-agent",
            "model_id": "bootstrap",
            "system_prompt": "changed",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Restore.
    let (status, _) = post_json(
        &app,
        "/v1/config/agents/audit-check-agent/restore",
        &json!({ "version": create_id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Inspect the restore event in the audit log.
    let audit = get_audit_log(&app, "resource=agents/audit-check-agent").await;
    let items = audit["items"].as_array().expect("items");
    let restore_ev = items
        .iter()
        .find(|e| e["action"] == "restore")
        .expect("restore event");

    assert_eq!(
        restore_ev["restored_from"].as_str(),
        Some(create_id.as_str())
    );
    assert_eq!(restore_ev["action"], "restore");
    assert!(restore_ev["after"]["system_prompt"] == "initial");
}

/// Restore from a Publish audit event restores `event.after` as the resource state.
///
/// # Why ignored
///
/// The publish flow described in ADR-0025 is not yet implemented. There is no API
/// endpoint or service method that emits a `Publish` audit event, so it is not
/// possible to construct one through the normal execution path. Synthesising a fake
/// Publish event directly in the store would test the plumbing without validating
/// that real publish events have the correct shape; that would give false confidence.
///
/// Enable this test once the publish endpoint from ADR-0025 is implemented and
/// `AuditAction::Publish` events are produced by the real code path.
#[tokio::test]
#[ignore = "publish flow (ADR-0025) not yet implemented; enable once publish endpoint exists"]
async fn restore_from_publish_event_uses_after() {
    let app = build_test_app().await;

    // Create an agent so there is at least one version on record.
    let (status, _) = post_json(
        &app,
        "/v1/config/agents",
        &json!({
            "id": "publish-restore-agent",
            "model_id": "bootstrap",
            "system_prompt": "published version",
            "max_rounds": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // TODO: call the publish endpoint once it exists to emit a real Publish event,
    // then retrieve its ULID from the audit log, call restore with it, and assert:
    //   - response body matches event.after
    //   - audit log has exactly one new entry of action=restore
    //   - restored_from references the publish event ULID
    todo!("wire up publish endpoint from ADR-0025")
}
