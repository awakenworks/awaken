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

use awaken_cli::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, TokenSpec,
    build_durable_management_router, build_secured_management_router,
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
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;

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
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;

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
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
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
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;

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
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;

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
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
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

/// A `workspace_user` (no `apikey.*` at all) cannot even READ the vault surface —
/// the existing coverage proves this for `/v1/config/credentials`, but never for
/// the account-level `/v1/vaults` front door.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_user_cannot_read_the_vault_surface() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;
    let token = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_user_vault".into(),
            service_id: "ci-user".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "workspace_user".into(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();
    let (s, err) = call(&app, "GET", "/v1/vaults", Some(token.as_str()), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
}

/// A read-scoped token (`apikey.read`, no write) is denied on the vault *sub-resource*
/// write routes, not just `POST /v1/vaults` — the scope check fires before any
/// handler/not-found logic, so an id that does not exist still 403s (never 404).
#[tokio::test(flavor = "multi_thread")]
async fn a_read_scoped_token_cannot_write_vault_sub_resources() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;
    let token = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_ro_vault".into(),
            service_id: "ci-ro".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "workspace_restricted_developer".into(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();
    let t = Some(token.as_str());
    for (method, uri) in [
        ("DELETE", "/v1/vaults/vault_x"),
        ("POST", "/v1/vaults/vault_x/archive"),
        ("POST", "/v1/vaults/vault_x/credentials"),
    ] {
        let (s, err) = call(&app, method, uri, t, Some(json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {err}");
        assert_eq!(err["error"]["type"], json!("permission_error"));
    }
}

/// The same read-scoped token cannot archive a config credential
/// (`POST /v1/config/credentials/{id}/archive` maps to `apikey.write`) — an untested
/// write sub-route.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_scoped_token_cannot_archive_a_credential() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;
    let token = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_ro_cred".into(),
            service_id: "ci-ro".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "workspace_restricted_developer".into(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/credentials/cred_x/archive",
        Some(token.as_str()),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));
}

/// The workspace-equality fence is symmetric and not special to the Global-bound
/// bootstrap admin: a token minted into a *second* workspace reads its own tenant
/// (200) but is fenced off another (403). The existing mismatch test only exercises
/// the bootstrap token.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_bound_token_reads_its_own_tenant_but_not_another() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;
    let token = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_ws_b".into(),
            service_id: "ci-ws-b".into(),
            workspace_id: "wrkspc_b".into(),
            role: "workspace_admin".into(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();
    let t = Some(token.as_str());

    // Its own workspace is admitted past the fence (an empty list is still a 200).
    let (s, _) = call(
        &app,
        "GET",
        "/v1/config/credentials?workspace_id=wrkspc_b",
        t,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Another tenant's workspace is fenced — 403, fail closed.
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
async fn minted_tokens_survive_a_restart_over_the_same_directory() {
    let dir = tempfile::tempdir().unwrap();

    // ---- lifetime A: bootstrap + mint a second token -----------------------
    let bootstrap;
    let developer;
    {
        let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;
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
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;

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
    let app = build_durable_management_router(dir.path(), &KEY).await;

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

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_hand_rolled_iam_layout_is_imported_once_on_boot() {
    use awaken_iam_core::{ApiTokenDirectory, ApiTokenMinter, OsEntropy, PolicySet};

    let dir = tempfile::tempdir().unwrap();

    // Reproduce a pre-SqlStore install faithfully: the old code persisted the
    // contract ApiToken serde in singular `iam_api_token` (+ mint-time role)
    // and bindings in `iam_role_binding` with the `*` Global sentinel.
    let cleartext;
    {
        let mut directory = ApiTokenDirectory::new();
        let mut policy = PolicySet::new();
        let issued = ApiTokenMinter::new(OsEntropy)
            .mint(
                &mut directory,
                &mut policy,
                awaken_iam_core::MintApiToken {
                    id: awaken_iam_contract::ApiTokenId("tok_legacy_admin".into()),
                    principal: awaken_iam_contract::PrincipalRef::Service {
                        service_id: BOOTSTRAP_PRINCIPAL.into(),
                    },
                    workspace: awaken_iam_contract::WorkspaceId(BOOTSTRAP_WORKSPACE.into()),
                    role: awaken_iam_core::RoleId("admin".into()),
                    created_at: awaken_iam_contract::Timestamp("2026-01-01T00:00:00Z".into()),
                    expires_at: None,
                },
            )
            .unwrap();
        cleartext = issued.secret;
        let conn = rusqlite::Connection::open(dir.path().join("iam.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE iam_api_token (id TEXT PRIMARY KEY, prefix TEXT NOT NULL UNIQUE, \
             data TEXT NOT NULL, role TEXT);\n\
             CREATE TABLE iam_role_binding (principal TEXT NOT NULL, role TEXT NOT NULL, \
             workspace TEXT NOT NULL, PRIMARY KEY (principal, role, workspace));",
        )
        .unwrap();
        let principal = serde_json::to_string(&issued.token.principal).unwrap();
        conn.execute(
            "INSERT INTO iam_api_token (id, prefix, data, role) VALUES (?1, ?2, ?3, 'admin')",
            rusqlite::params![
                issued.token.id.0,
                issued.token.prefix.0,
                serde_json::to_string(&issued.token).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO iam_role_binding (principal, role, workspace) VALUES (?1, 'admin', ?2)",
            rusqlite::params![principal, BOOTSTRAP_WORKSPACE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO iam_role_binding (principal, role, workspace) VALUES (?1, 'admin', '*')",
            rusqlite::params![principal],
        )
        .unwrap();
    }

    // Boot the SqlStore-backed code over the legacy file: the import must carry
    // the token across (no re-bootstrap: the hydrated directory is non-empty,
    // so no admin-token file appears), authenticating and authorizing as before.
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    assert!(!dir.path().join(ADMIN_TOKEN_FILE).exists());
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/providers/anthropic",
        Some(&cleartext),
        Some(provider_body()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // A second boot must not import again (the legacy tables were renamed).
    drop(app);
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let (s, _) = call(&app, "GET", "/v1/config/catalog", Some(&cleartext), None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversize_request_body_is_413() {
    // The guard buffers the body (to run the workspace fence over it); a body past
    // the 2 MiB buffer is refused with 413 rather than read unboundedly. This
    // body-buffering fence + the Managed ErrorResponse envelope is one of the
    // Managed-specific seams that keep the guard local even though iam-host's PEP
    // is now tenancy-capable (ADR-0048; see the authz.rs module doc).
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let token = admin_token(dir.path());
    let big = "x".repeat(3 * 1024 * 1024);
    let body = json!({ "workspace_id": BOOTSTRAP_WORKSPACE, "blob": big });
    let (s, _) = call(
        &app,
        "POST",
        "/v1/config/credentials",
        Some(token.as_str()),
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}
