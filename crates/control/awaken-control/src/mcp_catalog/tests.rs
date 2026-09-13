use super::*;
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{InMemorySecretStore, repo::InMemoryCredentialRepo};
use awaken_session_contract::{McpProbe, McpProbeStatus};
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;
#[derive(Default)]
struct Probe(AtomicUsize);
#[async_trait::async_trait]
impl McpProbe for Probe {
    async fn probe(&self, _: &str, _: &RedactedString) -> McpProbeStatus {
        self.0.fetch_add(1, Ordering::SeqCst);
        McpProbeStatus::Valid
    }
    async fn inspect(
        &self,
        url: &str,
        bearer: Option<&RedactedString>,
    ) -> awaken_session_contract::McpInspection {
        let bearer = bearer.unwrap();
        let status = self.probe(url, bearer).await;
        awaken_session_contract::McpInspection {
            status,
            tools: vec![awaken_session_contract::McpProbeTool {
                name: "read".into(),
                description: Some(bearer.expose_secret().into()),
                input_schema: json!({"type":"object","description":bearer.expose_secret()}),
            }],
        }
    }
}
struct Harness {
    app: Router,
    store: Arc<awaken_admin_config_api::SqliteAdminStore>,
    probe: Arc<Probe>,
    _temp: tempfile::TempDir,
}
impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            awaken_admin_config_api::SqliteAdminStore::open(
                temp.path().join("admin.sqlite").to_str().unwrap(),
            )
            .unwrap(),
        );
        let probe = Arc::new(Probe::default());
        let vault = Arc::new(
            VaultState::new(
                Arc::new(InMemorySecretStore::new()),
                Arc::new(InMemoryCredentialRepo::new()),
            )
            .with_probe(probe.clone()),
        );
        let app = router(McpCatalogService::new(store.clone(), vault.clone()))
            .merge(awaken_protocol_managed::vault_router(vault));
        Self {
            app,
            store,
            probe,
            _temp: temp,
        }
    }
    async fn call(
        &self,
        workspace: &str,
        method: &str,
        url: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(url)
            .header("content-type", "application/json")
            .header("anthropic-beta", "managed-agents-2026-04-01")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        request
            .extensions_mut()
            .insert(WorkspaceScope(workspace.into()));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
        )
    }
    async fn credential(&self, workspace: &str, label: &str, url: &str) -> (String, String) {
        let (status, vault) = self
            .call(
                workspace,
                "POST",
                "/v1/vaults",
                json!({"display_name":label}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{vault}");
        let vault_id = vault["id"].as_str().unwrap().to_string();
        let (status,credential)=self.call(workspace,"POST",&format!("/v1/vaults/{vault_id}/credentials"),json!({"display_name":label,"auth":{"type":"static_bearer","mcp_server_url":url,"token":"synthetic-account-token"}})).await; // awaken-allow: secret
        assert_eq!(status, StatusCode::OK, "{credential}");
        (vault_id, credential["id"].as_str().unwrap().to_string())
    }
}
fn server(id: &str, url: &str) -> Value {
    json!({"id":id,"name":id,"description":"MCP test","icon":null,"transport":{"type":"url","url":url},"prompts_as_skills":false})
}
#[tokio::test]
async fn credential_target_scope_lifecycle_and_precedence_decision_table() {
    // Graph: same Workspace + active Vault/credential + canonical URL match
    // -> candidate; explicit selected id + current config -> exact source pin.
    // C1 two accounts same endpoint => retain both; C2 wrong URL/Workspace =>
    // absent/reject; C3 stale revision => 409; C4 revoked => reject/no probe;
    // C5 valid selected account => one live probe, no secret in response.
    let h = Harness::new();
    let url = "https://mcp.notion.com/mcp";
    let (va, a) = h.credential("w", "Product Notion", url).await;
    let (_vb, b) = h
        .credential("w", "Personal Notion", "HTTPS://MCP.NOTION.COM:443/mcp/")
        .await;
    let (_, wrong) = h
        .credential("w", "Other service", "https://mcp.notion.com/other")
        .await;
    let (_, foreign) = h.credential("other-workspace", "Foreign Notion", url).await;
    let (status, choices) = h
        .call(
            "w",
            "GET",
            "/v1/config/mcp-servers/catalog:notion/credentials",
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(choices["data"].as_array().unwrap().len(), 2, "C1/C2");
    for id in [&a, &b] {
        let (status, selected) = h
            .call(
                "w",
                "POST",
                "/v1/config/mcp-servers/catalog:notion/prepare",
                json!({"expected_revision":0,"credential_id":id}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        assert_eq!(selected["credential"]["id"], *id);
        assert_eq!(
            selected["binding"]["credential"],
            selected["credential"]["source"]
        );
        assert!(!selected.to_string().contains("synthetic-account-token"));
    }
    for id in [wrong, foreign] {
        let (status, _) = h
            .call(
                "w",
                "POST",
                "/v1/config/mcp-servers/catalog:notion/inspect",
                json!({"expected_revision":0,"credential_id":id}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "C2");
    }
    assert_eq!(h.probe.0.load(Ordering::SeqCst), 0, "C2 no network effect");
    let (status, result) = h
        .call(
            "w",
            "POST",
            "/v1/config/mcp-servers/catalog:notion/inspect",
            json!({"expected_revision":0,"credential_id":b}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["status"], "valid");
    assert!(
        !result.to_string().contains("synthetic-account-token"),
        "C5 tool metadata cannot echo the credential"
    );
    assert_eq!(h.probe.0.load(Ordering::SeqCst), 1);
    let (status, _) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers/catalog:notion/default-credential",
            json!({"expected_revision":0,"credential_id":a}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call(
            "w",
            "POST",
            "/v1/config/mcp-servers/catalog:notion/prepare",
            json!({"expected_revision":0,"credential_id":b}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "C3");
    let (status, _) = h
        .call(
            "w",
            "POST",
            &format!("/v1/vaults/{va}/credentials/{a}/archive"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call(
            "w",
            "POST",
            "/v1/config/mcp-servers/catalog:notion/inspect",
            json!({"expected_revision":1,"credential_id":a}),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "C4 no fallback to Personal Notion"
    );
    assert_eq!(h.probe.0.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn local_config_stdio_defaults_and_conflicts_are_atomic() {
    // L1 local HTTP/stdio create -> stored and selectable; L2 duplicate CAS ->
    // 409 without overwriting; L3 stdio + HTTP credential -> reject; L4 changed
    // URL invalidates default applicability; L5 remove definition clears only
    // its default; L6 another Workspace never reads/writes these rows.
    let h = Harness::new();
    let url = "https://mcp.example.test/mcp";
    let (_, credential) = h.credential("w", "Account", url).await;
    let (status, _) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers",
            json!({"expected_revision":0,"definition":server("custom",url)}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status,_)=h.call("w","PUT","/v1/config/mcp-servers",json!({"expected_revision":0,"definition":server("custom","https://wrong.example.test")})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers/local:custom/default-credential",
            json!({"expected_revision":1,"credential_id":credential}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status,updated)=h.call("w","PUT","/v1/config/mcp-servers",json!({"expected_revision":2,"definition":server("custom","https://new.example.test/mcp")})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        updated["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == "local:custom")
            .unwrap()["default_matches_target"],
        false
    );
    let mut stdio = server("stdio", url);
    stdio["transport"] = json!({"type":"sandbox_stdio","command":"test-mcp","args":["--stdio"]});
    let (status, _) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers",
            json!({"expected_revision":3,"definition":stdio}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call(
            "w",
            "POST",
            "/v1/config/mcp-servers/local:stdio/prepare",
            json!({"expected_revision":4,"credential_id":credential}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = h
        .call(
            "other",
            "GET",
            "/v1/config/mcp-servers/local:custom/credentials",
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .call(
            "w",
            "POST",
            "/v1/config/mcp-servers/local:custom/remove",
            json!({"expected_revision":4}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let stored = h.store.get_mcp_configuration("w").unwrap().unwrap();
    assert!(!stored.defaults.contains_key("local:custom"));
    assert!(stored.server("local:stdio").is_some());
}

#[tokio::test]
async fn current_revision_noop_does_not_create_a_configuration_revision() {
    // N1 identical local definition + current CAS -> no revision; N2 stale CAS
    // even with identical content -> conflict; N3 clear absent default -> no-op.
    let h = Harness::new();
    let body = server("same", "https://mcp.example.test/mcp");
    let (status, first) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers",
            json!({"expected_revision":0,"definition":body}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, same) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers",
            json!({"expected_revision":1,"definition":body}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, same);
    let (status, _) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers",
            json!({"expected_revision":0,"definition":body}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, cleared) = h
        .call(
            "w",
            "PUT",
            "/v1/config/mcp-servers/catalog:withdrawn/default-credential",
            json!({"expected_revision":1,"credential_id":null}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cleared["revision"], 1);
}

#[cfg(feature = "schema")]
#[tokio::test]
async fn documented_mcp_operations_are_mounted_on_the_real_router() {
    // O1 documented method/path -> routed owner, including validation failures;
    // O2 missing method/route -> framework 404/405 and test failure. Schema and
    // TS generation share this registry, not a separate frontend endpoint list.
    let h = Harness::new();
    for (path, operations) in contract_paths() {
        for method in operations.as_object().unwrap().keys() {
            let path = path.replace("{key}", "catalog:notion");
            let (status, body) = h.call("w", &method.to_uppercase(), &path, json!({})).await;
            assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
            assert!(
                !(status == StatusCode::NOT_FOUND && body.get("raw").is_some()),
                "missing {method} {path}"
            );
        }
    }
}
