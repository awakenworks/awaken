use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};

use awaken_iam_contract::{
    AuthorizationOutcome, AuthorizationRequest, Jwks, ScopeRef, WorkspaceId,
};
use awaken_iam_host::{
    AccessTokenActor, AccessTokenAuthority, AccessTokenClaims, AccessTokenSubjectKind,
    DelegatedAccessTokenClaims, LocalSeedSigner,
};
use awaken_service_auth_contract::ProjectedFileServiceBearerTokenSource;
use axum::{
    Extension, Json,
    body::Body,
    extract::Path,
    http::HeaderMap,
    routing::{get, post},
};

use super::*;

const AUDIENCE: &str = "private-fixture";
const ISSUER: &str = "https://accounts.example.test";

struct Authorities {
    jwks: Jwks,
    expected_bearer: String,
    account_status: AtomicU16,
    scope_status: AtomicU16,
    decision_status: AtomicU16,
    wrong_coordinates: AtomicBool,
    userinfo_calls: AtomicUsize,
    scope_calls: AtomicUsize,
    decision_calls: AtomicUsize,
    file_decisions: AtomicUsize,
    cancel_decisions: AtomicUsize,
}

async fn current_account(State(state): State<Arc<Authorities>>, headers: HeaderMap) -> Response {
    state.userinfo_calls.fetch_add(1, Ordering::SeqCst);
    if headers["authorization"] != state.expected_bearer.as_str() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let status = StatusCode::from_u16(state.account_status.load(Ordering::SeqCst)).unwrap();
    if status == StatusCode::OK {
        Json(serde_json::json!({"sub":"alice"})).into_response()
    } else {
        status.into_response()
    }
}

async fn current_context(
    State(state): State<Arc<Authorities>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state.scope_calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        headers["authorization"],
        "Bearer fixture-coordinator-service"
    );
    assert_eq!(headers["x-iam-audience"], AUDIENCE);
    assert_eq!(
        body,
        serde_json::json!({"workspace_id":"assistant-a","account_id":"alice"})
    );
    let status = StatusCode::from_u16(state.scope_status.load(Ordering::SeqCst)).unwrap();
    if status != StatusCode::OK {
        return status.into_response();
    }
    let workspace = if state.wrong_coordinates.load(Ordering::SeqCst) {
        "assistant-b"
    } else {
        "assistant-a"
    };
    Json(
        SessionPrivateBinding::new(
            "org-a".into(),
            "desk".into(),
            "alice".into(),
            workspace.into(),
            3,
        )
        .unwrap(),
    )
    .into_response()
}

async fn decision(
    State(state): State<Arc<Authorities>>,
    Json(request): Json<AuthorizationRequest>,
) -> Response {
    state.decision_calls.fetch_add(1, Ordering::SeqCst);
    if request.action == crate::authz::qualified_action(ActionNamespace::Workspace, "file.read") {
        state.file_decisions.fetch_add(1, Ordering::SeqCst);
    } else if request.action
        == crate::authz::qualified_action(ActionNamespace::HostedRuntime, "run.cancel")
    {
        state.cancel_decisions.fetch_add(1, Ordering::SeqCst);
    } else {
        assert_eq!(
            request.action,
            crate::authz::qualified_action(ActionNamespace::HostedRuntime, "run.read")
        );
    }
    assert_eq!(
        request.principal,
        PrincipalRef::Account {
            account_id: AccountId("alice".into())
        }
    );
    assert_eq!(
        request.scope,
        ScopeRef::Workspace {
            workspace_id: WorkspaceId("assistant-a".into())
        }
    );
    assert!(request.on_behalf_of.is_empty());
    let status = StatusCode::from_u16(state.decision_status.load(Ordering::SeqCst)).unwrap();
    if status != StatusCode::OK {
        return status.into_response();
    }
    Json(AuthorizationOutcome {
        decision: AuthorizationDecision::Allow,
        reason: "fixture".into(),
        matched_grants: Vec::new(),
        matched_roles: Vec::new(),
        obligation: None,
    })
    .into_response()
}

