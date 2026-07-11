//! Session-create MCP binding (ADR-0043 Phase 3): a session's `mcp_servers` are
//! bound to vault credentials by exact URL and provisioned through
//! `SessionRuntime::prepare_session` BEFORE the record exists — a failed
//! preparation fails the create with the mapped error envelope.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_credential_vault::InMemorySecretStore;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeReport, RunError, RunErrorKind, SessionInit,
    SessionLifecycleSink, SessionRuntime, TurnOutcome, VaultState, router, vault_router,
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
    async fn run(
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

/// The shared bind-time check is fail-closed on an unknown vault, and a pre-flight
/// caller gets exactly the error `create_session` would — without minting a session.
#[test]
fn check_bind_is_fail_closed_on_unknown_vault() {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let vaults = Arc::new(VaultState::new(secrets, credentials));
    let state = ManagedState::new(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        fail_with: None,
    })
    .with_vaults(vaults);

    let bad: awaken_protocol_managed::types::SessionCreateParams =
        serde_json::from_value(json!({ "agent": "a", "vault_ids": ["vlt_missing"] })).unwrap();
    assert!(matches!(
        state.check_bind(&bad),
        Err(awaken_protocol_managed::StateError::VaultNotFound(_))
    ));

    // No referenced vault → the bind is legal.
    let ok: awaken_protocol_managed::types::SessionCreateParams =
        serde_json::from_value(json!({ "agent": "a" })).unwrap();
    assert!(state.check_bind(&ok).is_ok());
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
    // The credential was entered without a refresh object, so the binding
    // carries no refresh configuration.
    assert!(init.mcp_servers[0].refresh.is_none());
}

#[tokio::test]
async fn create_carries_the_refresh_binding_of_a_refreshable_credential() {
    let h = harness(None);
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
            "access_token": "at-secret-token", // awaken-allow: secret
            "refresh": {
                "client_id": "cli_pub",
                "refresh_token": "rt-secret-token", // awaken-allow: secret
                "token_endpoint": "https://auth.example.com/token",
                "token_endpoint_auth": { "type": "none" },
                "scope": "mcp:read"
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let cred_id = cred["id"].as_str().unwrap().to_string();

    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "calc-agent",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": MCP_URL }],
            "vault_ids": [vault_id.clone()],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The binding carries the stored refresh configuration next to the source
    // id — the sealed refresh token's ref, never the token itself.
    let captured = h.captured.lock().unwrap();
    let refresh = captured[0].mcp_servers[0]
        .refresh
        .as_ref()
        .expect("a refreshable credential's binding carries its refresh config");
    assert_eq!(refresh.token_endpoint, "https://auth.example.com/token");
    assert_eq!(refresh.client_id, "cli_pub");
    assert_eq!(refresh.scope.as_deref(), Some("mcp:read"));
    assert_eq!(refresh.resource, None);
    let source_id = h.vaults.credential_source_id(&vault_id, &cred_id).unwrap();
    assert_eq!(
        refresh.refresh_token_ref.0,
        format!("sec:refresh:{}", source_id.0)
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

/// Fail closed at create: a `vault_ids` entry naming no vault 404s with the
/// standard envelope naming the vault id — no session record is left behind
/// and the runtime is never asked to provision anything.
#[tokio::test]
async fn unknown_vault_id_fails_the_create_with_404_and_provisions_nothing() {
    let h = harness(None);
    let (s, body) = call(
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
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("vlt_missing"),
        "the envelope names the offending vault id: {message}"
    );
    assert!(
        h.captured.lock().unwrap().is_empty(),
        "prepare_session must never run for a refused create"
    );
    let (s, _) = call(&h.app, "GET", "/v1/sessions/sesn_0", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no session record was created");
}

/// A create mixing one real vault with one bogus id fails closed too — every
/// named vault must exist, not just some.
#[tokio::test]
async fn known_plus_unknown_vault_id_still_fails_the_create() {
    let h = harness(None);
    let (vault_id, _) = vault_with_mcp_oauth(&h).await;
    let (s, body) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "calc-agent",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": MCP_URL }],
            "vault_ids": [vault_id, "vlt_bogus"],
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("vlt_bogus")
    );
    assert!(h.captured.lock().unwrap().is_empty());
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

/// A fresh process restarts the session sequence at 0, but the store may hold
/// committed truth from a previous process. Minting must skip such ids: a NEW
/// session must never graft onto an old thread's transcript (rehydration by
/// explicit id stays the only reattach path).
#[tokio::test]
async fn minting_skips_session_ids_that_own_committed_truth() {
    use awaken_agent_contract::agent::content::ContentBlock;

    struct HauntedRuntime;
    #[async_trait::async_trait]
    impl SessionRuntime for HauntedRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<TurnOutcome, RunError> {
            unreachable!("no turn in this test")
        }
        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: Decision,
        ) -> Result<TurnOutcome, RunError> {
            unreachable!()
        }
        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: &str,
            _is_error: bool,
        ) -> Result<TurnOutcome, RunError> {
            unreachable!()
        }
        async fn owns_thread(&self, thread: &str) -> bool {
            // A previous process persisted threads sesn_0 and sesn_1.
            thread == "sesn_0" || thread == "sesn_1"
        }
        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }
        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            unreachable!()
        }
        fn model(&self) -> String {
            "haunted".into()
        }
    }

    let state = ManagedState::new(HauntedRuntime);
    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                environment_id: None,
                title: None,
                metadata: Default::default(),
                mcp_servers: Vec::new(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            None,
        )
        .await
        .expect("create skips haunted ids");
    assert_eq!(session.id, "sesn_2", "sesn_0/sesn_1 own committed truth");
}

