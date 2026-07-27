//! Session-create MCP binding (ADR-0043 Phase 3): a session's `mcp_servers` are
//! bound to vault credentials by the canonical normalized-URL rule. Preparing,
//! frozen generation-1 state and realization claims are durable before Runtime
//! I/O; a failed realization fails the create with the mapped error envelope.

mod support;

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_credential_vault::InMemorySecretStore;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_protocol_managed::{
    ManagedSessionRepository, ManagedState, OutcomeReport, PersistedSession, RunError,
    RunErrorKind, SessionInit, SessionLifecycleSink, SessionRuntime,
    SqliteManagedSessionRepository, StepOutcome, ToolPermissionDecision, VaultState, router,
    vault_router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use support::{ScheduledConflictRepository, replace_session_fixture};

/// A fake runtime that records every `prepare_session` init (and optionally
/// fails it), so a test can assert exactly what a session create provisions.
struct PreparingFake {
    captured: Arc<Mutex<Vec<SessionInit>>>,
    staged: Arc<Mutex<Vec<awaken_session_contract::StageMcpAttachment>>>,
    observed_durable: Arc<Mutex<Vec<PersistedSession>>>,
    repo: Option<Arc<dyn ManagedSessionRepository>>,
    fail_with: Option<RunErrorKind>,
}

#[async_trait::async_trait]
impl SessionRuntime for PreparingFake {
    async fn prepare_session(&self, thread: &str, init: SessionInit) -> Result<(), RunError> {
        if let Some(repo) = &self.repo
            && let Some(session) = repo.get(thread).await
        {
            self.observed_durable.lock().unwrap().push(session);
        }
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
        _c: &str,
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[async_trait::async_trait]
impl awaken_protocol_managed::McpAttachmentRealizer for PreparingFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        self.staged.lock().unwrap().push(request.clone());
        let receipt_fingerprint = request.fingerprint();
        Ok(awaken_session_contract::McpRealizationReceipt {
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: Some(
                awaken_credential_contract::CredentialRealizationKind::WorkerRelay,
            ),
            receipt_fingerprint,
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

struct Harness {
    app: Router,
    vaults: Arc<VaultState>,
    captured: Arc<Mutex<Vec<SessionInit>>>,
    staged: Arc<Mutex<Vec<awaken_session_contract::StageMcpAttachment>>>,
    observed_durable: Arc<Mutex<Vec<PersistedSession>>>,
    repo: Arc<dyn ManagedSessionRepository>,
}

#[derive(Clone, Copy, Debug, Default)]
enum HotStageMode {
    #[default]
    Success,
    FailNext,
    MismatchNext,
}

#[derive(Default)]
struct HotRuntimeState {
    staged: Vec<awaken_session_contract::StageMcpAttachment>,
    published: Vec<awaken_session_contract::McpGenerationRef>,
    drained: Vec<awaken_session_contract::McpGenerationRef>,
    durable_at_stage: Vec<PersistedSession>,
    mode: HotStageMode,
    fail_publish_next: bool,
    fail_drain_next: bool,
    replaced_toolsets: Vec<(String, Vec<awaken_agent_contract::ToolsetPolicy>)>,
}

struct HotRuntime {
    state: Arc<Mutex<HotRuntimeState>>,
    repo: Arc<dyn ManagedSessionRepository>,
}

#[async_trait::async_trait]
impl SessionRuntime for HotRuntime {
    async fn prepare_session(&self, _thread: &str, _init: SessionInit) -> Result<(), RunError> {
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: &str,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn replace_session_toolsets(
        &self,
        thread: &str,
        toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    ) -> Result<(), RunError> {
        self.state
            .lock()
            .unwrap()
            .replaced_toolsets
            .push((thread.to_string(), toolsets));
        Ok(())
    }
    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "hot-model".into()
    }
}

#[async_trait::async_trait]
impl awaken_protocol_managed::McpAttachmentRealizer for HotRuntime {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        let durable = self
            .repo
            .get(&request.generation.session_id)
            .await
            .expect("stage follows durable claim");
        let mut state = self.state.lock().unwrap();
        state.durable_at_stage.push(durable);
        state.staged.push(request.clone());
        let mode = std::mem::take(&mut state.mode);
        if matches!(mode, HotStageMode::FailNext) {
            return Err(RunError::internal("hot stage failed"));
        }
        let receipt_fingerprint = request.fingerprint();
        let mut generation = request.generation;
        if matches!(mode, HotStageMode::MismatchNext) {
            generation.generation.0 += 1;
        }
        Ok(awaken_session_contract::McpRealizationReceipt {
            generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
            receipt_fingerprint,
        })
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        let mut state = self.state.lock().unwrap();
        state.published.push(generation);
        if std::mem::take(&mut state.fail_publish_next) {
            return Err(RunError::internal("hot publish failed"));
        }
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        let mut state = self.state.lock().unwrap();
        state.drained.push(generation);
        if std::mem::take(&mut state.fail_drain_next) {
            return Err(RunError::internal("hot drain failed"));
        }
        Ok(())
    }
}

struct HotHarness {
    app: Router,
    managed: Arc<ManagedState>,
    state: Arc<Mutex<HotRuntimeState>>,
    repo: Arc<dyn ManagedSessionRepository>,
}

fn hot_harness() -> HotHarness {
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open hot Session repository"),
    );
    hot_harness_with_repo(repo)
}

fn hot_harness_with_repo(repo: Arc<dyn ManagedSessionRepository>) -> HotHarness {
    let runtime_state = Arc::new(Mutex::new(HotRuntimeState::default()));
    let managed = Arc::new(
        ManagedState::new_with_mcp(HotRuntime {
            state: runtime_state.clone(),
            repo: repo.clone(),
        })
        .with_session_repo(repo.clone()),
    );
    HotHarness {
        app: router(managed.clone()),
        managed,
        state: runtime_state,
        repo,
    }
}

/// Sessions + vaults over ONE shared `VaultState`, the way the server mounts
/// them, so a credential entered through the vault routes is bindable at create.
fn harness(fail_with: Option<RunErrorKind>) -> Harness {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let vaults = Arc::new(VaultState::new(secrets, credentials));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let staged = Arc::new(Mutex::new(Vec::new()));
    let observed_durable = Arc::new(Mutex::new(Vec::new()));
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open Session repository"),
    );
    let state = ManagedState::new_with_mcp(PreparingFake {
        captured: captured.clone(),
        staged: staged.clone(),
        observed_durable: observed_durable.clone(),
        repo: Some(repo.clone()),
        fail_with,
    })
    .with_vaults(vaults.clone())
    .with_session_repo(repo.clone());
    let app = router(Arc::new(state)).merge(vault_router(vaults.clone()));
    Harness {
        app,
        vaults,
        captured,
        staged,
        observed_durable,
        repo,
    }
}

/// The shared bind-time check is fail-closed on an unknown vault, and a pre-flight
/// caller gets exactly the error `create_session` would — without minting a session.
#[test]
fn check_bind_is_fail_closed_on_unknown_vault() {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let vaults = Arc::new(VaultState::new(secrets, credentials));
    let state = ManagedState::new_with_mcp(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        staged: Arc::new(Mutex::new(Vec::new())),
        observed_durable: Arc::new(Mutex::new(Vec::new())),
        repo: None,
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
    let (status, _, body) = call_with_headers(app, method, uri, body, &[]).await;
    (status, body)
}

async fn call_with_headers(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        b = b.header(*name, *value);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, value)
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

async fn create_mcp_credential(
    h: &Harness,
    vault_id: &str,
    kind: &str,
    url: &str,
    secret: &str,
) -> String {
    let auth = match kind {
        "static_bearer" => json!({
            "type": kind,
            "mcp_server_url": url,
            "token": secret
        }),
        "mcp_oauth" => json!({
            "type": kind,
            "mcp_server_url": url,
            "access_token": secret
        }),
        _ => panic!("unsupported test credential kind: {kind}"),
    };
    let (status, credential) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(auth),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    credential["id"].as_str().unwrap().to_string()
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

    // The exact-generation stage saw the secret-free credential access; the
    // baseline-only prepare call carries no parallel MCP authority.
    let expected = h
        .vaults
        .credential_source_id(&vault_id, &cred_id)
        .expect("wire credential maps to a domain source");
    {
        let captured = h.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].agent_id, "calc-agent");
    }
    {
        let staged = h.staged.lock().unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].name, "calc");
        assert_eq!(staged[0].target.url, MCP_URL);
        // The binding carries the neutral row-id string (the port speaks no vault vocab).
        assert_eq!(
            staged[0]
                .credential
                .as_ref()
                .map(|access| access.credential.id.as_str()),
            Some(expected.0.as_str())
        );
        assert_eq!(
            staged[0]
                .credential
                .as_ref()
                .map(|access| access.credential.revision),
            Some(1),
            "Session-inline Vault selection is compiled to one exact credential revision"
        );
        // The credential was entered without a refresh object, so the binding
        // carries no refresh configuration.
        assert!(
            staged[0]
                .credential
                .as_ref()
                .and_then(|access| access.refresh.as_ref())
                .is_none()
        );
    }

    // Cause graph: exact generation is Requested -> lease+claim root CAS ->
    // Runtime stage -> success/failure -> terminal activation CAS.
    //
    // | Rule | generation requested | claim durable before I/O | stage result | Effect |
    // |---|---|---|---|---|
    // | A1 | T | T | success | Active + visible |
    // | A2 | T | T | failure | Failed + hidden |
    // | A3 | F | - | success | no attachment effect |
    {
        let observed = h.observed_durable.lock().unwrap();
        assert_eq!(observed.len(), 1, "A1");
        assert_eq!(
            observed[0].revision,
            awaken_session_contract::SessionRevision(3),
            "A1 insert(Preparing) -> finalize generation 1 -> claim all commit before Runtime I/O"
        );
        assert!(matches!(
            observed[0].baseline,
            awaken_session_contract::SessionBaselineState::Frozen(_)
        ));
        assert!(observed[0].realization.is_some(), "A1 lease is durable");
        assert_eq!(
            observed[0].mcp.attachments[0].state,
            awaken_session_contract::McpAttachmentState::Realizing,
            "A1 claim is durable before Runtime I/O"
        );
    }
    let active = h.repo.get("sesn_0").await.expect("active aggregate");
    assert_eq!(
        active.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Active,
        "A1"
    );
    assert!(
        active.mcp.attachments[0].publication_acknowledged,
        "A1 publication acknowledgement is durable before success"
    );
    assert_eq!(active.visible_mcp_servers().len(), 1, "A1");
}

