//! `POST /v1/sessions` resolves the session's `environment_id` to its networking
//! policy and stages the exact `EnvironmentSnapshot`: semantic MCP/package
//! exceptions are resolved before the snapshot is frozen and the Runtime sees
//! only one canonical network fact. No coarse boolean becomes a second policy
//! authority.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::{
    ManagedState, environment_authoring_router, environment_work_router, router,
};
use awaken_session_contract::{
    OutcomeDrive, RunError, SessionInit, SessionRuntime, StepOutcome, ToolPermissionDecision,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Records each `prepare_session`'s exact frozen network policy.
struct CapturingFake {
    egress: Arc<Mutex<Vec<awaken_session_contract::SessionNetworkPolicy>>>,
}

#[async_trait::async_trait]
impl SessionRuntime for CapturingFake {
    async fn prepare_session(&self, _thread: &str, init: SessionInit) -> Result<(), RunError> {
        self.egress.lock().unwrap().push(init.environment.network);
        Ok(())
    }
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn add_system(&self, _t: &str, _x: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for CapturingFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        Ok(awaken_session_contract::McpRealizationReceipt {
            receipt_fingerprint: request.fingerprint(),
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn session_carries_the_exact_frozen_environment_network() {
    let (environment_authoring, environment_execution) =
        awaken_protocol_managed::test_support::environment_components();
    let egress = Arc::new(Mutex::new(Vec::new()));
    let managed = Arc::new(
        ManagedState::new_with_mcp(CapturingFake {
            egress: egress.clone(),
        })
        .with_environments(environment_execution.clone()),
    );
    let app = router(managed)
        .merge(environment_authoring_router(environment_authoring))
        .merge(environment_work_router(environment_execution));

    // A limited-networking environment.
    let (_, limited) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "l", "config": { "type": "cloud", "networking": { "type": "limited" } } })),
    )
    .await;
    let limited_id = limited["id"].as_str().unwrap().to_string();

    // A session on it stages the exact closed policy.
    let (s, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": limited_id })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_contract::SessionNetworkPolicy::None)
    );

    // An unrestricted environment keeps the exact open policy.
    let (_, open) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "o", "config": { "type": "cloud", "networking": { "type": "unrestricted" } } })),
    )
    .await;
    let open_id = open["id"].as_str().unwrap().to_string();
    call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": open_id })),
    )
    .await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_contract::SessionNetworkPolicy::Unrestricted)
    );

    // An omitted/unknown environment defaults to host network.
    call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_contract::SessionNetworkPolicy::Unrestricted)
    );
}

/// Limited-network behavior cause graph:
/// C1 Environment is limited; C2 MCP exception enabled; C3 exact MCP target is
/// present at Session creation; C4 package-manager exception enabled.
/// E1 the sole snapshot compiler adds exact selected hosts; E2 Runtime receives
/// that frozen canonical policy; E3 a disabled exception grants nothing.
///
/// | Rule | C2 | C3 | C4 | Runtime effect |
/// |---|---|---|---|---|
/// | N1 | F | T | F | explicit hosts only; MCP denied |
/// | N2 | T | T | F | exact MCP host added |
/// | N3 | T | F | F | no ambient/default MCP host added |
/// | N4 | F | F | T | canonical public registry hosts added |
#[tokio::test]
async fn limited_network_exceptions_change_the_prepared_runtime_policy() {
    let (environment_authoring, environment_execution) =
        awaken_protocol_managed::test_support::environment_components();
    let egress = Arc::new(Mutex::new(Vec::new()));
    let managed = Arc::new(
        ManagedState::new_with_mcp(CapturingFake {
            egress: egress.clone(),
        })
        .with_environments(environment_execution.clone()),
    );
    let app = router(managed)
        .merge(environment_authoring_router(environment_authoring))
        .merge(environment_work_router(environment_execution));

    async fn environment(app: &Router, networking: Value) -> String {
        let (status, value) = call(
            app,
            "POST",
            "/v1/environments",
            Some(json!({
                "name": "limited",
                "config": { "type": "cloud", "networking": networking }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        value["id"].as_str().unwrap().to_string()
    }
    async fn create_session(app: &Router, environment_id: &str, with_mcp: bool) {
        let mut body = json!({ "agent": "a", "environment_id": environment_id });
        if with_mcp {
            body["mcp_servers"] = json!([{
                "name": "docs",
                "url": "https://Docs.Example.test:443/rpc"
            }]);
        }
        let (status, response) = call(app, "POST", "/v1/sessions", Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{response}");
    }

    let disabled = environment(
        &app,
        json!({
            "type": "limited",
            "allowed_hosts": ["api.example.test"],
            "allow_mcp_servers": false
        }),
    )
    .await;
    create_session(&app, &disabled, true).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_contract::SessionNetworkPolicy::Allowlist {
            hosts: vec!["api.example.test".into()]
        }),
        "N1 disabled MCP exception has no runtime effect"
    );

    let enabled = environment(
        &app,
        json!({ "type": "limited", "allow_mcp_servers": true }),
    )
    .await;
    create_session(&app, &enabled, true).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_contract::SessionNetworkPolicy::Allowlist {
            hosts: vec!["docs.example.test".into()]
        }),
        "N2 only the exact normalized MCP host reaches Runtime"
    );
    create_session(&app, &enabled, false).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_contract::SessionNetworkPolicy::None),
        "N3 no declared MCP target means no ambient fallback"
    );

    let packages = environment(
        &app,
        json!({ "type": "limited", "allow_package_managers": true }),
    )
    .await;
    create_session(&app, &packages, false).await;
    let expected = awaken_session_contract::SessionNetworkPolicy::Allowlist {
        hosts: awaken_environment_contract::PUBLIC_PACKAGE_REGISTRY_HOSTS
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
    .normalized();
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&expected),
        "N4 package-manager exception reaches Runtime as the canonical catalog"
    );
}