#[test]
fn addressed_private_read_requires_current_account_binding_and_decision_before_the_native_handler()
{
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let signer = AccessTokenAuthority::new(LocalSeedSigner::new("private-ingress", [23; 32]));
    let now = crate::authz::unix_seconds_at(awaken_time::system_clock().as_ref());
    let claims = AccessTokenClaims {
        iss: ISSUER.into(),
        sub: "alice".into(),
        subject_kind: AccessTokenSubjectKind::Account,
        aud: AUDIENCE.into(),
        exp: now + 300,
        iat: now,
        jti: "private-alice".into(),
        scope: vec!["openid".into()],
    };
    let (human, delegated, service) = runtime.block_on(async {
        let human = signer.mint(&claims).await.unwrap();
        let delegated = signer
            .mint_delegated(&DelegatedAccessTokenClaims {
                claims: claims.clone(),
                act: Some(AccessTokenActor {
                    sub: "agent".into(),
                    subject_kind: AccessTokenSubjectKind::Service,
                }),
            })
            .await
            .unwrap();
        let service = signer
            .mint(&AccessTokenClaims {
                subject_kind: AccessTokenSubjectKind::Service,
                ..claims
            })
            .await
            .unwrap();
        (human, delegated, service)
    });
    let state = Arc::new(Authorities {
        jwks: signer.jwks(),
        expected_bearer: format!("Bearer {human}"),
        account_status: AtomicU16::new(200),
        scope_status: AtomicU16::new(200),
        decision_status: AtomicU16::new(200),
        wrong_coordinates: AtomicBool::new(false),
        userinfo_calls: AtomicUsize::new(0),
        scope_calls: AtomicUsize::new(0),
        decision_calls: AtomicUsize::new(0),
        file_decisions: AtomicUsize::new(0),
        cancel_decisions: AtomicUsize::new(0),
    });
    let (base, server) = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route(
                "/.well-known/jwks.json",
                get(
                    |State(state): State<Arc<Authorities>>| async move { Json(state.jwks.clone()) },
                ),
            )
            .route("/v1/oauth/userinfo", get(current_account))
            .route("/private-context", post(current_context))
            .route("/v1/authorize", post(decision))
            .with_state(state.clone());
        (
            url,
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
        )
    });
    let token_source: Arc<awaken_agent_contract::RedactedStringSource> =
        Arc::new(|| Ok(RedactedString::new("fixture-pdp")));
    let iam = RemoteManagementAuthz::connect_with_user_token_source(
        base.clone(),
        AUDIENCE.into(),
        ISSUER.into(),
        token_source,
        Some("fixture-pdp".into()),
        awaken_time::system_clock(),
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let projected = directory.path().join("coordinator-token");
    std::fs::write(&projected, "fixture-coordinator-service").unwrap();
    let ordinary = HttpOrdinaryWorkspaceAdmission::new(
        &format!("{base}/ordinary"),
        AUDIENCE,
        Arc::new(ProjectedFileServiceBearerTokenSource::new(
            projected, "fixture",
        )),
    )
    .unwrap();
    let context = Arc::new(
        HttpPrivateWorkspaceContext::new(&format!("{base}/private-context"), &ordinary).unwrap(),
    );
    let handled = Arc::new(AtomicUsize::new(0));
    let calls = handled.clone();
    let sessions = Router::new().route(
        "/v1/sessions/{id}/events",
        get(
            move |Path(id): Path<String>,
                  Extension(workspace): Extension<WorkspaceScope>,
                  Extension(_private): Extension<AdmittedPrivateSessionRequest>,
                  headers: HeaderMap| {
                let calls = calls.clone();
                async move {
                    assert_eq!(id, "session-a");
                    assert_eq!(workspace.0, "assistant-a");
                    assert!(!headers.contains_key("authorization"));
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let managed = sessions.route(
        "/v1/files/{id}/content",
        get(
            |Path(id): Path<String>,
             Extension(_private): Extension<AdmittedPrivateSessionRequest>,
             headers: HeaderMap| async move {
                assert_eq!(id, "file-a");
                assert!(!headers.contains_key("authorization"));
                StatusCode::ACCEPTED
            },
        ),
    );
    let managed =
        managed.route(
            "/v1/sessions/{id}/events/stream",
            get(
                |Extension(_private): Extension<AdmittedPrivateSessionRequest>,
                 headers: HeaderMap| async move {
                    assert!(!headers.contains_key("authorization"));
                    StatusCode::OK
                },
            ),
        );
    let managed = managed.route(
        "/v1/sessions/{id}/events",
        axum::routing::post(
            |Path(id): Path<String>,
             Extension(_private): Extension<AdmittedPrivateSessionRequest>,
             headers: HeaderMap,
             axum::Json(body): axum::Json<serde_json::Value>| async move {
                assert_eq!(id, "session-a");
                assert_eq!(headers["idempotency-key"], "interrupt-original");
                assert!(!headers.contains_key("authorization"));
                assert!(!headers.contains_key("anthropic-user-profile-id"));
                assert_eq!(
                    body,
                    serde_json::json!({"events":[{"type":"user.interrupt"}]})
                );
                StatusCode::CREATED
            },
        ),
    );
    let app = private_assistant_router(managed, iam, context);
    let path = "/v1/private-assistant/sessions/session-a/events";
    let request = |method: &str, path: &str, bearer: &str, addressed: bool| {
        let builder = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {bearer}"));
        let builder = if addressed {
            builder.extension(RequestTenancy {
                workspace_id: "assistant-a".into(),
            })
        } else {
            builder
        };
        builder.body(Body::empty()).unwrap()
    };
    runtime.block_on(async {
        let call = |req: Request| {
            let app = app.clone();
            async move { app.oneshot(req).await.unwrap().status() }
        };
        assert_eq!(
            call(request("GET", path, &human, false)).await,
            StatusCode::FORBIDDEN
        );
        for token in [&delegated, &service] {
            assert_eq!(
                call(request("GET", path, token, true)).await,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            call(request("POST", path, &human, true)).await,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(state.userinfo_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            call(request("GET", path, &human, true)).await,
            StatusCode::NO_CONTENT
        );
        let stream = format!("{path}/stream");
        assert_eq!(
            call(request("GET", &stream, &human, true)).await,
            StatusCode::OK
        );
        let stop = "/v1/private-assistant/sessions/session-a/stop";
        assert_eq!(
            call(request("POST", stop, &human, true)).await,
            StatusCode::BAD_REQUEST
        );
        let stop_request = |body| {
            let mut req = request("POST", stop, &human, true);
            req.headers_mut()
                .insert("idempotency-key", "interrupt-original".parse().unwrap());
            req.headers_mut()
                .insert("anthropic-user-profile-id", "forged".parse().unwrap());
            *req.body_mut() = body;
            req
        };
        assert_eq!(
            call(stop_request(Body::from(
                r#"{"events":[{"type":"user.message"}]}"#
            )))
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(call(stop_request(Body::empty())).await, StatusCode::CREATED);
        assert_eq!(
            call(request(
                "GET",
                "/v1/private-assistant/files/file-a/content",
                &human,
                true
            ))
            .await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            call(request("GET", "/v1/private-assistant/files", &human, true)).await,
            StatusCode::NOT_FOUND
        );
        state.wrong_coordinates.store(true, Ordering::SeqCst);
        assert_eq!(
            call(request("GET", path, &human, true)).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        state.wrong_coordinates.store(false, Ordering::SeqCst);
        state.scope_status.store(404, Ordering::SeqCst);
        assert_eq!(
            call(request("GET", path, &human, true)).await,
            StatusCode::NOT_FOUND
        );
        state.scope_status.store(200, Ordering::SeqCst);
        state.decision_status.store(503, Ordering::SeqCst);
        assert_eq!(
            call(request("GET", path, &human, true)).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        state.account_status.store(401, Ordering::SeqCst);
        assert_eq!(
            call(request("GET", &stream, &human, true)).await,
            StatusCode::UNAUTHORIZED
        );
    });
    assert_eq!(state.userinfo_calls.load(Ordering::SeqCst), 9);
    assert_eq!(state.scope_calls.load(Ordering::SeqCst), 8);
    assert!(state.decision_calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(state.file_decisions.load(Ordering::SeqCst), 1);
    assert_eq!(state.cancel_decisions.load(Ordering::SeqCst), 2);
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    server.abort();
}
