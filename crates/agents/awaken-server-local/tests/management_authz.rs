//! Embedded IAM over the management plane (ADR-0042/0043 P1): the admin +
//! vault surfaces behind bearer `ApiToken` authn and preset-role authz.
//!
//! Uses `build_secured_management_router` (explicit dir + key, env-free) so the
//! tests cannot race other tests on process-global env vars, and the returned
//! [`ManagementAuthz`] handle to mint non-admin tokens. Trust-model pins:
//! missing/garbage/expired tokens 401 in the Managed `ErrorResponse` envelope;
//! the bootstrap admin token has full CRUD; a restricted-developer token reads
//! but cannot write; a workspace_user token cannot even read credentials; any
//! named `workspace_id` must match the token's workspace (fail closed); minted
//! tokens hydrate across a restart over the same directory; and WITHOUT the
//! guard the plane stays exactly as open as before (regression pin).

use awaken_server_local::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_WORKSPACE, TokenSpec, build_durable_management_router,
    build_secured_management_router,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const KEY: [u8; 32] = [9u8; 32];

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        b = b.header("authorization", format!("Bearer {token}"));
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// The bootstrap admin token the boot wrote to `<dir>/admin-token`.
fn admin_token(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join(ADMIN_TOKEN_FILE)).expect("bootstrap admin-token file")
}

fn provider_body() -> Value {
    json!({ "id": "anthropic", "slug": "anthropic", "display_name": "Anthropic", "version": 1 })
}

