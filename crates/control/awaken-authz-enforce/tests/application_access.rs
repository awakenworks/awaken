use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_authz_enforce::{
    ApplicationAccessAuthenticator, ApplicationAuthenticationError, ApplicationGrant,
    ApplicationIdentity, ApplicationThreadBinding, application_guard,
};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Json, Path};
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tower::ServiceExt;

fn grant(protocols: &[&str], operations: &[&str]) -> ApplicationGrant {
    ApplicationGrant {
        protocols: protocols.iter().map(|value| (*value).to_string()).collect(),
        operations: operations
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        thread_bindings: HashMap::from([(
            "customer-thread".to_string(),
            ApplicationThreadBinding {
                managed_session_id: "sesn_existing".to_string(),
                agent_id: "support".to_string(),
            },
        )]),
    }
}

struct TestAuthenticator {
    token: String,
    identity: ApplicationIdentity,
    failure: Option<ApplicationAuthenticationError>,
}

#[async_trait::async_trait]
impl ApplicationAccessAuthenticator for TestAuthenticator {
    async fn authenticate(
        &self,
        presented: &str,
    ) -> Result<ApplicationIdentity, ApplicationAuthenticationError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if presented != self.token {
            return Err(ApplicationAuthenticationError::Invalid);
        }
        Ok(self.identity.clone())
    }
}

fn authenticator(token: &str, grant: ApplicationGrant) -> Arc<dyn ApplicationAccessAuthenticator> {
    Arc::new(TestAuthenticator {
        token: token.to_string(),
        identity: ApplicationIdentity {
            workspace_id: "ws-1".into(),
            grant,
        },
        failure: None,
    })
}

fn failing_authenticator(
    error: ApplicationAuthenticationError,
) -> Arc<dyn ApplicationAccessAuthenticator> {
    Arc::new(TestAuthenticator {
        token: String::new(),
        identity: ApplicationIdentity {
            workspace_id: "ws-1".into(),
            grant: grant(&["ai-sdk"], &["thread.run"]),
        },
        failure: Some(error),
    })
}

fn app(authenticator: Arc<dyn ApplicationAccessAuthenticator>) -> Router {
    async fn echo_run(
        Path(thread): Path<String>,
        resolved: Option<axum::Extension<awaken_tenancy::ResolvedResourceId>>,
        agent: Option<axum::Extension<awaken_tenancy::ResolvedAgentId>>,
        workspace: Option<axum::Extension<awaken_tenancy::WorkspaceScope>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        Json(json!({
            "thread": thread,
            "resolved": resolved.map(|axum::Extension(value)| value.0),
            "agent": agent.map(|axum::Extension(value)| value.0),
            "workspace": workspace.map(|axum::Extension(value)| value.0),
            "body": body,
        }))
    }
    async fn echo_history(
        Path(thread): Path<String>,
        resolved: Option<axum::Extension<awaken_tenancy::ResolvedResourceId>>,
    ) -> Json<Value> {
        Json(json!({
            "thread": thread,
            "resolved": resolved.map(|axum::Extension(value)| value.0),
        }))
    }
    async fn echo_ag_ui(
        resolved: Option<axum::Extension<awaken_tenancy::ResolvedResourceId>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        Json(json!({
            "resolved": resolved.map(|axum::Extension(value)| value.0),
            "body": body,
        }))
    }

    Router::new()
        .route("/v1/ai-sdk/threads/{thread}/runs", post(echo_run))
        .route("/v1/ai-sdk/threads/{thread}/messages", get(echo_history))
        .route("/v1/ag-ui", post(echo_ag_ui))
        .route("/v1/ag-ui/threads/{thread}/messages", get(echo_history))
        .layer(axum::middleware::from_fn_with_state(
            authenticator,
            application_guard,
        ))
}

