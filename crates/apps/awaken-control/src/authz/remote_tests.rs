use std::net::TcpListener as StdListener;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use awaken_iam_contract::{
    ApiTokenStatus, AuthorizationDecision, AuthorizationOutcome, AuthorizationRequest, Jwks,
    PrincipalRef, TokenIntrospectionRequest, TokenIntrospectionResponse, WorkspaceId,
};
use awaken_iam_host::{AccessTokenAuthority, AccessTokenClaims, LocalSeedSigner};
use awaken_tenancy::workspace_purpose::OrdinaryWorkspaceAdmission;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tower::ServiceExt as _;

use super::{RemoteManagementAuthz, cloud_authorization_denial_detail};
use crate::authz::{cloud_management_guard, qualify_hosted_runtime_action, unix_seconds_at};

const SEED: [u8; 32] = [19; 32];
const KID: &str = "awaken-local-cloud-test";
const ISSUER: &str = "https://fake-accounts.test";
const AUDIENCE: &str = "awaken-runtime";
const SCOPED_KEY: &str = concat!("sk-", "awaken-", "fixture-workspace-coordinate");

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
    authorization_status: Arc<AtomicU16>,
}

async fn jwks(State(state): State<CloudIamState>) -> Json<Jwks> {
    Json(state.jwks)
}

async fn authorize(
    State(state): State<CloudIamState>,
    headers: HeaderMap,
    Json(_request): Json<AuthorizationRequest>,
) -> Response {
    state.authorization_bearers.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .trim_start_matches("Bearer ")
            .to_owned(),
    );
    let status = StatusCode::from_u16(state.authorization_status.load(Ordering::SeqCst)).unwrap();
    if status != StatusCode::OK {
        return status.into_response();
    }
    Json(AuthorizationOutcome {
        decision: AuthorizationDecision::Allow,
        reason: "test-policy".into(),
        matched_grants: Vec::new(),
        matched_roles: Vec::new(),
        obligation: None,
    })
    .into_response()
}

struct CloudIamFixture {
    base_url: String,
    authorization_bearers: Arc<Mutex<Vec<String>>>,
    authorization_status: Arc<AtomicU16>,
    token_introspections: Arc<AtomicUsize>,
}

#[test]
fn remote_management_keeps_the_original_introspected_workspace_before_pdp() {
    // Cause/effect decision rows: K1 bound ws_cloud + installed ws_foreign +
    // no selector -> use ws_cloud; K2 explicit matching path/Header -> allow;
    // K3 foreign/conflicting path/Header -> 403 before PDP/handler even when
    // PDP would Allow both; K4 malformed/repeated Header -> 400 before effects.
    let fixture = cloud_iam_fixture();
    let authz = connect_key_fixture(fixture.base_url);
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let app = Router::new()
        .route(
            "/v1/config/catalog",
            get(move || {
                let handler_calls = handler_calls.clone();
                async move {
                    handler_calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            authz,
            cloud_management_guard,
        ));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for (path, headers, status) in [
            (None, vec![], StatusCode::OK),
            (Some("ws_cloud"), vec![], StatusCode::OK),
            (None, vec!["ws_cloud"], StatusCode::OK),
            (Some("ws_cloud"), vec!["ws_cloud"], StatusCode::OK),
            (Some("ws_foreign"), vec![], StatusCode::FORBIDDEN),
            (None, vec!["ws_foreign"], StatusCode::FORBIDDEN),
            (Some("ws_cloud"), vec!["ws_foreign"], StatusCode::FORBIDDEN),
            (None, vec![""], StatusCode::BAD_REQUEST),
            (None, vec!["ws_cloud", "ws_cloud"], StatusCode::BAD_REQUEST),
        ] {
            let mut request = key_request(path.unwrap_or("ws_cloud"));
            if path.is_none() {
                request
                    .extensions_mut()
                    .remove::<awaken_authz_enforce::RequestTenancy>();
            }
            request
                .extensions_mut()
                .insert(awaken_runtime_contract::runs::scope::WorkspaceScope(
                    "ws_foreign".into(),
                ));
            for header in headers {
                request
                    .headers_mut()
                    .append("anthropic-workspace-id", header.parse().unwrap());
            }
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), status, "path={path:?}");
            if status == StatusCode::OK {
                assert_eq!(response.headers()["anthropic-workspace-id"], "ws_cloud");
            }
        }
    });
    assert_eq!(fixture.token_introspections.load(Ordering::SeqCst), 9);
    assert_eq!(fixture.authorization_bearers.lock().unwrap().len(), 4);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