fn credential_body() -> Value {
    json!({
        "workspace_id": BOOTSTRAP_WORKSPACE, "kind": "vault", "provider_id": "anthropic",
        "env_key": "ANTHROPIC_API_KEY",
        "secret": "sk-authz-secret" // awaken-allow: secret
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_and_garbage_tokens_are_rejected_with_401() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY);

    // No token at all → 401 in the Managed error envelope.
    let (s, err) = call(&app, "GET", "/v1/config/catalog", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{err}");
    assert_eq!(err["type"], json!("error"));
    assert_eq!(err["error"]["type"], json!("authentication_error"));

    // A token-shaped but unknown credential → the same opaque 401.
    let (s, err) = call(
        &app,
        "GET",
        "/v1/config/catalog",
        Some("sk-ant-bogusprefix.bogussecret"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(err["error"]["type"], json!("authentication_error"));

    // Plain garbage that does not even parse as a token → 401 too.
    let (s, _) = call(&app, "GET", "/v1/config/catalog", Some("garbage"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // The vault front door is behind the same guard.
    let (s, _) = call(&app, "POST", "/v1/vaults", None, Some(json!({}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_token_fails_authentication_with_401() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY);

    // Minted in the past and already expired (expiry must be strictly after
    // creation, so both stamps are historical).
    let expired = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_expired".into(),
            service_id: "ci-expired".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "admin".into(),
            created_at: Some("2020-01-01T00:00:00Z".into()),
            expires_at: Some("2020-01-02T00:00:00Z".into()),
        })
        .unwrap();

    let (s, err) = call(&app, "GET", "/v1/config/catalog", Some(&expired), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(err["error"]["type"], json!("authentication_error"));
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expired"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bootstrap_admin_token_authorizes_full_crud_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY);
    let token = admin_token(dir.path());
    let t = Some(token.as_str());

    // Catalog writes + reads (workspace.*).
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/providers/anthropic",
        t,
        Some(provider_body()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, catalog) = call(&app, "GET", "/v1/config/catalog", t, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(catalog["providers"].as_object().is_some());

    // Credential surface (apikey.*): enter, list, pool.
    let (s, cred) = call(
        &app,
        "POST",
        "/v1/config/credentials",
        t,
        Some(credential_body()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{cred}");
    let cred_id = cred["id"].as_str().unwrap().to_string();
    let (s, creds) = call(
        &app,
        "GET",
        &format!("/v1/config/credentials?workspace_id={BOOTSTRAP_WORKSPACE}"),
        t,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        creds
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == cred["id"])
    );
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/credential-pools/pool1",
        t,
        Some(json!({
            "id": "pool1", "workspace_id": BOOTSTRAP_WORKSPACE,
            "members": [{ "credential_source_id": cred_id, "ordinal": 0,
                          "enabled": true, "selection_weight": 0 }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Admin aggregates (workspace.*): MCP server def + agent binding.
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/mcp-servers/calc-def",
        t,
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
        t,
        Some(json!({ "agent_id": "calc-agent", "mcp_server_ids": ["calc-def"], "version": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The vault front door (apikey.*).
    let (s, vault) = call(
        &app,
        "POST",
        "/v1/vaults",
        t,
        Some(json!({ "display_name": "authz vault" })),
    )
    .await;
    assert!(s.is_success(), "vault create: {s} {vault}");

    // The SDK's `x-api-key` header carries the same credential (compat path).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/config/catalog")
                .header("x-api-key", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "x-api-key authenticates too");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restricted_developer_token_reads_everything_but_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY);

    // Per the preset catalog, workspace_restricted_developer holds
    // `apikey.read` + `workspace.read` (among file/skill), but no write.
    let token = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_restricted".into(),
            service_id: "ci-restricted".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "workspace_restricted_developer".into(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();
    let t = Some(token.as_str());

    let (s, _) = call(&app, "GET", "/v1/config/catalog", t, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/config/credentials?workspace_id={BOOTSTRAP_WORKSPACE}"),
        t,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Credential write → apikey.write → deny.
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/credentials",
        t,
        Some(credential_body()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));

    // Catalog write → workspace.write → deny.
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/providers/anthropic",
        t,
        Some(provider_body()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Vault create → apikey.write → deny.
    let (s, _) = call(&app, "POST", "/v1/vaults", t, Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_user_token_reads_config_but_not_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY);

    // workspace_user holds workspace.read but NO apikey pattern at all.
    let token = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_user".into(),
            service_id: "ci-user".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "workspace_user".into(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();
    let t = Some(token.as_str());

    let (s, _) = call(&app, "GET", "/v1/config/catalog", t, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, err) = call(
        &app,
        "GET",
        &format!("/v1/config/credentials?workspace_id={BOOTSTRAP_WORKSPACE}"),
        t,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_mismatch_is_refused_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY);
    let token = admin_token(dir.path());
    let t = Some(token.as_str());

    // Body naming a foreign workspace → 403 even for the admin token: its
    // authority is bound at wrkspc_default, and the fence fails closed.
    let mut body = credential_body();
    body["workspace_id"] = json!("wrkspc_other");
    let (s, err) = call(&app, "POST", "/v1/config/credentials", t, Some(body)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));

    // The query-string fence closes the read path the same way.
    let (s, _) = call(
        &app,
        "GET",
        "/v1/config/credentials?workspace_id=wrkspc_other",
        t,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn minted_tokens_survive_a_restart_over_the_same_directory() {
    let dir = tempfile::tempdir().unwrap();

    // ---- lifetime A: bootstrap + mint a second token -----------------------
    let bootstrap;
    let developer;
    {
        let (app, iam) = build_secured_management_router(dir.path(), &KEY);
        bootstrap = admin_token(dir.path());
        developer = iam
            .mint_service_token(TokenSpec {
                token_id: "tok_dev".into(),
                service_id: "ci-dev".into(),
                workspace_id: BOOTSTRAP_WORKSPACE.into(),
                role: "workspace_admin".into(),
                created_at: None,
                expires_at: None,
            })
            .unwrap();
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/config/providers/anthropic",
            Some(&bootstrap),
            Some(provider_body()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    } // drop router A: "process" ends

    // ---- lifetime B: same dir — hydration, not re-bootstrap ----------------
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY);

    // The directory hydrated, so no fresh bootstrap overwrote the token file.
    assert_eq!(admin_token(dir.path()), bootstrap);

    // Both previously minted tokens still authenticate and authorize, and the
    // domain state they authored is still there.
    let (s, catalog) = call(&app, "GET", "/v1/config/catalog", Some(&bootstrap), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        catalog["providers"]
            .as_object()
            .is_some_and(|p| p.contains_key("anthropic")),
        "{catalog}"
    );
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/providers/anthropic",
        Some(&developer),
        Some(provider_body()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_guard_the_management_plane_stays_open() {
    // Regression pin: the default (AWAKEN_MGMT_IAM unset ⇒ no guard) is
    // byte-identical to the pre-IAM behavior — no token, everything works.
    let dir = tempfile::tempdir().unwrap();
    let app = build_durable_management_router(dir.path(), &KEY);

    let (s, _) = call(&app, "GET", "/v1/config/catalog", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/providers/anthropic",
        None,
        Some(provider_body()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}
