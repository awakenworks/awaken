//! The HTTP token-management surface over the embedded IAM (ADR-0042/0043 P1
//! authz completion): `POST/GET /v1/config/iam/tokens` +
//! `DELETE /v1/config/iam/tokens/{id}` behind the management guard.
//!
//! Trust-model pins: the bootstrap admin's GLOBAL role binding lets it mint
//! tokens for workspaces other than its own (the scope graph decides, not
//! header equality); a workspace-bound admin mints only for its own workspace;
//! minted tokens obey the route→action rules for their role and stay fenced to
//! their workspace; list responses are secret-free (no argon2 hash, no
//! cleartext); non-admin roles cannot mint; unknown roles are 422 problem+json;
//! revocation is immediate, self-service-capable, persisted across restart, and
//! scoped (404 unknown id); and the bootstrap token can be rotated away —
//! revoke it with a freshly minted admin token and only the successor works.

use awaken_server_local::{BOOTSTRAP_WORKSPACE, TokenSpec, build_secured_management_router};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const KEY: [u8; 32] = [7u8; 32];

/// A workspace the bootstrap token is NOT bound at — cross-workspace proof.
const OTHER_WORKSPACE: &str = "wrkspc_two";

async fn call_raw(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, String, Option<String>) {
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
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        content_type,
    )
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, text, _) = call_raw(app, method, uri, token, body).await;
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    (status, value)
}

fn admin_token(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join(awaken_server_local::ADMIN_TOKEN_FILE))
        .expect("bootstrap admin-token file")
}

fn mint_body(workspace_id: &str, role: &str) -> Value {
    json!({ "workspace_id": workspace_id, "role": role })
}