#[tokio::test]
async fn session_binding_supports_static_bearer_and_normalized_mcp_urls() {
    let h = harness(None);
    let (status, vault) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "bearer" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let credential_id = create_mcp_credential(
        &h,
        &vault_id,
        "static_bearer",
        "HTTPS://MCP.EXAMPLE.COM:443/sse/",
        "static-secret", // awaken-allow: secret
    )
    .await;

    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "calc-agent",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": "https://mcp.example.com/sse" }],
            "vault_ids": [vault_id.clone()]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let expected = h
        .vaults
        .credential_source_id(&vault_id, &credential_id)
        .unwrap();
    let staged = h.staged.lock().unwrap();
    assert_eq!(
        staged[0]
            .credential
            .as_ref()
            .map(|access| access.credential.id.as_str()),
        Some(expected.0.as_str())
    );
}

#[tokio::test]
async fn session_binding_honors_vault_order_and_leaves_a_miss_unauthenticated() {
    let h = harness(None);
    let mut vaults = Vec::new();
    let mut sources = Vec::new();
    for name in ["first-created", "second-created"] {
        let (status, vault) = call(
            &h.app,
            "POST",
            "/v1/vaults",
            Some(json!({ "display_name": name })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let vault_id = vault["id"].as_str().unwrap().to_string();
        let credential_id = create_mcp_credential(
            &h,
            &vault_id,
            "mcp_oauth",
            MCP_URL,
            name, // awaken-allow: secret
        )
        .await;
        sources.push(
            h.vaults
                .credential_source_id(&vault_id, &credential_id)
                .unwrap(),
        );
        vaults.push(vault_id);
    }

    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "ordered",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": MCP_URL }],
            "vault_ids": [vaults[1].clone(), vaults[0].clone()]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.staged.lock().unwrap()[0]
            .credential
            .as_ref()
            .map(|access| access.credential.id.as_str()),
        Some(sources[1].0.as_str())
    );

    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "anonymous",
            "mcp_servers": [{ "name": "other", "type": "url", "url": "https://other.example.com/mcp" }],
            "vault_ids": [vaults[0].clone()]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        h.staged.lock().unwrap()[1].credential.is_none(),
        "Anthropic-compatible optional MCP auth leaves a non-match unauthenticated"
    );
}

