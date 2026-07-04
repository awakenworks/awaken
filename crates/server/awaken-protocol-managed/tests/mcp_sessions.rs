//! Session-create MCP binding (ADR-0043 Phase 3): a session's `mcp_servers` are
//! bound to vault credentials by exact URL and provisioned through
//! `SessionRuntime::prepare_session` BEFORE the record exists — a failed
//! preparation fails the create with the mapped error envelope.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_credential_vault::InMemorySecretStore;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeReport, RunError, RunErrorKind, SessionInit, SessionRuntime,
    TurnOutcome, VaultState, router, vault_router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A fake runtime that records every `prepare_session` init (and optionally
/// fails it), so a test can assert exactly what a session create provisions.
struct PreparingFake {
    captured: Arc<Mutex<Vec<SessionInit>>>,
    fail_with: Option<RunErrorKind>,
}

#[async_trait::async_trait]
impl SessionRuntime for PreparingFake {
    async fn prepare_session(&self, _thread: &str, init: SessionInit) -> Result<(), RunError> {
        self.captured.lock().unwrap().push(init);
        match self.fail_with {
            Some(RunErrorKind::BadRequest) => Err(RunError::bad_request("prepare refused")),
            Some(RunErrorKind::Internal) => Err(RunError::internal("prepare blew up")),
            None => Ok(()),
        }
    }
    async fn run_turn(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
        _e: bool,
    ) -> Result<TurnOutcome, RunError> {
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

struct Harness {
    app: Router,
    vaults: Arc<VaultState>,
    captured: Arc<Mutex<Vec<SessionInit>>>,
}

/// Sessions + vaults over ONE shared `VaultState`, the way the server mounts
/// them, so a credential entered through the vault routes is bindable at create.
fn harness(fail_with: Option<RunErrorKind>) -> Harness {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let vaults = Arc::new(VaultState::new(secrets, credentials));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let state = ManagedState::new(PreparingFake {
        captured: captured.clone(),
        fail_with,
    })
    .with_vaults(vaults.clone());
    let app = router(Arc::new(state)).merge(vault_router(vaults.clone()));
    Harness {
        app,
        vaults,
        captured,
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

const MCP_URL: &str = "https://mcp.example.com/sse";

/// Create a vault holding an `mcp_oauth` credential for [`MCP_URL`]; returns
/// `(vault_id, credential_id)`.
async fn vault_with_mcp_oauth(h: &Harness) -> (String, String) {
    let (s, vault) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "mcp" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let (s, cred) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "mcp_oauth",
            "mcp_server_url": MCP_URL,
            "access_token": "at-secret-token" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    (vault_id, cred["id"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn create_binds_mcp_server_to_vault_credential_and_echoes_the_wire_shape() {
    let h = harness(None);
    let (vault_id, cred_id) = vault_with_mcp_oauth(&h).await;

    let (s, session) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "calc-agent",
            // The SDK sends `type: "url"`; it is tolerated on input.
            "mcp_servers": [{ "name": "calc", "type": "url", "url": MCP_URL }],
            "vault_ids": [vault_id],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // The agent object echoes the accepted server in the SDK response shape.
    assert_eq!(
        session["agent"]["mcp_servers"],
        json!([{ "name": "calc", "type": "url", "url": MCP_URL }])
    );

    // prepare_session saw the binding, carrying the credential's DOMAIN id (the
    // resolver vocabulary), never the wire credential id.
    let captured = h.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let init = &captured[0];
    assert_eq!(init.agent_id, "calc-agent");
    assert_eq!(init.mcp_servers.len(), 1);
    assert_eq!(init.mcp_servers[0].name, "calc");
    assert_eq!(init.mcp_servers[0].url, MCP_URL);
    let expected = h
        .vaults
        .credential_source_id(&vault_id, &cred_id)
        .expect("wire credential maps to a domain source");
    assert_eq!(
        init.mcp_servers[0].credential_source_id.as_ref(),
        Some(&expected)
    );
}

#[tokio::test]
async fn session_without_mcp_servers_echoes_empty_and_prepares_an_empty_init() {
    let h = harness(None);
    let (s, session) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "coder" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(session["agent"]["mcp_servers"], json!([]));
    let captured = h.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].agent_id, "coder");
    assert!(captured[0].mcp_servers.is_empty());
}

/// Pins the documented-lenient path: the vault surface exposes no existence
/// lookup, so a `vault_id` naming no vault contributes NO binding (the MCP
/// server then rejects the unauthenticated connect at the first turn) rather
/// than failing the create.
#[tokio::test]
async fn unknown_vault_id_yields_no_binding_not_an_error() {
    let h = harness(None);
    let (s, session) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "calc-agent",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": MCP_URL }],
            "vault_ids": ["vlt_missing"],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(session["agent"]["mcp_servers"].as_array().unwrap().len(), 1);
    let captured = h.captured.lock().unwrap();
    assert!(captured[0].mcp_servers[0].credential_source_id.is_none());
}

#[tokio::test]
async fn failing_prepare_session_fails_the_create_with_the_mapped_envelope() {
    for (kind, status, error_type) in [
        (
            RunErrorKind::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
        ),
        (
            RunErrorKind::BadRequest,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
    ] {
        let h = harness(Some(kind));
        let (s, body) = call(
            &h.app,
            "POST",
            "/v1/sessions",
            Some(json!({ "agent": "calc-agent" })),
        )
        .await;
        assert_eq!(s, status, "{kind:?}");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], error_type);
        // Fail closed: the failed create left no session behind.
        let (s, _) = call(&h.app, "GET", "/v1/sessions/sesn_0", None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "no half-provisioned session");
    }
}