/// Mint over HTTP and return `(cleartext, token_id)`.
async fn mint_http(
    app: &axum::Router,
    minter: &str,
    workspace_id: &str,
    role: &str,
) -> (String, String) {
    let (s, minted) = call(
        app,
        "POST",
        "/v1/config/iam/tokens",
        Some(minter),
        Some(mint_body(workspace_id, role)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{minted}");
    (
        minted["token"]
            .as_str()
            .expect("cleartext token")
            .to_string(),
        minted["api_token"]["id"]
            .as_str()
            .expect("token id")
            .to_string(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_global_bootstrap_binding_mints_cross_workspace_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let bootstrap = admin_token(dir.path());

    // The bootstrap credential's own workspace is wrkspc_default, yet it mints
    // for wrkspc_two: only the Global admin binding can authorize apikey.write
    // at that TARGET workspace.
    let (s, minted) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(mint_body(OTHER_WORKSPACE, "workspace_admin")),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{minted}");
    let cleartext = minted["token"].as_str().unwrap();
    // Awaken-branded scheme: management tokens are `sk-awaken-…`, distinct from
    // real Anthropic provider keys (`sk-ant-…`, now legacy-verify only).
    assert!(cleartext.starts_with("sk-awaken-"), "{cleartext}");
    let view = &minted["api_token"];
    assert_eq!(view["workspace_id"], json!(OTHER_WORKSPACE));
    assert_eq!(view["role"], json!("workspace_admin"));
    assert!(view["id"].as_str().unwrap().starts_with("tok_"));
    assert!(view.get("secret_hash").is_none(), "view is hash-free");
    assert!(view["revoked_at"].is_null() || view.get("revoked_at").is_none());

    // The minted token works within its role's route→action rules, in ITS
    // workspace: workspace_admin holds workspace.* + apikey.* at wrkspc_two.
    let t = Some(cleartext);
    let (s, _) = call(&app, "GET", "/v1/config/catalog", t, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, cred) = call(
        &app,
        "POST",
        "/v1/config/credentials",
        t,
        Some(json!({
            "workspace_id": OTHER_WORKSPACE, "kind": "vault", "provider_id": "anthropic",
            "env_key": "ANTHROPIC_API_KEY",
            "secret": "sk-tokens-two" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{cred}");

    // …and 403s cross-workspace: the fence + its wrkspc_two-only binding.
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
async fn a_workspace_bound_admin_mints_only_for_its_own_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let bootstrap = admin_token(dir.path());
    let (scoped, _) = mint_http(&app, &bootstrap, OTHER_WORKSPACE, "workspace_admin").await;

    // Its own workspace: allowed (binding at wrkspc_two covers wrkspc_two).
    let (s, minted) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&scoped),
        Some(mint_body(OTHER_WORKSPACE, "workspace_user")),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{minted}");

    // Another workspace: the scope graph refuses — no Global binding here.
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&scoped),
        Some(mint_body(BOOTSTRAP_WORKSPACE, "workspace_user")),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));

    // Listing another workspace's tokens is refused the same way.
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&scoped),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn token_listings_are_secret_free() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let bootstrap = admin_token(dir.path());
    let (cleartext, _) = mint_http(&app, &bootstrap, BOOTSTRAP_WORKSPACE, "workspace_admin").await;

    let (s, raw, _) = call_raw(
        &app,
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&bootstrap),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{raw}");
    // The argon2id PHC hash and the cleartexts never appear on the wire.
    assert!(!raw.contains("$argon2"), "list leaks a secret hash: {raw}");
    assert!(!raw.contains(&cleartext), "list leaks the minted cleartext");
    assert!(
        !raw.contains(bootstrap.trim()),
        "list leaks the bootstrap cleartext"
    );

    let listed: Value = serde_json::from_str(&raw).unwrap();
    let listed = listed.as_array().unwrap();
    // Both the bootstrap token and the freshly minted one are visible as views.
    assert!(
        listed
            .iter()
            .any(|t| t["principal_id"] == json!("mgmt-bootstrap"))
    );
    assert!(listed.iter().any(|t| t["role"] == json!("workspace_admin")));
    for view in listed {
        assert!(view.get("secret_hash").is_none());
        assert_eq!(view["workspace_id"], json!(BOOTSTRAP_WORKSPACE));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn non_admin_roles_cannot_mint_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let bootstrap = admin_token(dir.path());

    // workspace_user holds no apikey pattern at all — mint (apikey.write) 403s.
    let (user, _) = mint_http(&app, &bootstrap, BOOTSTRAP_WORKSPACE, "workspace_user").await;
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&user),
        Some(mint_body(BOOTSTRAP_WORKSPACE, "workspace_user")),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{err}");
    assert_eq!(err["error"]["type"], json!("permission_error"));

    // …and cannot even read the token list (apikey.read).
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&user),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_role_is_a_422_problem() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let bootstrap = admin_token(dir.path());

    let (s, raw, content_type) = call_raw(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(mint_body(BOOTSTRAP_WORKSPACE, "superuser")),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    let err: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(err["code"], json!("unknown_role"));
    assert_eq!(err["status"], json!(422));
    assert!(err["detail"].as_str().unwrap().contains("superuser"));

    // Missing required fields are 422 problems too.
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({ "role": "workspace_admin" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err["code"], json!("invalid_token_spec"));

    // A garbage expiry never reaches the engine's lexical comparison.
    let (s, err) = call(
        &app,
        "POST",
        "/v1/config/iam/tokens",
        Some(&bootstrap),
        Some(json!({
            "workspace_id": BOOTSTRAP_WORKSPACE, "role": "workspace_admin",
            "expires_at": "banana"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
    assert_eq!(err["code"], json!("invalid_token_spec"));
}

#[tokio::test(flavor = "multi_thread")]
async fn revocation_is_immediate_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let bootstrap;
    let revoked_cleartext;
    let keeper_cleartext;
    {
        let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
        bootstrap = admin_token(dir.path());
        let (revoked, revoked_id) =
            mint_http(&app, &bootstrap, BOOTSTRAP_WORKSPACE, "workspace_admin").await;
        let (keeper, _) = mint_http(&app, &bootstrap, BOOTSTRAP_WORKSPACE, "workspace_admin").await;
        revoked_cleartext = revoked;
        keeper_cleartext = keeper;

        // Live before the revoke…
        let (s, _) = call(
            &app,
            "GET",
            "/v1/config/catalog",
            Some(&revoked_cleartext),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        let (s, view) = call(
            &app,
            "DELETE",
            &format!("/v1/config/iam/tokens/{revoked_id}"),
            Some(&bootstrap),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{view}");
        assert!(view["revoked_at"].as_str().is_some(), "{view}");

        // …401 immediately after, while the sibling token still works.
        let (s, err) = call(
            &app,
            "GET",
            "/v1/config/catalog",
            Some(&revoked_cleartext),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{err}");
        assert_eq!(err["error"]["type"], json!("authentication_error"));
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("revoked")
        );
        let (s, _) = call(
            &app,
            "GET",
            "/v1/config/catalog",
            Some(&keeper_cleartext),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    } // "process" ends

    // Restart over the same dir: the revocation hydrated from the rewritten
    // row — still 401 — and the untouched tokens still authenticate.
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let (s, _) = call(
        &app,
        "GET",
        "/v1/config/catalog",
        Some(&revoked_cleartext),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(
        &app,
        "GET",
        "/v1/config/catalog",
        Some(&keeper_cleartext),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, "GET", "/v1/config/catalog", Some(&bootstrap), None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bootstrap_token_rotates_to_a_minted_successor() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let bootstrap = admin_token(dir.path());

    // Mint the successor with the FULL admin role, then use the successor to
    // revoke the bootstrap token (its workspace is wrkspc_default; the
    // successor's admin binding there authorizes apikey.write).
    let (successor, _) = mint_http(&app, &bootstrap, BOOTSTRAP_WORKSPACE, "admin").await;
    let (s, listed) = call(
        &app,
        "GET",
        &format!("/v1/config/iam/tokens?workspace_id={BOOTSTRAP_WORKSPACE}"),
        Some(&successor),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let bootstrap_id = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["principal_id"] == json!("mgmt-bootstrap"))
        .and_then(|t| t["id"].as_str())
        .expect("bootstrap token is listed")
        .to_string();

    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/v1/config/iam/tokens/{bootstrap_id}"),
        Some(&successor),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Rotation proven: the old credential 401s, the successor passes.
    let (s, _) = call(&app, "GET", "/v1/config/catalog", Some(&bootstrap), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&app, "GET", "/v1/config/catalog", Some(&successor), None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_may_revoke_itself_and_the_request_completes() {
    let dir = tempfile::tempdir().unwrap();
    let (app, iam) = build_secured_management_router(dir.path(), &KEY).await;

    // Minted via the embedding path so the test controls the id.
    let cleartext = iam
        .mint_service_token(TokenSpec {
            token_id: "tok_selfrevoke".into(),
            service_id: "ci-self".into(),
            workspace_id: BOOTSTRAP_WORKSPACE.into(),
            role: "workspace_admin".into(),
            created_at: None,
            expires_at: None,
        })
        .unwrap();

    // The revoke request itself completes (documented): authn happened before
    // the revocation landed…
    let (s, view) = call(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/tok_selfrevoke",
        Some(&cleartext),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{view}");
    assert!(view["revoked_at"].as_str().is_some());

    // …and every subsequent call 401s.
    let (s, _) = call(&app, "GET", "/v1/config/catalog", Some(&cleartext), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_an_unknown_token_id_is_a_404_problem() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _iam) = build_secured_management_router(dir.path(), &KEY).await;
    let bootstrap = admin_token(dir.path());

    let (s, raw, content_type) = call_raw(
        &app,
        "DELETE",
        "/v1/config/iam/tokens/tok_missing",
        Some(&bootstrap),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{raw}");
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    let err: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(err["code"], json!("not_found"));

    // And the surface stays behind the guard: no token at all → 401.
    let (s, _) = call(
        &app,
        "GET",
        "/v1/config/iam/tokens?workspace_id=x",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