fn connect_key_fixture(base_url: String) -> Arc<RemoteManagementAuthz> {
    let source: Arc<awaken_agent_contract::RedactedStringSource> = Arc::new(|| {
        Ok(awaken_agent_contract::RedactedString::new(
            "fixture-control-carrier",
        ))
    });
    RemoteManagementAuthz::connect_with_user_token_source(
        base_url,
        AUDIENCE.into(),
        ISSUER.into(),
        source,
        Some("fixture-control-carrier".into()),
        awaken_time::system_clock(),
    )
    .unwrap()
}

fn key_request(workspace: &str) -> Request {
    Request::builder()
        .uri("/v1/config/catalog")
        .extension(awaken_authz_enforce::RequestTenancy {
            workspace_id: workspace.into(),
        })
        .extension(awaken_runtime_contract::runs::scope::WorkspaceScope(
            workspace.into(),
        ))
        .header("authorization", format!("Bearer {SCOPED_KEY}"))
        .body(Body::empty())
        .unwrap()
}

struct PurposeObservation {
    status: AtomicU16,
    observed: Mutex<Vec<String>>,
}

#[test]
fn ordinary_management_admission_cannot_reuse_positive_workspace_purpose() {
    let fixture = cloud_iam_fixture();
    let authz = connect_key_fixture(fixture.base_url);
    let purpose = Arc::new(PurposeObservation {
        status: AtomicU16::new(204),
        observed: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/v1/config/catalog", get(|| async { StatusCode::OK }))
        .layer(axum::middleware::from_fn_with_state(
            authz,
            cloud_management_guard,
        ));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/purpose", listener.local_addr().unwrap());
        let source = purpose.clone();
        let owner = Router::new().route(
            "/purpose",
            post(
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let source = source.clone();
                    async move {
                        assert_eq!(headers["x-iam-audience"], AUDIENCE);
                        assert_eq!(headers["authorization"], "Bearer fixture-purpose-carrier");
                        source
                            .observed
                            .lock()
                            .unwrap()
                            .push(body["workspace_id"].as_str().unwrap().to_owned());
                        StatusCode::from_u16(source.status.load(Ordering::SeqCst)).unwrap()
                    }
                },
            ),
        );
        let server = tokio::spawn(async move { axum::serve(listener, owner).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let projection = directory.path().join("purpose-token");
        std::fs::write(&projection, "fixture-purpose-carrier").unwrap();
        let authority: Arc<dyn OrdinaryWorkspaceAdmission> = Arc::new(
            crate::HttpOrdinaryWorkspaceAdmission::from_projected_service_token(
                &endpoint, AUDIENCE, projection,
            )
            .unwrap(),
        );
        let app = app.layer(axum::Extension(authority));
        for (observed, expected) in [
            (204, StatusCode::OK),
            (404, StatusCode::FORBIDDEN),
            (503, StatusCode::SERVICE_UNAVAILABLE),
            (200, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            purpose.status.store(observed, Ordering::SeqCst);
            assert_eq!(
                app.clone()
                    .oneshot(key_request("ws_cloud"))
                    .await
                    .unwrap()
                    .status(),
                expected,
            );
        }
        server.abort();
    });
    assert_eq!(*purpose.observed.lock().unwrap(), ["ws_cloud"; 4]);
    assert_eq!(fixture.authorization_bearers.lock().unwrap().len(), 1);
}

#[test]
fn remote_management_never_drops_a_verified_actor_before_pdp() {
    use awaken_iam_host::AccessTokenSubjectKind::{Account, Service};
    use awaken_iam_server::{AccessTokenActor, DelegatedAccessTokenClaims};

    let fixture = cloud_iam_fixture();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let cases = runtime.block_on(async {
        let now = unix_seconds_at(awaken_time::system_clock().as_ref());
        let authority = AccessTokenAuthority::new(LocalSeedSigner::new(KID, SEED));
        let mut cases = Vec::new();
        for (name, kind) in [("account", Account), ("service", Service)] {
            let claims = AccessTokenClaims {
                iss: ISSUER.into(),
                sub: format!("direct-{name}"),
                subject_kind: kind,
                aud: AUDIENCE.into(),
                exp: now + 3600,
                iat: now,
                jti: format!("direct-{name}"),
                scope: vec!["workspace:ws_cloud".into()],
            };
            cases.push((StatusCode::OK, authority.mint(&claims).await.unwrap()));
            for (actor_name, actor_kind) in [("account", Account), ("service", Service)] {
                let mut delegated = claims.clone();
                delegated.jti = format!("{name}-with-{actor_name}-actor");
                let token = authority
                    .mint_delegated(&DelegatedAccessTokenClaims {
                        claims: delegated,
                        act: Some(AccessTokenActor {
                            sub: format!("unadmitted-{actor_name}"),
                            subject_kind: actor_kind,
                        }),
                    })
                    .await
                    .unwrap();
                cases.push((StatusCode::UNAUTHORIZED, token));
            }
        }
        cases
    });
    let current = cases[0].1.clone();
    let source: Arc<awaken_agent_contract::RedactedStringSource> =
        Arc::new(move || Ok(awaken_agent_contract::RedactedString::new(current.clone())));
    let authz = RemoteManagementAuthz::connect_with_user_token_source(
        fixture.base_url,
        AUDIENCE.into(),
        ISSUER.into(),
        source,
        None,
        awaken_time::system_clock(),
    )
    .unwrap();
    let handlers = Arc::new(AtomicUsize::new(0));
    let handler_calls = handlers.clone();
    let app = Router::new()
        .route(
            "/v1/config/catalog",
            get(move || {
                let calls = handler_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            authz,
            cloud_management_guard,
        ));
    runtime.block_on(async {
        for (expected, token) in cases {
            let pdp_before = fixture.authorization_bearers.lock().unwrap().len();
            let handlers_before = handlers.load(Ordering::SeqCst);
            let mut request = Request::builder()
                .uri("/v1/config/catalog")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(awaken_runtime_contract::runs::scope::WorkspaceScope(
                    "ws_cloud".into(),
                ));
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                expected
            );
            let admitted = usize::from(expected == StatusCode::OK);
            assert_eq!(
                fixture.authorization_bearers.lock().unwrap().len(),
                pdp_before + admitted,
                "a subject Allow cannot discharge an unsupported actor"
            );
            assert_eq!(
                handlers.load(Ordering::SeqCst),
                handlers_before + admitted,
                "rejected actor credentials have no protected-handler effects"
            );
        }
    });
}

fn cloud_iam_fixture() -> CloudIamFixture {
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let authorization_bearers = Arc::new(Mutex::new(Vec::new()));
    let server_bearers = Arc::clone(&authorization_bearers);
    let authorization_status = Arc::new(AtomicU16::new(200));
    let server_status = Arc::clone(&authorization_status);
    let token_introspections = Arc::new(AtomicUsize::new(0));
    let server_introspections = token_introspections.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let state = CloudIamState {
                jwks: AccessTokenAuthority::new(LocalSeedSigner::new(KID, SEED)).jwks(),
                authorization_bearers: server_bearers,
                authorization_status: server_status,
            };
            let app = Router::new()
                .route("/.well-known/jwks.json", get(jwks))
                .route("/v1/authorize", post(authorize))
                .route(
                    "/v1/tokens/introspect",
                    post(move |Json(request): Json<TokenIntrospectionRequest>| {
                        let server_introspections = server_introspections.clone();
                        async move {
                            server_introspections.fetch_add(1, Ordering::SeqCst);
                            if request.token != SCOPED_KEY {
                                return StatusCode::UNAUTHORIZED.into_response();
                            }
                            Json(TokenIntrospectionResponse {
                                principal: PrincipalRef::Service {
                                    service_id: "shared-management-service".into(),
                                },
                                workspace: WorkspaceId("ws_cloud".into()),
                                status: ApiTokenStatus::Active,
                            })
                            .into_response()
                        }
                    }),
                )
                .with_state(state);
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            ready_tx.send(()).ok();
            axum::serve(listener, app).await.unwrap();
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let base_url = format!("http://{address}");
    CloudIamFixture {
        base_url,
        authorization_bearers,
        authorization_status,
        token_introspections,
    }
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
    let CloudIamFixture {
        base_url,
        authorization_bearers,
        authorization_status,
        ..
    } = cloud_iam_fixture();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (token, rotated_token, service_token) = runtime.block_on(async {
        let now = unix_seconds_at(awaken_time::system_clock().as_ref());
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
        let service_token = authority
            .mint(&AccessTokenClaims {
                iss: ISSUER.into(),
                sub: "system:serviceaccount:flow:flow-cell".into(),
                subject_kind: awaken_iam_host::AccessTokenSubjectKind::Service,
                aud: AUDIENCE.into(),
                exp: now + 3600,
                iat: now,
                jti: "flow-runtime-readiness".into(),
                scope: vec!["workspace:ws_cloud".into()],
            })
            .await
            .unwrap();
        (token, rotated_token, service_token)
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
        awaken_time::system_clock(),
    )
    .unwrap();

    async fn ok() -> StatusCode {
        StatusCode::OK
    }
    let handlers = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&handlers);
    let app = Router::new()
        .route(
            "/v1/config/catalog",
            get(move || {
                let calls = Arc::clone(&handler_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            authz,
            cloud_management_guard,
        ));
    runtime.block_on(async {
        let request =
            |bearer: Option<&str>| {
                let mut builder = Request::builder().uri("/v1/config/catalog");
                if let Some(bearer) = bearer {
                    builder = builder.header("authorization", format!("Bearer {bearer}"));
                }
                let mut request = builder.body(Body::empty()).unwrap();
                request.extensions_mut().insert(
                    awaken_runtime_contract::runs::scope::WorkspaceScope("ws_cloud".into()),
                );
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

    // R6 valid immutable-JWKS authentication + a real HTTP PDP 503 ->
    // Managed 503 with zero protected-handler effects (not a credential 401 or
    // policy 403). R7 the original PDP recovers -> the same token reaches the
    // same handler. Original transport retry owns retries, no adapter classifier.
    runtime.block_on(async {
        let request =
            || {
                let mut request = Request::builder()
                    .uri("/v1/config/catalog")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap();
                request.extensions_mut().insert(
                    awaken_runtime_contract::runs::scope::WorkspaceScope("ws_cloud".into()),
                );
                request
            };
        let before = handlers.load(Ordering::SeqCst);
        authorization_status.store(503, Ordering::SeqCst);
        assert_eq!(
            app.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(handlers.load(Ordering::SeqCst), before);
        authorization_status.store(200, Ordering::SeqCst);
        assert_eq!(
            app.clone().oneshot(request()).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(handlers.load(Ordering::SeqCst), before + 1);
    });

    let projected_dir = tempfile::tempdir().unwrap();
    let projected_token = projected_dir.path().join("management-token");
    std::fs::write(&projected_token, "service-test\n").unwrap();
    let hosted = RemoteManagementAuthz::connect_with_projected_service_token(
        base_url,
        AUDIENCE.into(),
        ISSUER.into(),
        projected_token,
        awaken_time::system_clock(),
    )
    .unwrap();
    let hosted_app = Router::new()
        .route("/v1/config/catalog", get(ok))
        .route("/v1/workers/readiness", get(ok))
        .layer(axum::middleware::from_fn_with_state(
            hosted,
            cloud_management_guard,
        ));
    runtime.block_on(async {
        let request =
            |bearer: Option<&str>| {
                let mut builder = Request::builder().uri("/v1/config/catalog");
                if let Some(bearer) = bearer {
                    builder = builder.header("authorization", format!("Bearer {bearer}"));
                }
                let mut request = builder.body(Body::empty()).unwrap();
                request.extensions_mut().insert(
                    awaken_runtime_contract::runs::scope::WorkspaceScope("ws_cloud".into()),
                );
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
                .clone()
                .oneshot(request(Some(&token)))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        // Cause/effect rules for the internal readiness route: R1 a Flow
        // service bearer with the resolved Workspace and an IAM Allow reaches
        // the Coordinator handler; R2 no bearer never falls back to the
        // projected service credential. The route-policy test separately
        // covers an unmapped Worker path and absence from public route export.
        let fleet_request =
            |bearer: Option<&str>| {
                let mut builder = Request::builder().uri("/v1/workers/readiness");
                if let Some(bearer) = bearer {
                    builder = builder.header("authorization", format!("Bearer {bearer}"));
                }
                let mut request = builder.body(Body::empty()).unwrap();
                request.extensions_mut().insert(
                    awaken_runtime_contract::runs::scope::WorkspaceScope("ws_cloud".into()),
                );
                request
            };
        assert_eq!(
            hosted_app
                .clone()
                .oneshot(fleet_request(None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            hosted_app
                .oneshot(fleet_request(Some(&service_token)))
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
                authorization_status: Arc::new(AtomicU16::new(200)),
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
        let now = unix_seconds_at(awaken_time::system_clock().as_ref());
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
        awaken_time::system_clock(),
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
            .insert(awaken_runtime_contract::runs::scope::WorkspaceScope(
                "ws_cloud".into(),
            ));
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