/// ADR-0048 / S10: creating a session fires the lifecycle projection sink with the
/// session's owner and the `session.status_idled` fact (the webhook catalog name,
/// matching Anthropic's official set) — the seam a webhook dispatcher hangs off,
/// projected out-of-band.
#[tokio::test]
async fn create_session_fires_the_lifecycle_sink_with_the_owner() {
    #[derive(Default)]
    struct CapturingSink {
        seen: Mutex<Vec<(String, Option<String>, String)>>,
    }
    #[async_trait::async_trait]
    impl SessionLifecycleSink for CapturingSink {
        async fn emit(&self, session_id: &str, workspace_id: Option<&str>, event_type: &str) {
            self.seen.lock().unwrap().push((
                session_id.to_string(),
                workspace_id.map(str::to_string),
                event_type.to_string(),
            ));
        }
    }

    let sink = Arc::new(CapturingSink::default());
    let state = ManagedState::new(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        fail_with: None,
    })
    .with_lifecycle_sink(sink.clone());

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                environment_id: None,
                title: None,
                metadata: Default::default(),
                mcp_servers: Vec::new(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            Some("wrkspc_acme".to_string()),
        )
        .await
        .expect("create session");

    let seen = sink.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "exactly one lifecycle fact emitted");
    assert_eq!(
        seen[0],
        (
            session.id.clone(),
            Some("wrkspc_acme".to_string()),
            "session.status_idled".to_string()
        ),
        "the sink sees the session, its owner, and the idled fact"
    );
}

/// Archiving a session fires the lifecycle sink with the `session.status_terminated`
/// fact and the session's owner — the terminal transition mirrors create's
/// `session.status_idled`. Idempotent: a second archive fans out no second event.
#[tokio::test]
async fn archive_session_fires_the_terminated_fact_once() {
    #[derive(Default)]
    struct CapturingSink {
        seen: Mutex<Vec<(String, Option<String>, String)>>,
    }
    #[async_trait::async_trait]
    impl SessionLifecycleSink for CapturingSink {
        async fn emit(&self, session_id: &str, workspace_id: Option<&str>, event_type: &str) {
            self.seen.lock().unwrap().push((
                session_id.to_string(),
                workspace_id.map(str::to_string),
                event_type.to_string(),
            ));
        }
    }

    let sink = Arc::new(CapturingSink::default());
    let state = ManagedState::new(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        fail_with: None,
    })
    .with_lifecycle_sink(sink.clone());

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                environment_id: None,
                title: None,
                metadata: Default::default(),
                mcp_servers: Vec::new(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            Some("wrkspc_acme".to_string()),
        )
        .await
        .expect("create session");

    // First archive → the terminated fact; second archive → nothing new.
    state.archive_session(&session.id).await.expect("archive");
    state
        .archive_session(&session.id)
        .await
        .expect("re-archive is idempotent");

    let seen = sink.seen.lock().unwrap();
    // create's idled, then exactly one terminated (not two).
    assert_eq!(seen.len(), 2, "idled on create, terminated once on archive");
    assert_eq!(
        seen[1],
        (
            session.id.clone(),
            Some("wrkspc_acme".to_string()),
            "session.status_terminated".to_string()
        ),
        "the sink sees the session, its owner, and the terminated fact",
    );
}
