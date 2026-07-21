//! Restart persistence for the durable management plane (ADR-0043): a router
//! built over `AWAKEN_MGMT_DIR`-style SQLite stores is dropped and rebuilt over
//! the same directory + key, and everything authored through HTTP — catalog,
//! credential (sealed secret), pool, inference profile, MCP server def, agent
//! MCP binding — reads back and still *resolves* (the credential materializes
//! from the sealed blob store). A rebuild under the WRONG key fails closed at
//! materialization (`Seal` → 422) while the secret-free config rows stay
//! readable. Uses `build_durable_management_router` (explicit dir + key), not
//! env vars, so the test cannot race other tests on process-global state.

use awaken_cli::build_durable_management_router;
use awaken_config_store::{ScopeId, ScopedConfigRegistry, SqliteConfigStore};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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

const KEY: [u8; 32] = [7u8; 32];
const WRONG_KEY: [u8; 32] = [8u8; 32];

#[tokio::test(flavor = "multi_thread")]
async fn authored_config_and_sealed_credentials_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();

    // ---- lifetime A: author everything over HTTP --------------------------
    let cred_id;
    {
        let app = build_durable_management_router(dir.path(), &KEY).await;

        let audit_probe_body = serde_json::to_vec(&json!({
            "id": "audit-probe", "slug": "audit-probe",
            "display_name": "Audit Probe", "version": 1
        }))
        .unwrap();
        let audit_probe = Request::builder()
            .method("PUT")
            .uri("/v1/config/providers/audit-probe")
            .header("content-type", "application/json")
            .header("x-request-id", "audit-request-1")
            .body(Body::from(audit_probe_body.clone()))
            .unwrap();
        let response = app.clone().oneshot(audit_probe).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let replay = Request::builder()
            .method("PUT")
            .uri("/v1/config/providers/audit-probe")
            .header("content-type", "application/json")
            .header("x-request-id", "audit-request-1")
            .body(Body::from(audit_probe_body))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(replay).await.unwrap().status(),
            StatusCode::CONFLICT,
            "an ambiguous stable-id retry must never repeat the business write"
        );
        let audit_store = SqliteConfigStore::open(dir.path().join("config.db").to_str().unwrap())
            .expect("open durable audit store");
        let platform_workspace = std::fs::read_to_string(dir.path().join("platform-workspace-id"))
            .expect("platform workspace persisted");
        let audit_entry = audit_store
            .get_management_audit_scoped(
                &ScopeId::from(platform_workspace.trim()),
                "http:PUT:/v1/config/providers/audit-probe",
                "audit-request-1",
            )
            .await
            .unwrap()
            .expect("management mutation audit");
        assert!(audit_entry.business_committed);

        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/providers/anthropic",
            Some(json!({ "id": "anthropic", "slug": "anthropic", "display_name": "Anthropic", "version": 1 })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/agents/calc-agent/resources",
            Some(json!({
                "agent_id": "ignored-path-is-authoritative",
                "inputs": [{
                    "binding_id": "memory",
                    "target": { "kind": "memory_store", "id": "memory-main" },
                    "mount_path": "/mnt/memory", "access": "read_write",
                    "instructions": null
                }],
                "revision": 1
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/endpoints/ep1",
            Some(json!({
                "id": "ep1", "provider_id": "anthropic", "dialect": "anthropic_messages",
                "base_url": "https://api.anthropic.com/v1/", "timeout_secs": 300,
                "display_name": "prod", "version": 1
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(
            &app,
            "POST",
            "/v1/config/offerings",
            Some(json!({
                "model_id": "claude-opus-4-8", "provider_id": "anthropic",
                "protocol_endpoint_id": "ep1", "dialect": "anthropic_messages",
                "upstream_model": null
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // Enter a vault credential: the secret is sealed into credential.db.
        let (s, cred) = call(
            &app,
            "POST",
            "/v1/config/credentials",
            Some(json!({
                "workspace_id": "ws", "kind": "vault", "provider_id": "anthropic",
                "env_key": "ANTHROPIC_API_KEY",
                "secret": "sk-restart-secret" // awaken-allow: secret
            })),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{cred}");
        cred_id = cred["id"].as_str().unwrap().to_string();

        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/credential-pools/pool1",
            Some(json!({
                "id": "pool1", "workspace_id": "ws",
                "members": [{ "credential_source_id": cred_id, "ordinal": 0,
                              "enabled": true, "selection_weight": 0 }]
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/inference-profiles/prof1",
            Some(json!({
                "model_id": "claude-opus-4-8",
                "credential_binding": { "type": "exact", "credential_source_id": cred_id },
                "disabled_endpoint_ids": []
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/mcp-servers/calc-def",
            Some(json!({
                "id": "calc-def", "display_name": "calc", "url": "http://127.0.0.1:1/",
                "credential_binding": { "type": "exact", "credential_source_id": cred_id },
                "version": 1
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/agents/calc-agent/mcp",
            Some(json!({ "agent_id": "calc-agent", "mcp_server_ids": ["calc-def"], "version": 1 })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    } // drop router A: "process" ends

    // ---- lifetime B: same dir, same key — everything is still there -------
    let app = build_durable_management_router(dir.path(), &KEY).await;

    let (s, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        catalog["providers"]
            .as_object()
            .is_some_and(|p| p.contains_key("anthropic")),
        "provider persisted: {catalog}"
    );
    assert!(
        catalog["endpoints"]
            .as_object()
            .is_some_and(|e| e.contains_key("ep1")),
        "endpoint persisted: {catalog}"
    );

    let (s, creds) = call(&app, "GET", "/v1/config/credentials?workspace_id=ws", None).await;
    assert_eq!(s, StatusCode::OK);
    let creds = creds.as_array().unwrap();
    assert!(creds.iter().any(|c| c["id"] == json!(cred_id)));
    assert!(
        !serde_json::to_string(&creds)
            .unwrap()
            .contains("sk-restart-secret"),
        "credential list stays secret-free after restart"
    );

    let (s, pool) = call(&app, "GET", "/v1/config/credential-pools/pool1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(pool["members"].as_array().unwrap().len(), 1);

    let (s, prof) = call(&app, "GET", "/v1/config/inference-profiles/prof1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(prof["model_id"], json!("claude-opus-4-8"));

    let (s, servers) = call(&app, "GET", "/v1/config/mcp-servers", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(servers.as_array().unwrap().len(), 1);
    assert_eq!(servers[0]["id"], json!("calc-def"));

    let (s, binding) = call(&app, "GET", "/v1/config/agents/calc-agent/mcp", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(binding["mcp_server_ids"], json!(["calc-def"]));

    let (s, resources) = call(&app, "GET", "/v1/config/agents/calc-agent/resources", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resources["agent_id"], json!("calc-agent"));
    assert_eq!(resources["inputs"][0]["target"]["id"], json!("memory-main"));

    // The resolve arm works: the credential materializes from the sealed store.
    let (s, resolved) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws", "model_id": "claude-opus-4-8",
            "binding": { "type": "exact", "credential_source_id": cred_id }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{resolved}");
    assert_eq!(resolved["credential_present"], json!(true));

    // Profile resolve reads the persisted profile + sealed secret too.
    let (s, resolved) = call(
        &app,
        "POST",
        "/v1/config/inference-profiles/prof1/resolve",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{resolved}");
    assert_eq!(resolved["credential_present"], json!(true));

    // ---- lifetime C: same dir, WRONG key — fails closed, rows readable ----
    drop(app);
    let app = build_durable_management_router(dir.path(), &WRONG_KEY).await;

    // The secret-free config rows are untouched by the key...
    let (s, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        catalog["providers"]
            .as_object()
            .is_some_and(|p| p.contains_key("anthropic"))
    );
    let (s, cred) = call(
        &app,
        "GET",
        &format!("/v1/config/credentials/{cred_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cred["id"], json!(cred_id));

    // ...but materialization fails closed (AEAD open fails → Seal → 422).
    let (s, problem) = call(
        &app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws", "model_id": "claude-opus-4-8",
            "binding": { "type": "exact", "credential_source_id": cred_id }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], json!("credential_invalid"));
    assert!(
        !serde_json::to_string(&problem)
            .unwrap()
            .contains("sk-restart-secret"),
        "the failure never leaks the plaintext"
    );
}
