//! The MCP management chain end-to-end over HTTP (ADR-0043 Phase 3): enter a
//! credential, author an MCP server definition bound to it, bind an agent to that
//! server, then dry-run the agent's binding through the resolver — the response
//! reports `credential_present` and never carries the secret. Also proves the
//! fail-closed write validation (dangling credential / server / agent → 404).

use std::sync::Arc;

use awaken_admin_config_api::{AdminState, admin_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn harness() -> Router {
    admin_router(AdminState {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
        probe: None,
        availability: Default::default(),
    })
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Enter a vault credential over HTTP and return its id.
async fn enter_credential(app: &Router, secret: &str) -> String {
    let (s, cred) = call(
        app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "vault",
            "provider_id": null,
            "env_key": null,
            "secret": secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    cred["id"].as_str().expect("credential id").to_string()
}

#[tokio::test]
async fn author_credential_mcp_server_and_agent_binding_then_resolve_secret_free() {
    let app = harness();
    let cred_id = enter_credential(&app, "sk-mcp-wire-secret").await;

    // Author an MCP server bound to the credential (path id authoritative).
    let (s, server) = call(
        &app,
        "PUT",
        "/v1/config/mcp-servers/jira",
        Some(json!({
            "id": "ignored-by-path",
            "workspace_id": "ws",
            "display_name": "Jira",
            "url": "https://jira.example/mcp",
            "credential_binding": { "type": "exact", "credential_source_id": cred_id },
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(server["id"], "jira");

    // And an unauthenticated one (binding None).
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/mcp-servers/docs",
        Some(json!({
            "id": "docs",
            "workspace_id": "ws",
            "display_name": "Docs",
            "url": "https://docs.example/mcp",
            "credential_binding": { "type": "none" },
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // GET round-trips the def; the binding is a reference, never a secret.
    let (s, got) = call(&app, "GET", "/v1/config/mcp-servers/jira", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["display_name"], "Jira");
    assert_eq!(got["credential_binding"]["type"], "exact");

    // List returns both, ordered by id.
    let (s, listed) = call(&app, "GET", "/v1/config/mcp-servers", None).await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["docs", "jira"]);

    // Bind the agent to both servers (path agent id authoritative).
    let (s, binding) = call(
        &app,
        "PUT",
        "/v1/config/agents/agent1/mcp",
        Some(json!({
            "agent_id": "ignored-by-path",
            "workspace_id": "ws",
            "mcp_server_ids": ["jira", "docs"],
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(binding["agent_id"], "agent1");

    let (s, got) = call(&app, "GET", "/v1/config/agents/agent1/mcp", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["mcp_server_ids"], json!(["jira", "docs"]));

    // Resolve the agent's binding: credential_present, but NEVER the secret.
    let (s, resolved) = call(
        &app,
        "POST",
        "/v1/config/agents/agent1/mcp/resolve",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        resolved,
        json!([
            { "name": "Jira", "url": "https://jira.example/mcp", "credential_present": true },
            { "name": "Docs", "url": "https://docs.example/mcp", "credential_present": false }
        ])
    );
    let body_text = serde_json::to_string(&resolved).unwrap();
    assert!(
        !body_text.contains("sk-mcp-wire-secret"),
        "secret leaked: {body_text}"
    );
}

#[tokio::test]
async fn mcp_server_with_unknown_credential_source_is_rejected() {
    let app = harness();
    let (s, err) = call(
        &app,
        "PUT",
        "/v1/config/mcp-servers/jira",
        Some(json!({
            "id": "jira",
            "display_name": "Jira",
            "url": "https://jira.example/mcp",
            "credential_binding": { "type": "exact", "credential_source_id": "ghost-cred" },
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");

    // The invalid def was never stored (fail-closed write).
    let (s, _) = call(&app, "GET", "/v1/config/mcp-servers/jira", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// put_mcp_server (pool arm): a def bound to a credential *pool* is validated
/// fail-closed on write — an unknown pool is a 404 and the def is never stored,
/// while a def bound to an authored pool is accepted and retrievable. The existing
/// suite only exercises the `Exact` and `None` binding arms; this covers the
/// `OneOfCredentialPool` branch (both its rejection and its success path).
#[tokio::test]
async fn mcp_server_pool_binding_is_validated_fail_closed_on_write() {
    let app = harness();

    // Unknown pool → 404, and nothing is stored (a def that can never resolve is
    // never persisted).
    let (s, err) = call(
        &app,
        "PUT",
        "/v1/config/mcp-servers/jira",
        Some(json!({
            "id": "jira",
            "display_name": "Jira",
            "url": "https://jira.example/mcp",
            "credential_binding": {
                "type": "one_of_credential_pool",
                "credential_pool_id": "ghost-pool"
            },
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");
    let (s, _) = call(&app, "GET", "/v1/config/mcp-servers/jira", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Author the pool, then the same binding is accepted and stored.
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/credential-pools/pool-a",
        Some(json!({ "id": "pool-a", "workspace_id": "ws", "members": [] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, server) = call(
        &app,
        "PUT",
        "/v1/config/mcp-servers/jira",
        Some(json!({
            "id": "jira",
            "display_name": "Jira",
            "url": "https://jira.example/mcp",
            "credential_binding": {
                "type": "one_of_credential_pool",
                "credential_pool_id": "pool-a"
            },
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        server["credential_binding"]["type"],
        "one_of_credential_pool"
    );
    let (s, got) = call(&app, "GET", "/v1/config/mcp-servers/jira", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["credential_binding"]["credential_pool_id"], "pool-a");
}

#[tokio::test]
async fn agent_binding_to_unknown_mcp_server_is_rejected() {
    let app = harness();
    let (s, err) = call(
        &app,
        "PUT",
        "/v1/config/agents/agent1/mcp",
        Some(json!({
            "agent_id": "agent1",
            "mcp_server_ids": ["ghost-server"],
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");

    // The dangling binding was never stored (fail-closed write).
    let (s, _) = call(&app, "GET", "/v1/config/agents/agent1/mcp", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resolving_an_unbound_agent_is_problem_json_404() {
    let app = harness();
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/agents/nobody/mcp/resolve",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["status"], 404);
}

#[tokio::test]
async fn agent_resource_binding_crud_round_trips() {
    let app = harness();

    // Unknown agent → 404 problem+json.
    let (s, err) = call(&app, "GET", "/v1/config/agents/nobody/resources", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");

    // Bind typed Memory + File inputs. Outputs and Skills are deliberately not
    // members of this input union.
    let body = json!({
        "agent_id": "ignored-path-wins",
        "inputs": [
            { "binding_id": "memory", "target": { "kind": "memory_store", "id": "memstore-7" },
              "mount_path": "/mnt/memory/prefs", "access": "read_write",
              "instructions": "user preferences" },
            { "binding_id": "file", "target": { "kind": "file", "id": "file-1" },
              "mount_path": "/mnt/files/input.txt", "access": "read_only" }
        ],
        "revision": 1
    });
    let (s, put) = call(
        &app,
        "PUT",
        "/v1/config/agents/agent-1/resources",
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(put["agent_id"], "agent-1"); // the path id is authoritative
    assert_eq!(put["inputs"].as_array().unwrap().len(), 2);

    let (s, conflict) = call(
        &app,
        "PUT",
        "/v1/config/agents/agent-1/resources",
        Some(json!({
            "agent_id": "agent-1",
            "inputs": [],
            "revision": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "revision_conflict");

    let (s, invalid) = call(
        &app,
        "PUT",
        "/v1/config/agents/agent-2/resources",
        Some(json!({
            "agent_id": "agent-2",
            "inputs": [],
            "revision": 0
        })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid["code"], "invalid_revision");

    // Read it back.
    let (s, got) = call(&app, "GET", "/v1/config/agents/agent-1/resources", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["inputs"][0]["target"]["kind"], "memory_store");
    assert_eq!(got["inputs"][0]["mount_path"], "/mnt/memory/prefs");
    assert_eq!(got["inputs"][1]["target"]["kind"], "file");
}