#[tokio::test]
async fn an_archived_vault_cannot_be_attached_to_a_new_session() {
    let h = harness(None);
    let (vault_id, _) = vault_with_mcp_oauth(&h).await;
    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "calc-agent",
            "mcp_servers": [{ "name": "calc", "type": "url", "url": MCP_URL }],
            "vault_ids": [vault_id]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(h.captured.lock().unwrap().is_empty());
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
    let staged = h.staged.lock().unwrap();
    let refresh = staged[0]
        .credential
        .as_ref()
        .and_then(|access| access.refresh.as_ref())
        .expect("a refreshable credential's binding carries its refresh config");
    assert_eq!(refresh.token_endpoint, "https://auth.example.com/token");
    assert_eq!(refresh.client_id, "cli_pub");
    assert_eq!(refresh.scope.as_deref(), Some("mcp:read"));
    assert_eq!(refresh.resource, None);
    let source_id = h.vaults.credential_source_id(&vault_id, &cred_id).unwrap();
    assert_eq!(
        refresh.refresh_token_ref,
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
    assert!(h.staged.lock().unwrap().is_empty());
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
            Some(json!({
                "agent": "calc-agent",
                "mcp_servers": [{"type": "url", "name": "calc", "url": MCP_URL}]
            })),
        )
        .await;
        assert_eq!(s, status, "{kind:?}");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], error_type);
        // Fail closed: the failed create left no session behind.
        let (s, _) = call(&h.app, "GET", "/v1/sessions/sesn_0", None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "no half-provisioned session");
        let failed = h
            .repo
            .get("sesn_0")
            .await
            .expect("recoverable failed intent");
        assert_eq!(
            failed.mcp.attachments[0].state,
            awaken_session_contract::McpAttachmentState::Failed,
            "A2: {kind:?}"
        );
        assert!(failed.visible_mcp_servers().is_empty(), "A2: {kind:?}");
    }
}

#[tokio::test]
async fn create_without_mcp_has_no_attachment_effect() {
    let h = harness(None);
    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "plain-agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "A3");
    assert_eq!(body["agent"]["mcp_servers"], json!([]), "A3");
    let durable = h.repo.get("sesn_0").await.expect("A3 aggregate");
    assert!(durable.mcp.attachments.is_empty(), "A3");
}