fn request(method: &str, path: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if method == "POST" {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = if method == "POST" {
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    builder.body(body).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Cause-effect graph: authenticated token (C1), protocol permission (C2),
/// operation permission (C3), exact external-thread binding (C4), matching
/// path/body thread (C5), and matching frozen Agent (C6) gate the sole effects:
/// dispatch to the bound Managed Session (E1) or fail before dispatch (E2).
/// Decision rules covered here: R1 all causes true -> E1 with exact rewrite;
/// R2 C1 false -> 401/E2. Later tests cover R3-R7 for each other false cause.
#[tokio::test]
async fn exact_binding_rewrites_to_the_existing_managed_session() {
    let token = "valid-application-token"; // awaken-allow: secret -- inert test fixture
    let authenticator = authenticator(
        token,
        grant(&["ai-sdk"], &["thread.run", "thread.messages.read"]),
    );
    let router = app(authenticator);

    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(token),
            json!({ "threadId": "customer-thread", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["thread"], "customer-thread");
    assert_eq!(body["resolved"], "sesn_existing");
    assert_eq!(body["body"]["threadId"], "sesn_existing");
    assert_eq!(body["body"]["agentId"], "support");
    assert_eq!(body["agent"], "support");
    assert_eq!(body["workspace"], "ws-1");
    assert!(!body["resolved"].as_str().unwrap().starts_with("app_"));

    let missing = router
        .clone()
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/customer-thread/messages",
            None,
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    let invalid = router
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/customer-thread/messages",
            Some("invalid-application-token"),
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
}

/// Cause/effect graph extension: the authenticated application token's exact
/// Workspace (C7) and an optional path-selected Workspace (C8) must agree before
/// the bound Session reaches a protocol adapter. Decision rules: R8 no path
/// selection -> publish C7 downstream; R9 C8 == C7 -> publish the same scope;
/// R10 C8 != C7 -> 403 before adapter dispatch. This keeps tenancy in the
/// existing application credential instead of relying on a second outer PEP.
#[tokio::test]
async fn application_token_fences_and_projects_its_workspace() {
    async fn workspace(
        axum::Extension(scope): axum::Extension<awaken_tenancy::WorkspaceScope>,
    ) -> String {
        scope.0
    }

    let token = "workspace-application-token"; // awaken-allow: secret -- inert test fixture
    let authenticator = authenticator(token, grant(&["ai-sdk"], &["thread.run"]));
    let router = Router::new()
        .route("/v1/ai-sdk/chat", post(workspace))
        .layer(axum::middleware::from_fn_with_state(
            authenticator,
            application_guard,
        ));
    let scoped_request = |selected: Option<&str>| {
        let mut request = request(
            "POST",
            "/v1/ai-sdk/chat",
            Some(token),
            json!({"threadId": "customer-thread", "messages": []}),
        );
        if let Some(selected) = selected {
            request
                .extensions_mut()
                .insert(awaken_authz_enforce::RequestTenancy {
                    workspace_id: selected.to_owned(),
                });
        }
        request
    };

    let bare = router.clone().oneshot(scoped_request(None)).await.unwrap();
    assert_eq!(bare.status(), StatusCode::OK, "R8");
    assert_eq!(
        to_bytes(bare.into_body(), usize::MAX).await.unwrap(),
        "ws-1"
    );

    let exact = router
        .clone()
        .oneshot(scoped_request(Some("ws-1")))
        .await
        .unwrap();
    assert_eq!(exact.status(), StatusCode::OK, "R9");

    let foreign = router
        .oneshot(scoped_request(Some("ws-foreign")))
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::FORBIDDEN, "R10");
}

/// Decision rules from the graph above: R3 protocol false, R4 operation false,
/// and R5 binding false each produce 403/E2. The protocol rule proves an AI SDK
/// credential cannot become an AG-UI credential even for the same Session.
#[tokio::test]
async fn protocol_operation_and_binding_permissions_fail_closed() {
    let run_only = "limited-application-token";
    let router = app(authenticator(run_only, grant(&["ai-sdk"], &["thread.run"])));

    let ag_ui = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ag-ui",
            Some(run_only),
            json!({ "threadId": "customer-thread", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(ag_ui.status(), StatusCode::FORBIDDEN);

    let history = router
        .clone()
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/customer-thread/messages",
            Some(run_only),
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(history.status(), StatusCode::FORBIDDEN);

    let unbound = router
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/another-thread/runs",
            Some(run_only),
            json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(unbound.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn response_only_capability_accepts_tool_decisions_but_rejects_new_turns() {
    let token = "response-only-application-token"; // awaken-allow: secret -- inert test fixture
    let router = app(authenticator(
        token,
        grant(&["ai-sdk"], &["thread.messages.read", "thread.respond"]),
    ));
    let decision = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(token),
            json!({"messages": [{
                "id": "assistant-1",
                "role": "assistant",
                "parts": [{
                    "type": "dynamic-tool",
                    "toolCallId": "tool-1",
                    "toolName": "bash",
                    "state": "approval-responded",
                    "input": {"command": "cargo test"},
                    "approval": {"id": "approval-1", "approved": true}
                }]
            }]}),
        ))
        .await
        .unwrap();
    assert_eq!(decision.status(), StatusCode::OK);

    let new_turn = router
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(token),
            json!({"messages": [{
                "id": "user-1", "role": "user",
                "parts": [{"type": "text", "text": "do something else"}]
            }]}),
        ))
        .await
        .unwrap();
    assert_eq!(new_turn.status(), StatusCode::FORBIDDEN);
}

/// Decision rules from the graph above: R6 path/body mismatch -> 400/E2 and
/// R7 requested Agent differs from the bound Session baseline -> 403/E2.
#[tokio::test]
async fn conflicting_request_identity_cannot_override_the_binding() {
    let token = "identity-application-token"; // awaken-allow: secret -- inert test fixture
    let router = app(authenticator(token, grant(&["ai-sdk"], &["thread.run"])));

    let thread_mismatch = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(token),
            json!({ "threadId": "another-thread", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(thread_mismatch.status(), StatusCode::BAD_REQUEST);

    let agent_mismatch = router
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(token),
            json!({ "agentId": "billing", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(agent_mismatch.status(), StatusCode::FORBIDDEN);
}

/// Route classification is an independent default-deny cause: no known exact
/// route means no operation exists to authorize, so the terminal effect is 403.
#[tokio::test]
async fn unknown_routes_are_not_inferred_from_the_http_method() {
    let token = "unknown-route-application-token"; // awaken-allow: secret -- inert test fixture
    let response = app(authenticator(
        token,
        grant(&["ai-sdk"], &["thread.messages.read"]),
    ))
    .oneshot(request(
        "GET",
        "/v1/ai-sdk/threads/customer-thread/export",
        Some(token),
        Value::Null,
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// Authority health is independent from credential validity. Cause/effect
/// rules: R11 repository unavailable and R12 durable row corrupt both produce
/// 503/E2; neither condition may be collapsed into 401 or invoke the adapter.
#[tokio::test]
async fn authority_failures_return_service_unavailable_before_dispatch() {
    async fn dispatched(axum::Extension(calls): axum::Extension<Arc<AtomicUsize>>) -> StatusCode {
        calls.fetch_add(1, Ordering::Relaxed);
        StatusCode::OK
    }

    let mut expected_body = None;
    for error in [
        ApplicationAuthenticationError::Unavailable,
        ApplicationAuthenticationError::Corrupt,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/v1/ai-sdk/chat", post(dispatched))
            .layer(axum::Extension(calls.clone()))
            .layer(axum::middleware::from_fn_with_state(
                failing_authenticator(error),
                application_guard,
            ));
        let response = router
            .oneshot(request(
                "POST",
                "/v1/ai-sdk/chat",
                Some("opaque-application-token"),
                json!({"threadId": "customer-thread", "messages": []}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        if let Some(expected) = &expected_body {
            assert_eq!(&body, expected, "R11/R12 expose one fixed response");
        } else {
            expected_body = Some(body);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}
