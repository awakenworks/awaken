use super::*;
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{InMemorySecretStore, repo::InMemoryCredentialRepo};
use awaken_session_configuration_contract::{McpProbe, McpProbeStatus};
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
    ) -> awaken_session_configuration_contract::McpInspection {
        let bearer = bearer.unwrap();
        let status = self.probe(url, bearer).await;
        awaken_session_configuration_contract::McpInspection {
            status,
            tools: vec![awaken_session_configuration_contract::McpProbeTool {
                name: "read".into(),
                description: Some(bearer.expose_secret().into()),
                input_schema: json!({"type":"object","description":bearer.expose_secret()}),
            }],
            failure: None,
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
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            awaken_admin_config_api::SqliteAdminStore::open(
                temp.path().join("admin.sqlite").to_str().unwrap(),
            )
            .await
            .unwrap(),
        );
        let probe = Arc::new(Probe::default());
        let vault = Arc::new(
            VaultState::new(
                Arc::new(InMemorySecretStore::new()),
                Arc::new(InMemoryCredentialRepo::new()),
                awaken_time::system_clock(),
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
        let (status,credential)=self.call(workspace,"POST",&format!("/v1/vaults/{vault_id}/credentials"),json!({"display_name":label,"auth":{"type":"static_bearer","mcp_server_url":url,"token":"synthetic-account-token"}})).await; // awaken-allow(secret):
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
    let h = Harness::new().await;
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
    let h = Harness::new().await;
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
    let stored = h.store.get_mcp_configuration("w").await.unwrap().unwrap();
    assert!(!stored.defaults.contains_key("local:custom"));
    assert!(stored.server("local:stdio").is_some());
}

#[tokio::test]
async fn current_revision_noop_does_not_create_a_configuration_revision() {
    // N1 identical local definition + current CAS -> no revision; N2 stale CAS
    // even with identical content -> conflict; N3 clear absent default -> no-op.
    let h = Harness::new().await;
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
    let h = Harness::new().await;
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

#[tokio::test]
async fn supervised_catalog_refresh_uses_one_sync_path_and_stops_with_lifecycle() {
    // Cause/effect graph: C1 configured Workspace, C2 reachable catalog,
    // C3 lifecycle active, C4 catalog revision advances, C5 response becomes
    // malformed, C6 another Workspace has no source. Effects: E1 immediate
    // refresh commits revision 2, E2 later tick commits revision 3 through the
    // same sync operation, E3 malformed content retains revision 3 and the task
    // recovers at revision 4, E4 the unconfigured Workspace is untouched, E5
    // lifecycle shutdown stops further HTTP requests. Rules: B1=C1+C2+C3=>E1;
    // B2=B1+C4+tick=>E2; B3=B2+C5=>E3; B4=C6=>E4; B5=!C3=>E5. The injected
    // interval changes scheduling only; production retains its five-minute value.
    use awaken_agent_config::mcp_config::{
        McpCatalogSnapshot, McpConfiguration, McpConfigurationRepository,
    };
    use std::sync::atomic::{AtomicBool, AtomicU64};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let remote_revision = Arc::new(AtomicU64::new(2));
    let malformed = Arc::new(AtomicBool::new(false));
    let requests_for_server = requests.clone();
    let revision_for_server = remote_revision.clone();
    let malformed_for_server = malformed.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let requests = requests_for_server.clone();
            let revision = revision_for_server.load(Ordering::SeqCst);
            let malformed = malformed_for_server.load(Ordering::SeqCst);
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut input = [0_u8; 4096];
                let _ = socket.read(&mut input).await.unwrap();
                requests.fetch_add(1, Ordering::SeqCst);
                let body = if malformed {
                    "{malformed".to_owned()
                } else {
                    serde_json::to_string(&McpCatalogSnapshot {
                        schema_version: 1,
                        revision,
                        servers: vec![],
                    })
                    .unwrap()
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
        }
    });

    let harness = Harness::new().await;
    let mut configuration = McpConfiguration::new(
        "background".into(),
        McpCatalogSnapshot {
            schema_version: 1,
            revision: 1,
            servers: vec![],
        },
    );
    configuration.catalog_url = Some(format!("http://{address}/catalog.json"));
    configuration.advance(0).unwrap();
    harness
        .store
        .put_mcp_configuration(0, &configuration)
        .await
        .unwrap();
    let mut unconfigured = McpConfiguration::new(
        "unconfigured".into(),
        McpCatalogSnapshot {
            schema_version: 1,
            revision: 1,
            servers: vec![],
        },
    );
    unconfigured.advance(0).unwrap();
    harness
        .store
        .put_mcp_configuration(0, &unconfigured)
        .await
        .unwrap();
    let service = McpCatalogService::new(
        harness.store.clone(),
        Arc::new(VaultState::new(
            Arc::new(InMemorySecretStore::new()),
            Arc::new(InMemoryCredentialRepo::new()),
            awaken_time::system_clock(),
        )),
    );
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    service.supervise_with_interval(&lifecycle, std::time::Duration::from_millis(25));

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if harness
                .store
                .get_mcp_configuration("background")
                .await
                .unwrap()
                .is_some_and(|value| value.catalog.revision == 2)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B1 immediate refresh");
    remote_revision.store(3, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if harness
                .store
                .get_mcp_configuration("background")
                .await
                .unwrap()
                .is_some_and(|value| value.catalog.revision == 3)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B2 scheduled refresh");
    malformed.store(true, Ordering::SeqCst);
    let before_failure = requests.load(Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while requests.load(Ordering::SeqCst) == before_failure {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B3 malformed refresh attempted");
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(
        harness
            .store
            .get_mcp_configuration("background")
            .await
            .unwrap()
            .unwrap()
            .catalog
            .revision,
        3,
        "B3 last accepted catalog retained"
    );
    assert!(lifecycle.is_healthy(), "B3 refresh failure is recoverable");
    malformed.store(false, Ordering::SeqCst);
    remote_revision.store(4, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if harness
                .store
                .get_mcp_configuration("background")
                .await
                .unwrap()
                .is_some_and(|value| value.catalog.revision == 4)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("B3 next valid refresh recovers");
    assert_eq!(
        harness
            .store
            .get_mcp_configuration("unconfigured")
            .await
            .unwrap()
            .unwrap(),
        unconfigured,
        "B4"
    );
    lifecycle
        .shutdown(std::time::Duration::from_secs(1))
        .await
        .unwrap();
    let after_shutdown = requests.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    assert_eq!(requests.load(Ordering::SeqCst), after_shutdown, "B5");
    assert!(lifecycle.is_healthy());
    server.abort();
    let _ = server.await;
}