/// Causal graph for the public full-replacement command:
///
/// ```text
/// parse + normalize desired set
///   -> same recoverable fingerprint? --yes--> no-op
///   -> root-CAS Requested intent
///   -> root-CAS exact lease claim
///   -> stage + validate receipt
///      -> failure/mismatch: Failed; keep previous Active
///      -> success: activation CAS -> publish -> exact old drain -> Removed
/// ```
///
/// The cases below are generated from this decision table, in rule order:
///
/// | Rule | desired | current | stage | receipt | Effect |
/// |---|---|---|---|---|---|
/// | H1 | same | Active | - | - | convergent no-op; no new generation/effect |
/// | H2 | add | absent | success | exact | gen1 Active; publish |
/// | H3 | replace | Active gen1 | success | exact | gen2 Active; gen1 drain+Removed |
/// | H4 | remove | Active | - | - | exact drain+Removed; empty projection |
/// | H5 | replace | Active gen1 | failure | - | gen2 Failed; gen1 remains Active |
/// | H6 | retry same | Failed gen2 + Active gen1 | success | exact | gen3 Active |
/// | H7 | add | absent | success | stale | Failed; hidden; compensating drain |
/// | H8 | malformed | any | - | - | 400; no Runtime effect |
#[tokio::test]
async fn hot_mcp_replacement_tests_are_generated_from_decision_table() {
    let h = hot_harness();
    let (status, created) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "hot-agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();

    let server_a = json!({"name": "calc", "type": "url", "url": "https://a.example/mcp"});
    let (status, added) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [server_a.clone()]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H2");
    assert_eq!(
        added["agent"]["mcp_servers"],
        json!([server_a.clone()]),
        "H2"
    );
    {
        let state = h.state.lock().unwrap();
        assert_eq!(state.staged.len(), 1, "H2");
        assert_eq!(state.staged[0].generation.generation.0, 1, "H2");
        assert_eq!(state.published.len(), 1, "H2");
        assert_eq!(
            state.durable_at_stage[0].mcp.attachments[0].state,
            awaken_session_contract::McpAttachmentState::Realizing,
            "H2 claim before I/O"
        );
    }

    let before = {
        let state = h.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };
    let (status, replayed) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [server_a.clone()]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H1");
    assert_eq!(replayed["agent"]["mcp_servers"], json!([server_a]), "H1");
    {
        let state = h.state.lock().unwrap();
        assert_eq!(
            (
                state.staged.len(),
                state.published.len(),
                state.drained.len()
            ),
            before,
            "H1"
        );
    }

    let server_b = json!({"name": "calc", "type": "url", "url": "https://b.example/mcp"});
    let (status, replaced) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [server_b.clone()]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H3");
    assert_eq!(replaced["agent"]["mcp_servers"], json!([server_b]), "H3");
    {
        let state = h.state.lock().unwrap();
        assert_eq!(
            state.staged.last().unwrap().generation.generation.0,
            2,
            "H3"
        );
        assert_eq!(state.published.last().unwrap().generation.0, 2, "H3");
        assert_eq!(state.drained.last().unwrap().generation.0, 1, "H3");
    }
    let durable = h.repo.get(id).await.expect("H3 durable");
    assert_eq!(durable.visible_mcp_servers().len(), 1, "H3");
    assert_eq!(
        durable.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Removed,
        "H3"
    );
    assert_eq!(
        durable.mcp.attachments[1].state,
        awaken_session_contract::McpAttachmentState::Active,
        "H3"
    );

    let (status, removed) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H4");
    assert_eq!(removed["agent"]["mcp_servers"], json!([]), "H4");
    assert_eq!(
        h.state.lock().unwrap().drained.last().unwrap().generation.0,
        2,
        "H4"
    );

    let failed = hot_harness();
    let (status, created) = call(
        &failed.app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "hot-agent",
            "mcp_servers": [{"type": "url", "name": "calc", "url": "https://old.example/mcp"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let failed_id = created["id"].as_str().unwrap();
    failed.state.lock().unwrap().mode = HotStageMode::FailNext;
    let desired = json!({"name": "calc", "type": "url", "url": "https://new.example/mcp"});
    let (status, _) = call(
        &failed.app,
        "POST",
        &format!("/v1/sessions/{failed_id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "H5");
    let durable = failed.repo.get(failed_id).await.expect("H5 durable");
    assert_eq!(
        durable.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Active,
        "H5"
    );
    assert_eq!(
        durable.mcp.attachments[1].state,
        awaken_session_contract::McpAttachmentState::Failed,
        "H5"
    );
    assert_eq!(
        durable.visible_mcp_servers()[0].url,
        "https://old.example/mcp",
        "H5"
    );

    let (status, retried) = call(
        &failed.app,
        "POST",
        &format!("/v1/sessions/{failed_id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H6");
    assert_eq!(retried["agent"]["mcp_servers"], json!([desired]), "H6");
    assert_eq!(
        failed
            .state
            .lock()
            .unwrap()
            .staged
            .last()
            .unwrap()
            .generation
            .generation
            .0,
        3,
        "H6"
    );

    let mismatch = hot_harness();
    let (status, created) = call(
        &mismatch.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "hot-agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mismatch_id = created["id"].as_str().unwrap();
    mismatch.state.lock().unwrap().mode = HotStageMode::MismatchNext;
    let (status, _) = call(
        &mismatch.app,
        "POST",
        &format!("/v1/sessions/{mismatch_id}"),
        Some(
            json!({"agent": {"mcp_servers": [{"type": "url", "name": "bad", "url": "https://bad.example/mcp"}]}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "H7");
    let durable = mismatch.repo.get(mismatch_id).await.expect("H7 durable");
    assert_eq!(
        durable.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Failed,
        "H7"
    );
    assert!(durable.visible_mcp_servers().is_empty(), "H7");

    let effects = {
        let state = mismatch.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };
    let (status, _) = call(
        &mismatch.app,
        "POST",
        &format!("/v1/sessions/{mismatch_id}"),
        Some(json!({"agent": {"mcp_servers": [{"type": "url", "name": "missing-url"}]}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "H8");
    let state = mismatch.state.lock().unwrap();
    assert_eq!(
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len()
        ),
        effects,
        "H8"
    );
}

/// Recovery uses the same stage/publish/drain coordinator as create and hot
/// replacement. These cases are generated from the durable-state decision table:
///
/// ```text
/// repository recovery index -> nonterminal state
///   Requested/Realizing/Active -> new lease claim -> stage -> activate if needed -> publish
///   Draining -> exact idempotent drain -> Removed
///   Failed/Removed -> absent from index
/// ```
///
/// | Rule | Durable state | Runtime result | Effect |
/// |---|---|---|---|
/// | R1 | Requested | success | Active + published under new lease |
/// | R2 | Active + expired lease | success | restaged + republished; remains Active |
/// | R3 | Draining | success/unknown | Removed; no stage/publish |
/// | R4 | Removed | - | not selected; no Runtime effect |
/// | R5 | Requested on idle Session | stage failure | terminal Failed; Session remains idle |
/// | R6 | prior R5 Failed | same desired command | generation N+1 becomes Active |
#[tokio::test]
async fn mcp_recovery_tests_are_generated_from_decision_table() {
    let h = hot_harness();
    let (status, created) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "recovery-agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let mut requested = h.repo.get(id).await.unwrap();
    requested
        .mcp
        .request_full_replacement(
            vec![awaken_session_contract::McpAttachmentDraft {
                name: "docs".into(),
                target: awaken_session_contract::McpTarget::parse_http("https://docs.example/mcp")
                    .unwrap(),
                credential: None,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap();
    replace_session_fixture(
        h.repo.as_ref(),
        "default",
        requested,
        "test:recovery-requested",
    )
    .await;

    assert_eq!(h.managed.reconcile_mcp_attachments().await, 1, "R1");
    let mut active = h.repo.get(id).await.unwrap();
    assert_eq!(
        active.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Active,
        "R1"
    );
    let first_epoch = active.realization.as_ref().unwrap().epoch;
    assert_eq!(h.state.lock().unwrap().published.len(), 1, "R1");

    active.realization.as_mut().unwrap().expires_at_unix_ms = 0;
    replace_session_fixture(
        h.repo.as_ref(),
        "default",
        active,
        "test:recovery-expired-lease",
    )
    .await;
    assert_eq!(h.managed.reconcile_mcp_attachments().await, 1, "R2");
    let active = h.repo.get(id).await.unwrap();
    assert!(
        active.realization.as_ref().unwrap().epoch > first_epoch,
        "R2"
    );
    assert_eq!(
        active.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Active,
        "R2"
    );
    assert_eq!(h.state.lock().unwrap().published.len(), 2, "R2");

    let mut draining = active;
    let attachment_id = draining.mcp.attachments[0].attachment_id.clone();
    draining
        .mcp
        .begin_drain(&attachment_id, awaken_session_contract::McpGeneration(1))
        .unwrap();
    replace_session_fixture(
        h.repo.as_ref(),
        "default",
        draining,
        "test:recovery-draining",
    )
    .await;
    let staged_before = h.state.lock().unwrap().staged.len();
    assert_eq!(h.managed.reconcile_mcp_attachments().await, 1, "R3");
    let removed = h.repo.get(id).await.unwrap();
    assert_eq!(
        removed.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Removed,
        "R3"
    );
    assert_eq!(h.state.lock().unwrap().staged.len(), staged_before, "R3");

    let effects = {
        let state = h.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };
    assert_eq!(h.managed.reconcile_mcp_attachments().await, 0, "R4");
    {
        let state = h.state.lock().unwrap();
        assert_eq!(
            (
                state.staged.len(),
                state.published.len(),
                state.drained.len()
            ),
            effects,
            "R4"
        );
    }

    let (status, created) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "retry-agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let retry_id = created["id"].as_str().unwrap();
    let mut retry = h.repo.get(retry_id).await.unwrap();
    retry
        .mcp
        .request_full_replacement(
            vec![awaken_session_contract::McpAttachmentDraft {
                name: "retry".into(),
                target: awaken_session_contract::McpTarget::parse_http("https://retry.example/mcp")
                    .unwrap(),
                credential: None,
                origin: awaken_session_contract::McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap();
    replace_session_fixture(h.repo.as_ref(), "default", retry, "test:recovery-retry").await;
    h.state.lock().unwrap().mode = HotStageMode::FailNext;
    assert_eq!(h.managed.reconcile_mcp_attachments().await, 0, "R5");
    let failed_retry = h.repo.get(retry_id).await.unwrap();
    assert_eq!(
        failed_retry.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Failed,
        "R5"
    );
    assert_eq!(failed_retry.status, "idle", "R5");

    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{retry_id}"),
        Some(json!({"agent": {"mcp_servers": [{
            "type": "url",
            "name": "retry",
            "url": "https://retry.example/mcp"
        }]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R6");
    let retried = h.repo.get(retry_id).await.unwrap();
    assert_eq!(retried.mcp.attachments.len(), 2, "R6");
    assert_eq!(
        retried.mcp.attachments[1].state,
        awaken_session_contract::McpAttachmentState::Active,
        "R6"
    );
    assert_eq!(retried.mcp.attachments[1].generation.0, 2, "R6");
}

/// External acknowledgement gaps are derived from this cause graph:
///
/// ```text
/// activation CAS -> publish
///   fail -> Active + unacknowledged -> same desired retry restages/publishes
/// drain command
///   fail -> Draining -> same desired retry drains idempotently -> Removed
/// ```
///
/// | Rule | Durable state | Failed effect | Retry | Effect |
/// |---|---|---|---|---|
/// | P1 | Active/unacknowledged | publish | none | error; no false success |
/// | P2 | Active/unacknowledged | prior publish | same desired | same gen published+acknowledged |
/// | P3 | Draining | drain | none | error; cleanup remains durable |
/// | P4 | Draining | prior drain | same desired | exact drain replay + Removed |
#[tokio::test]
async fn publication_and_drain_gap_tests_are_generated_from_decision_table() {
    let h = hot_harness();
    let (status, created) = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "gap-agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let desired = json!({"name": "gap", "type": "url", "url": "https://gap.example/mcp"});

    h.state.lock().unwrap().fail_publish_next = true;
    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "P1");
    let unacknowledged = h.repo.get(id).await.unwrap();
    assert_eq!(
        unacknowledged.mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Active,
        "P1"
    );
    assert!(
        !unacknowledged.mcp.attachments[0].publication_acknowledged,
        "P1"
    );

    let (status, recovered) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P2");
    assert_eq!(recovered["agent"]["mcp_servers"], json!([desired]), "P2");
    let acknowledged = h.repo.get(id).await.unwrap();
    assert_eq!(acknowledged.mcp.attachments.len(), 1, "P2 same generation");
    assert!(
        acknowledged.mcp.attachments[0].publication_acknowledged,
        "P2"
    );

    h.state.lock().unwrap().fail_drain_next = true;
    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "P3");
    assert_eq!(
        h.repo.get(id).await.unwrap().mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Draining,
        "P3"
    );

    let (status, removed) = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P4");
    assert_eq!(removed["agent"]["mcp_servers"], json!([]), "P4");
    assert_eq!(
        h.repo.get(id).await.unwrap().mcp.attachments[0].state,
        awaken_session_contract::McpAttachmentState::Removed,
        "P4"
    );
}

/// HTTP command reliability is generated from this graph:
///
/// ```text
/// parse headers -> prior durable receipt?
///   same key/hash -> replay; different hash -> conflict
///   absent -> If-Match exact? -> bounded CAS command -> atomic final receipt
/// ```
///
/// | Rule | Key | Hash | If-Match | Effect |
/// |---|---|---|---|---|
/// | I1 | new | same | exact | apply once + ETag + operation id |
/// | I2 | same | same | omitted | replay; revision/effects unchanged |
/// | I2b | same | same | later root revision | replay original command revision |
/// | I3 | same | different | omitted | 409; no effect |
/// | I4 | absent | - | stale | 409 before Runtime effect |
/// | I5 | absent | - | malformed | 400 before Runtime effect |
/// | I6 | absent | - | exact | mutable tools persist in root aggregate |
#[tokio::test]
async fn update_precondition_and_idempotency_tests_are_generated_from_decision_table() {
    let h = hot_harness();
    let (status, create_headers, created) = call_with_headers(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "idempotent-agent"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let etag = create_headers["etag"].to_str().unwrap().to_string();
    let desired = json!({"name": "idem", "type": "url", "url": "https://idem.example/mcp"});

    let before = h.state.lock().unwrap().staged.len();
    let (status, _, _) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("if-match", "\"0\"")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "I4");
    assert_eq!(h.state.lock().unwrap().staged.len(), before, "I4");

    let (status, applied_headers, applied) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("idempotency-key", "command-1"), ("if-match", &etag)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "I1");
    assert_eq!(
        applied["agent"]["mcp_servers"],
        json!([desired.clone()]),
        "I1"
    );
    assert!(applied_headers.contains_key("etag"), "I1");
    assert!(applied_headers.contains_key("x-awaken-operation-id"), "I1");
    let applied_etag = applied_headers["etag"].clone();
    let effects = {
        let state = h.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };

    let (status, replay_headers, replayed) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("idempotency-key", "command-1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "I2");
    assert_eq!(
        replayed["agent"]["mcp_servers"],
        json!([desired.clone()]),
        "I2"
    );
    assert_eq!(replay_headers["etag"], applied_etag, "I2");
    {
        let state = h.state.lock().unwrap();
        assert_eq!(
            (
                state.staged.len(),
                state.published.len(),
                state.drained.len()
            ),
            effects,
            "I2"
        );
    }

    let (status, later_headers, later) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"title": "later root fact"})),
        &[("if-match", replay_headers["etag"].to_str().unwrap())],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "I2b setup");
    assert_ne!(later_headers["etag"], applied_etag, "I2b setup");
    assert_eq!(later["title"], "later root fact", "I2b setup");
    let (status, historical_headers, historical) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("idempotency-key", "command-1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "I2b");
    assert_eq!(historical_headers["etag"], applied_etag, "I2b");
    assert_eq!(historical["title"], "later root fact", "I2b current body");

    let (status, _, _) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
        &[("idempotency-key", "command-1")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "I3");

    let (status, _, _) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"title": "invalid precondition"})),
        &[("if-match", "not-an-etag")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "I5");

    let current_etag = later_headers["etag"].to_str().unwrap();
    // Cause graph: Session toolset update -> durable exact projection -> runtime
    // policy replacement -> disposable runtime rebuild on the next turn.
    //
    // Decision table:
    // | update field | durable revision | runtime replacement |
    // | omitted      | unchanged        | none                |
    // | exact value  | incremented      | exact policy once   |
    // | replay       | original receipt | none duplicated     |
    let tools = json!([{
        "type": "agent_toolset_20260401",
        "configs": [{
            "name": "write",
            "enabled": false,
            "permission_policy": { "type": "always_allow" }
        }],
        "default_config": {
            "enabled": true,
            "permission_policy": { "type": "always_ask" }
        }
    }]);
    let (status, _, updated) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"tools": tools.clone()}})),
        &[("if-match", current_etag)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "I6");
    assert_eq!(updated["agent"]["tools"], tools, "I6");
    assert_eq!(
        h.repo.get(id).await.unwrap().agent_tools,
        serde_json::from_value::<Vec<awaken_session_contract::AgentTool>>(tools).unwrap(),
        "I6"
    );
    let state = h.state.lock().unwrap();
    assert_eq!(state.replaced_toolsets.len(), 1, "I6 runtime effect");
    assert_eq!(state.replaced_toolsets[0].0, id, "I6 exact Session");
    let write = state.replaced_toolsets[0].1[0].policy_for("write");
    assert!(!write.enabled, "I6 exact disabled policy reaches runtime");
}

/// CAS retry decisions are generated from `conflict × explicit precondition`:
///
/// | Rule | First root CAS | If-Match | Effect |
/// |---|---|---|---|
/// | C1 | conflict | absent | re-read, reapply same command, one generation/effect |
/// | C2 | conflict | exact at request start | return 409; do not weaken caller fence |
/// | C3 | publication-ack CAS conflict | absent | recover same generation; no duplicate |
/// | C4 | activation CAS conflict | exact at request start | drain staged generation + 409 |
#[tokio::test]
async fn update_cas_retry_tests_are_generated_from_decision_table() {
    let inner = Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open conflict repository"),
    );
    let conflicts = Arc::new(ScheduledConflictRepository::new(inner));
    let h = hot_harness_with_repo(conflicts.clone());
    let (status, _, created) = call_with_headers(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "cas-agent"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();

    conflicts.conflict_on_next(1);
    let desired = json!({"name": "cas", "type": "url", "url": "https://cas.example/mcp"});
    let (status, _, updated) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C1");
    assert_eq!(updated["agent"]["mcp_servers"], json!([desired]), "C1");
    assert_eq!(h.state.lock().unwrap().staged.len(), 1, "C1");
    assert_eq!(h.repo.get(id).await.unwrap().mcp.attachments.len(), 1, "C1");

    let (status, get_headers, _) =
        call_with_headers(&h.app, "GET", &format!("/v1/sessions/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let etag = get_headers["etag"].to_str().unwrap();
    let effects = h.state.lock().unwrap().staged.len();
    conflicts.conflict_on_next(1);
    let (status, _, _) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"title": "must-not-apply"})),
        &[("if-match", etag)],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "C2");
    assert_eq!(h.state.lock().unwrap().staged.len(), effects, "C2");
    assert_ne!(
        h.repo.get(id).await.unwrap().title.as_deref(),
        Some("must-not-apply"),
        "C2"
    );

    let replacement = json!({"name": "cas", "type": "url", "url": "https://cas-2.example/mcp"});
    conflicts.conflict_on_next(4);
    let staged_before = h.state.lock().unwrap().staged.len();
    let (status, _, updated) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [replacement.clone()]}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C3");
    assert_eq!(updated["agent"]["mcp_servers"], json!([replacement]), "C3");
    let durable = h.repo.get(id).await.unwrap();
    assert_eq!(
        durable.mcp.attachments.len(),
        2,
        "C3 no duplicate generation"
    );
    assert!(durable.mcp.attachments[1].publication_acknowledged, "C3");
    assert_eq!(
        h.state.lock().unwrap().staged.len(),
        staged_before + 2,
        "C3 restages exact gen2"
    );

    let (status, get_headers, _) =
        call_with_headers(&h.app, "GET", &format!("/v1/sessions/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let etag = get_headers["etag"].to_str().unwrap();
    conflicts.conflict_on_next(3);
    let drain_before = h.state.lock().unwrap().drained.len();
    let (status, _, _) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [{"type": "url", "name": "cas", "url": "https://cas-3.example/mcp"}]}})),
        &[("if-match", etag)],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "C4");
    let durable = h.repo.get(id).await.unwrap();
    assert_eq!(
        durable.mcp.attachments.last().unwrap().state,
        awaken_session_contract::McpAttachmentState::Realizing,
        "C4"
    );
    assert_eq!(
        h.state.lock().unwrap().drained.len(),
        drain_before + 1,
        "C4 compensation"
    );
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
        ) -> Result<StepOutcome, RunError> {
            unreachable!("no turn in this test")
        }
        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: &str,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
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
                application_contribution_required: false,
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

/// Cause graph for the public creation switch:
/// C1 = a registered application contribution is required. C1 freezes only the
/// preparation intent; !C1 compiles and realizes immediately. The two effects
/// are mutually exclusive, so creation cannot publish idle while still waiting
/// for an application or invoke Runtime before that contribution exists.
///
/// | Rule | C1 | wire status | Runtime prepare | idled fact | contribution |
/// |---|---|---|---|---|---|
/// | A1 | 0 | idle | once | once | NotRequired |
/// | A2 | 1 | preparing | never | never | Accepted |
#[tokio::test]
async fn application_required_creation_is_generated_from_the_decision_table() {
    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl SessionLifecycleSink for CapturingSink {
        async fn emit(&self, _session_id: &str, _workspace_id: Option<&str>, event_type: &str) {
            self.0.lock().unwrap().push(event_type.to_string());
        }
    }

    for (required, expected_status, expected_prepares, expected_facts, rule) in
        [(false, "idle", 1, 1, "A1"), (true, "preparing", 0, 0, "A2")]
    {
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(CapturingSink::default());
        let state = ManagedState::new_with_mcp(PreparingFake {
            captured: prepared.clone(),
            staged: Arc::new(Mutex::new(Vec::new())),
            observed_durable: Arc::new(Mutex::new(Vec::new())),
            repo: None,
            fail_with: None,
        })
        .with_lifecycle_sink(sink.clone());
        let session = state
            .create_session(
                awaken_protocol_managed::types::SessionCreateParams {
                    agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                    application_contribution_required: required,
                    environment_id: None,
                    title: None,
                    metadata: Default::default(),
                    mcp_servers: Vec::new(),
                    vault_ids: Vec::new(),
                    resources: Vec::new(),
                },
                Some("workspace-application".into()),
            )
            .await
            .unwrap_or_else(|error| panic!("{rule}: {error:?}"));
        assert_eq!(session.status, expected_status, "{rule}");
        assert_eq!(prepared.lock().unwrap().len(), expected_prepares, "{rule}");
        assert_eq!(sink.0.lock().unwrap().len(), expected_facts, "{rule}");

        let contribution = awaken_protocol_managed::ApplicationSessionContribution {
            session_id: session.id,
            application_fingerprint: "application-v1".into(),
            input: Default::default(),
        };
        let outcome =
            awaken_protocol_managed::ApplicationSessionContributionPort::contribute_application(
                &state,
                contribution,
            )
            .await;
        if required {
            assert!(outcome.is_ok(), "{rule}");
        } else {
            assert!(
                matches!(
                    outcome,
                    Err(
                        awaken_protocol_managed::ApplicationSessionContributionFailure::NotRequired
                    )
                ),
                "{rule}"
            );
        }
    }
}

/// A Preparing Session has no hidden wall-clock owner: it remains inert until
/// the claim-fenced contribution arrives, or the ordinary Session delete command
/// terminates it. Delete is the one cancellation path and a later contribution
/// cannot resurrect the aggregate.
#[tokio::test]
async fn preparing_session_can_be_cancelled_without_runtime_realization() {
    // Causal graph:
    // create(required) -> Preparing --contribution--> Frozen/idle
    //                              \--delete-------> tombstone/not found
    //
    // | current   | trigger              | Runtime prepare | terminal result |
    // | Preparing | no command           | never           | remains waiting |
    // | Preparing | valid contribution   | once            | idle            |
    // | Preparing | delete               | never           | not found       |
    // | deleted   | late contribution    | never           | NotFound        |
    let prepared = Arc::new(Mutex::new(Vec::new()));
    let state = ManagedState::new_with_mcp(PreparingFake {
        captured: prepared.clone(),
        staged: Arc::new(Mutex::new(Vec::new())),
        observed_durable: Arc::new(Mutex::new(Vec::new())),
        repo: None,
        fail_with: None,
    });
    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                application_contribution_required: true,
                environment_id: None,
                title: None,
                metadata: Default::default(),
                mcp_servers: Vec::new(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            Some("workspace-application".into()),
        )
        .await
        .expect("create preparing Session");
    assert_eq!(session.status, "preparing");
    assert!(prepared.lock().unwrap().is_empty());

    state
        .delete_session(&session.id)
        .await
        .expect("delete is the Preparing cancellation command");
    assert!(matches!(
        state.get_session(&session.id),
        Err(awaken_protocol_managed::StateError::NotFound)
    ));
    let late = awaken_protocol_managed::ApplicationSessionContributionPort::contribute_application(
        &state,
        awaken_protocol_managed::ApplicationSessionContribution {
            session_id: session.id,
            application_fingerprint: "late-plan".into(),
            input: Default::default(),
        },
    )
    .await;
    assert!(matches!(
        late,
        Err(awaken_protocol_managed::ApplicationSessionContributionFailure::NotFound)
    ));
    assert!(prepared.lock().unwrap().is_empty());
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
    let state = ManagedState::new_with_mcp(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        staged: Arc::new(Mutex::new(Vec::new())),
        observed_durable: Arc::new(Mutex::new(Vec::new())),
        repo: None,
        fail_with: None,
    })
    .with_lifecycle_sink(sink.clone());

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                application_contribution_required: false,
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
    let state = ManagedState::new_with_mcp(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        staged: Arc::new(Mutex::new(Vec::new())),
        observed_durable: Arc::new(Mutex::new(Vec::new())),
        repo: None,
        fail_with: None,
    })
    .with_lifecycle_sink(sink.clone());

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                application_contribution_required: false,
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

/// Deleting a session fires the lifecycle sink with the `session.deleted` fact and
/// the session's owner — a webhook subscriber is notified of a deletion just as it
/// is of create (`session.status_idled`) and archive (`session.status_terminated`).
/// The owner is still resolvable because delete drops the record but not the owner
/// index.
#[tokio::test]
async fn delete_session_fires_the_deleted_fact_with_the_owner() {
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
    let state = ManagedState::new_with_mcp(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        staged: Arc::new(Mutex::new(Vec::new())),
        observed_durable: Arc::new(Mutex::new(Vec::new())),
        repo: None,
        fail_with: None,
    })
    .with_lifecycle_sink(sink.clone());

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                application_contribution_required: false,
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

    state.delete_session(&session.id).await.expect("delete");

    let seen = sink.seen.lock().unwrap();
    // create's idled, then the deleted fact.
    assert_eq!(seen.len(), 2, "idled on create, deleted on delete");
    assert_eq!(
        seen[1],
        (
            session.id.clone(),
            Some("wrkspc_acme".to_string()),
            "session.deleted".to_string()
        ),
        "the sink sees the session, its owner, and the deleted fact",
    );
}
