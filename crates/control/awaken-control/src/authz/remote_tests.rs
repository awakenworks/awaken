use std::net::TcpListener as StdListener;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use awaken_iam_contract::{
    AuthorizationDecision, AuthorizationOutcome, AuthorizationRequest, Jwks,
};
use awaken_iam_host::{AccessTokenAuthority, AccessTokenClaims, LocalSeedSigner};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use tower::ServiceExt as _;

use super::{RemoteManagementAuthz, cloud_authorization_denial_detail};
use crate::authz::{cloud_management_guard, now_unix, qualify_hosted_runtime_action};

const SEED: [u8; 32] = [19; 32];
const KID: &str = "awaken-local-cloud-test";
const ISSUER: &str = "https://fake-accounts.test";
const AUDIENCE: &str = "awaken-runtime";

#[test]
fn cloud_denial_names_the_qualified_action_and_workspace_scope() {
    // Cause/effect decision table: a denied remote action must expose both
    // policy coordinates needed to diagnose its missing grant: the qualified
    // action key and trusted Workspace. It must never regress to the ambiguous
    // legacy "denied this action" text.
    let detail = cloud_authorization_denial_detail(
        &qualify_hosted_runtime_action("run.read"),
        "workspace_customer_a",
    );

    assert_eq!(
        detail,
        "cloud IAM denied action 'awaken.runtime::run.read' at Workspace 'workspace_customer_a'"
    );
}

#[derive(Clone)]
struct CloudIamState {
    jwks: Jwks,
    authorization_bearers: Arc<Mutex<Vec<String>>>,
}

async fn jwks(State(state): State<CloudIamState>) -> Json<Jwks> {
    Json(state.jwks)
}

async fn authorize(
    State(state): State<CloudIamState>,
    headers: HeaderMap,
    Json(_request): Json<AuthorizationRequest>,
) -> Json<AuthorizationOutcome> {
    state.authorization_bearers.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .trim_start_matches("Bearer ")
            .to_owned(),
    );
    Json(AuthorizationOutcome {
        decision: AuthorizationDecision::Allow,
        reason: "test-policy".into(),
        matched_grants: Vec::new(),
        matched_roles: Vec::new(),
        obligation: None,
    })
}

#[test]
fn cloud_guard_tracks_login_rotation_and_explicit_bearer_override() {
    // Causal graph: Cloud Account token -> typed IAM verification -> Account
    // principal -> remote Management authorization -> guarded Awaken route.
    //
    // | Account credential source | bearer validity | result |
    // |---|---|---|
    // | cached Cloud login | valid | authorize |
    // | rotated cached login | valid successor | rebuild PDP carrier and authorize |
    // | explicit bearer | valid | authorize |
    // | explicit bearer | invalid | `401` |
    // | hosted projected service | no user bearer | `401` |
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let authorization_bearers = Arc::new(Mutex::new(Vec::new()));
    let server_bearers = Arc::clone(&authorization_bearers);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let state = CloudIamState {
                jwks: AccessTokenAuthority::new(LocalSeedSigner::new(KID, SEED)).jwks(),
                authorization_bearers: server_bearers,
            };
            let app = Router::new()
                .route("/.well-known/jwks.json", get(jwks))
                .route("/v1/authorize", post(authorize))
                .with_state(state);
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            ready_tx.send(()).ok();
            axum::serve(listener, app).await.unwrap();
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let base_url = format!("http://{address}");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (token, rotated_token) = runtime.block_on(async {
        let now = now_unix();
        let authority = AccessTokenAuthority::new(LocalSeedSigner::new(KID, SEED));
        let token = authority
            .mint(&AccessTokenClaims {
                iss: ISSUER.into(),
                sub: "account-alice".into(),
                subject_kind: awaken_iam_host::AccessTokenSubjectKind::Account,
                aud: AUDIENCE.into(),
                exp: now + 3600,
                iat: now,
                jti: "cloud-login-1".into(),
                scope: Vec::new(),
            })
            .await
            .unwrap();
        let rotated_token = authority
            .mint(&AccessTokenClaims {
                iss: ISSUER.into(),
                sub: "account-alice".into(),
                subject_kind: awaken_iam_host::AccessTokenSubjectKind::Account,
                aud: AUDIENCE.into(),
                exp: now + 3600,
                iat: now,
                jti: "cloud-login-2".into(),
                scope: Vec::new(),
            })
            .await
            .unwrap();
        (token, rotated_token)
    });
    let current_token = Arc::new(Mutex::new(token.clone()));
    let token_source: Arc<awaken_agent_contract::RedactedStringSource> = {
        let current_token = Arc::clone(&current_token);
        Arc::new(move || {
            Ok(awaken_agent_contract::RedactedString::new(
                current_token.lock().unwrap().clone(),
            ))
        })
    };
    let authz = RemoteManagementAuthz::connect_with_user_token_source(
        base_url.clone(),
        AUDIENCE.into(),
        ISSUER.into(),
        token_source,
        None,
    )
    .unwrap();

    async fn ok() -> StatusCode {
        StatusCode::OK
    }
    let app = Router::new().route("/v1/config/catalog", get(ok)).layer(
        axum::middleware::from_fn_with_state(authz, cloud_management_guard),
    );
    runtime.block_on(async {
        let request = |bearer: Option<&str>| {
            let mut builder = Request::builder().uri("/v1/config/catalog");
            if let Some(bearer) = bearer {
                builder = builder.header("authorization", format!("Bearer {bearer}"));
            }
            let mut request = builder.body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(awaken_tenancy::WorkspaceScope("ws_cloud".into()));
            request
        };
        assert_eq!(
            app.clone().oneshot(request(None)).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some(&token)))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some("not-a-jwt")))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        *current_token.lock().unwrap() = rotated_token.clone();
        assert_eq!(
            app.clone().oneshot(request(None)).await.unwrap().status(),
            StatusCode::OK
        );
    });
    assert_eq!(
        authorization_bearers.lock().unwrap().as_slice(),
        [token.as_str(), token.as_str(), rotated_token.as_str()]
    );

    let projected_dir = tempfile::tempdir().unwrap();
    let projected_token = projected_dir.path().join("management-token");
    std::fs::write(&projected_token, "service-test\n").unwrap();
    let hosted = RemoteManagementAuthz::connect_with_projected_service_token(
        base_url,
        AUDIENCE.into(),
        ISSUER.into(),
        projected_token,
    )
    .unwrap();
    let hosted_app = Router::new().route("/v1/config/catalog", get(ok)).layer(
        axum::middleware::from_fn_with_state(hosted, cloud_management_guard),
    );
    runtime.block_on(async {
        let request = |bearer: Option<&str>| {
            let mut builder = Request::builder().uri("/v1/config/catalog");
            if let Some(bearer) = bearer {
                builder = builder.header("authorization", format!("Bearer {bearer}"));
            }
            let mut request = builder.body(Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(awaken_tenancy::WorkspaceScope("ws_cloud".into()));
            request
        };
        assert_eq!(
            hosted_app
                .clone()
                .oneshot(request(None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            hosted_app
                .oneshot(request(Some(&token)))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    });
}

#[test]
fn cloud_tunnel_guard_requires_wif_service_bearer_and_scope() {
    // Cause/effect decision table for the public Tunnel boundary:
    //
    // | transport credential | subject | scope | result |
    // | x-api-key             | any     | any   | 401    |
    // | Bearer                | account | exact | 403    |
    // | Bearer                | service | none  | 403    |
    // | Bearer                | service | exact | 200    |
    // | malformed Authorization + valid x-api-key        | 401 |
    //
    // IAM still evaluates Workspace membership after the workload credential
    // checks; the fake PDP allows it so this test isolates the PEP boundary.
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let state = CloudIamState {
                jwks: AccessTokenAuthority::new(LocalSeedSigner::new(KID, SEED)).jwks(),
                authorization_bearers: Arc::new(Mutex::new(Vec::new())),
            };
            let app = Router::new()
                .route("/.well-known/jwks.json", get(jwks))
                .route("/v1/authorize", post(authorize))
                .with_state(state);
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            ready_tx.send(()).ok();
            axum::serve(listener, app).await.unwrap();
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let base_url = format!("http://{address}");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (account_scoped, service_unscoped, service_scoped) = runtime.block_on(async {
        let now = now_unix();
        let authority = AccessTokenAuthority::new(LocalSeedSigner::new(KID, SEED));
        let mint = |sub: &str,
                    subject_kind: awaken_iam_host::AccessTokenSubjectKind,
                    scope: Vec<String>,
                    jti: &str| {
            let authority = authority.clone();
            let claims = AccessTokenClaims {
                iss: ISSUER.into(),
                sub: sub.into(),
                subject_kind,
                aud: AUDIENCE.into(),
                exp: now + 3600,
                iat: now,
                jti: jti.into(),
                scope,
            };
            async move { authority.mint(&claims).await.unwrap() }
        };
        (
            mint(
                "account-alice",
                awaken_iam_host::AccessTokenSubjectKind::Account,
                vec!["workspace:manage_tunnels".into()],
                "account-scoped",
            )
            .await,
            mint(
                "system:serviceaccount:connectors:tunnel-client",
                awaken_iam_host::AccessTokenSubjectKind::Service,
                Vec::new(),
                "service-unscoped",
            )
            .await,
            mint(
                "system:serviceaccount:connectors:tunnel-client",
                awaken_iam_host::AccessTokenSubjectKind::Service,
                vec!["workspace:manage_tunnels".into()],
                "service-scoped",
            )
            .await,
        )
    });
    let projected_dir = tempfile::tempdir().unwrap();
    let projected_token = projected_dir.path().join("management-token");
    std::fs::write(&projected_token, "service-test\n").unwrap();
    let authz = RemoteManagementAuthz::connect_with_projected_service_token(
        base_url,
        AUDIENCE.into(),
        ISSUER.into(),
        projected_token,
    )
    .unwrap();
    async fn ok() -> StatusCode {
        StatusCode::OK
    }
    let app = Router::new()
        .route("/v1/tunnels", get(ok))
        .route("/v1/organizations/tunnels", get(ok))
        .layer(axum::middleware::from_fn_with_state(
            authz,
            cloud_management_guard,
        ));
    let request = |authorization: Option<&str>, api_key: Option<&str>| {
        let mut builder = Request::builder().uri("/v1/tunnels");
        if let Some(authorization) = authorization {
            builder = builder.header("authorization", authorization);
        }
        if let Some(api_key) = api_key {
            builder = builder.header("x-api-key", api_key);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(awaken_tenancy::WorkspaceScope("ws_cloud".into()));
        request
    };
    runtime.block_on(async {
        assert_eq!(
            app.clone()
                .oneshot(request(None, Some(&service_scoped)))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED,
            "x-api-key is never accepted by /v1/tunnels"
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some(&format!("Bearer {account_scoped}")), None,))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "human/account access tokens are not WIF workload credentials"
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some(&format!("Bearer {service_unscoped}")), None,))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "the official Tunnel scope is mandatory"
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some(&format!("Bearer {service_scoped}")), None,))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Some("Basic invalid"), Some(&service_scoped)))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED,
            "a malformed Authorization header cannot fall back to x-api-key"
        );
        let mut legacy = request(Some(&format!("Bearer {service_scoped}")), None);
        *legacy.uri_mut() = "/v1/organizations/tunnels".parse().unwrap();
        assert_eq!(
            app.clone().oneshot(legacy).await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "WIF credentials cannot silently select legacy Admin API semantics"
        );
    });
}
