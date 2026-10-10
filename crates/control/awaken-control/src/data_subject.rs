// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: This source implements the repository-controlled memory product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! HTTP adapters for Data Subject consent, enrollment, capture decisions, and erasure.

use std::sync::Arc;

use awaken_data_governance_contract::{CapturePolicy, CapturePolicyDecision};
use awaken_data_governance_contract::{DataSubjectResolver, ErasureReceipt};
use awaken_data_subject_application::{
    ConsentRecord, DataSubjectApplication, DataSubjectApplicationError,
};
use awaken_runtime_contract::{ContentCapture, Purpose};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

struct ErasureHttpState {
    application: Arc<DataSubjectApplication>,
    resolver: Arc<dyn DataSubjectResolver>,
    org: String,
}

pub fn erasure_router(
    application: Arc<DataSubjectApplication>,
    resolver: Arc<dyn DataSubjectResolver>,
    org: String,
) -> Router {
    Router::new()
        .route("/v1/user_profiles/{id}/erasure", post(erase))
        .with_state(Arc::new(ErasureHttpState {
            application,
            resolver,
            org,
        }))
}

async fn erase(
    State(state): State<Arc<ErasureHttpState>>,
    Path(id): Path<String>,
) -> Result<Json<ErasureReceipt>, StatusCode> {
    state
        .application
        .erase_user_profile(&state.org, &id, state.resolver.as_ref())
        .await
        .map(Json)
        .map_err(status_of)
}

#[derive(Debug, Deserialize)]
struct GrantBody {
    purpose: Purpose,
    #[serde(default)]
    version: String,
}

#[derive(Debug, Serialize)]
struct ConsentView {
    id: String,
    grants: Vec<awaken_data_subject_application::ConsentGrant>,
    telemetry_content_ceiling: ContentCapture,
}

impl From<ConsentRecord> for ConsentView {
    fn from(record: ConsentRecord) -> Self {
        Self {
            id: record.id,
            grants: record.grants,
            telemetry_content_ceiling: record.telemetry_content_ceiling,
        }
    }
}

struct ConsentHttpState {
    application: Arc<DataSubjectApplication>,
    org: String,
    /// The Run boundary's ceiling and policy sources, so the decision read
    /// reports what a Run in the request's Workspace decides.
    capture_policy: CapturePolicy,
}

pub fn consent_router(
    application: Arc<DataSubjectApplication>,
    org: String,
    capture_policy: CapturePolicy,
) -> Router {
    let state = Arc::new(ConsentHttpState {
        application,
        org,
        capture_policy,
    });
    Router::new()
        .route(
            "/v1/user_profiles/{id}/consent",
            post(grant_consent).get(read_consent),
        )
        .route(
            "/v1/user_profiles/{id}/capture-decision",
            get(capture_decision),
        )
        .route("/v1/user_profiles/{id}/enroll", post(mint_enrollment))
        .route("/enroll/{token}", get(enroll_page))
        .route("/enroll/{token}/grant", post(enroll_grant))
        .with_state(state)
}

fn status_of(error: DataSubjectApplicationError) -> StatusCode {
    match error {
        DataSubjectApplicationError::NotFound => StatusCode::NOT_FOUND,
        DataSubjectApplicationError::InvalidEnrollment => StatusCode::BAD_REQUEST,
        DataSubjectApplicationError::Conflict => StatusCode::CONFLICT,
        DataSubjectApplicationError::WeakEnrollmentKey
        | DataSubjectApplicationError::Erasure(_)
        | DataSubjectApplicationError::Repository(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn grant_consent(
    State(state): State<Arc<ConsentHttpState>>,
    Path(id): Path<String>,
    Json(body): Json<GrantBody>,
) -> Result<Json<ConsentView>, StatusCode> {
    state
        .application
        .grant_consent_for_purpose(&state.org, &id, body.purpose, body.version)
        .await
        .map(ConsentView::from)
        .map(Json)
        .map_err(status_of)
}

async fn read_consent(
    State(state): State<Arc<ConsentHttpState>>,
    Path(id): Path<String>,
) -> Result<Json<ConsentView>, StatusCode> {
    state
        .application
        .read_consent(&state.org, &id)
        .await
        .map(ConsentView::from)
        .map(Json)
        .map_err(status_of)
}

#[derive(Debug, Deserialize)]
struct DecisionQuery {
    #[serde(default)]
    requested: Option<ContentCapture>,
}

/// The decision a Run attributed to this profile makes in the request's
/// Workspace (ADR-0050 D8): the same kernel and sources as the Run boundary.
async fn capture_decision(
    State(state): State<Arc<ConsentHttpState>>,
    Path(id): Path<String>,
    Query(query): Query<DecisionQuery>,
    crate::webhook_authoring::RequiredWorkspaceScope(workspace): crate::webhook_authoring::RequiredWorkspaceScope,
) -> Result<Json<CapturePolicyDecision>, StatusCode> {
    state
        .application
        .capture_decision(
            &state.org,
            &id,
            query.requested.unwrap_or(ContentCapture::Full),
            &state.capture_policy,
            &workspace,
        )
        .await
        .map(Json)
        .map_err(status_of)
}

async fn mint_enrollment(
    State(state): State<Arc<ConsentHttpState>>,
    Path(id): Path<String>,
    Query(query): Query<GrantBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    state
        .application
        .mint_enrollment_for_purposes(&state.org, &id, vec![query.purpose])
        .await
        .map(|ticket| {
            Json(serde_json::json!({
                "type": "enrollment_url",
                "url": ticket.url,
                "expires_at": ticket.expires_at,
            }))
        })
        .map_err(status_of)
}

fn token_from_path(path: &str) -> &str {
    path.strip_prefix("/enroll/").unwrap_or(path)
}

async fn enroll_page(
    State(state): State<Arc<ConsentHttpState>>,
    Path(token): Path<String>,
) -> Html<String> {
    match state
        .application
        .inspect_enrollment(token_from_path(&token))
    {
        Ok(request) => Html(format!(
            "<!doctype html><h1>Consent</h1><p>Grant {:?} for <b>{}</b>?</p>\
             <form method=\"post\" action=\"/enroll/{token}/grant\">\
             <button type=\"submit\">Accept</button></form>",
            request.purposes, request.subject_id
        )),
        Err(_) => Html("<!doctype html><p>This enrollment link is invalid or expired.</p>".into()),
    }
}

async fn enroll_grant(
    State(state): State<Arc<ConsentHttpState>>,
    Path(token): Path<String>,
) -> Result<Html<String>, StatusCode> {
    state
        .application
        .accept_enrollment(token_from_path(&token))
        .await
        .map(|_| Html("<!doctype html><p>Thank you — your consent has been recorded.</p>".into()))
        .map_err(status_of)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_data_subject_application::{CreateUserProfileCommand, UserProfileRelationship};
    use awaken_data_subject_store::InMemoryDataSubjectRepo;

    fn application() -> Arc<DataSubjectApplication> {
        Arc::new(
            DataSubjectApplication::new(
                Arc::new(InMemoryDataSubjectRepo::new()),
                b"test-control-enrollment-signing-key".to_vec(),
                awaken_time::system_clock(),
            )
            .expect("test key is strong"),
        )
    }

    #[tokio::test]
    async fn consent_http_delegates_to_the_same_profile_aggregate() {
        // Cause/effect graph: C1 profile exists in org; C2 purpose is telemetry;
        // C3 consent is granted through the Control adapter. Effects: E1 the
        // application mutates that exact aggregate, E2 the read projects Full.
        // Decision rule R1 = C1+C2+C3 -> E1+E2. Missing/cross-org/error rules are
        // covered by the application decision table.
        // Constraints/invariants: the HTTP adapter owns no parallel consent state
        // and cannot cross the configured organization boundary.
        let application = application();
        let profile = application
            .create_user_profile(CreateUserProfileCommand {
                org: "org_1".into(),
                metadata: Default::default(),
                relationship: UserProfileRelationship::External,
                access_type: None,
                external_id: None,
                external_user_onboarded_at: None,
                external_user_details: None,
                name: None,
            })
            .await
            .unwrap();
        let state = Arc::new(ConsentHttpState {
            application,
            org: "org_1".into(),
            capture_policy: CapturePolicy::new(ContentCapture::Full, Default::default()),
        });
        let Json(granted) = grant_consent(
            State(state.clone()),
            Path(profile.id.clone()),
            Json(GrantBody {
                purpose: Purpose::TelemetryContent,
                version: "v1".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(granted.telemetry_content_ceiling, ContentCapture::Full);
        let Json(read) = read_consent(State(state), Path(profile.id)).await.unwrap();
        assert_eq!(read.telemetry_content_ceiling, ContentCapture::Full);
    }

    #[tokio::test]
    async fn capture_decision_reads_the_request_workspace_narrowing() {
        // Causes: C1 deployment ceiling = full; C2 consent granted; C3 the
        // request Workspace's narrowing as authored through
        // /v1/config/telemetry (unset, each level) or unreadable; C4 requested.
        // Effects: E1 the read is exactly the Run boundary's kernel over the
        // same sources; E2 a narrowing authored in one Workspace is reflected
        // by the next read in that Workspace and never in another; E3 an
        // unreadable narrowing reads as off; E4 an unscoped request is 404.
        use crate::telemetry_policy::tests::MapRepository;
        use awaken_data_governance_contract::{
            CapturePolicyInputs, CapturePolicySources, NullResolver, TelemetryNarrowing,
            decide_capture,
        };
        use awaken_tenancy::WorkspaceScope;
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;

        async fn call(
            router: &Router,
            workspace: Option<&str>,
            method: &str,
            uri: &str,
            body: Option<serde_json::Value>,
        ) -> (StatusCode, serde_json::Value) {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json");
            if let Some(workspace) = workspace {
                request = request.extension(WorkspaceScope(workspace.into()));
            }
            let response = router
                .clone()
                .oneshot(
                    request
                        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice(&bytes).unwrap_or_default())
        }

        let application = application();
        let profile = application
            .create_user_profile(CreateUserProfileCommand {
                org: "org_1".into(),
                metadata: Default::default(),
                relationship: UserProfileRelationship::External,
                access_type: None,
                external_id: None,
                external_user_onboarded_at: None,
                external_user_details: None,
                name: None,
            })
            .await
            .unwrap();
        application
            .grant_consent_for_purpose("org_1", &profile.id, Purpose::TelemetryContent, "v1".into())
            .await
            .unwrap();
        let routes = |repository: Arc<MapRepository>| {
            let service = crate::TelemetryPolicyService::new(repository, ContentCapture::Full);
            crate::telemetry_policy::router(service.clone()).merge(consent_router(
                application.clone(),
                "org_1".into(),
                CapturePolicy::new(
                    ContentCapture::Full,
                    CapturePolicySources {
                        consent: Arc::new(NullResolver),
                        narrowing: service.narrowing_source(),
                    },
                ),
            ))
        };
        let router = routes(Arc::new(MapRepository::default()));
        let decision = |requested: &str| {
            format!(
                "/v1/user_profiles/{}/capture-decision?requested={requested}",
                profile.id
            )
        };
        let levels = [
            ("off", ContentCapture::Off),
            ("structured", ContentCapture::Structured),
            ("full", ContentCapture::Full),
        ];
        let mut revision = 0;
        for narrowing in [None, Some("full"), Some("structured"), Some("off"), None] {
            let (status, _) = call(
                &router,
                Some("ws-a"),
                "PUT",
                "/v1/config/telemetry",
                Some(serde_json::json!({
                    "expected_revision": revision,
                    "content_capture": narrowing,
                })),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            revision = if revision == 0 && narrowing.is_none() {
                0
            } else {
                revision + 1
            };
            let narrowing = narrowing.map(|name| {
                levels
                    .iter()
                    .find(|(wire, _)| *wire == name)
                    .map(|(_, level)| *level)
                    .unwrap()
            });
            for (wire, requested) in levels {
                let (status, read) =
                    call(&router, Some("ws-a"), "GET", &decision(wire), None).await;
                assert_eq!(status, StatusCode::OK);
                let expected = decide_capture(CapturePolicyInputs {
                    requested,
                    top: ContentCapture::Full,
                    workspace: Ok(TelemetryNarrowing {
                        content_capture: narrowing,
                    }),
                    consent: Some(ContentCapture::Full),
                });
                assert_eq!(read, serde_json::to_value(expected).unwrap(), "E1 E2");
                let (_, other) = call(&router, Some("ws-b"), "GET", &decision(wire), None).await;
                assert_eq!(other["ceiling"], "full", "E2 other Workspace is unchanged");
            }
        }

        let unreadable = routes(Arc::new(MapRepository {
            unavailable: true,
            ..MapRepository::default()
        }));
        let (status, read) = call(&unreadable, Some("ws-a"), "GET", &decision("full"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(read["ceiling"], "off", "E3");
        assert_eq!(read["effective"], "off", "E3");
        assert_eq!(read["reason"], "clamped_by_ceiling", "E3");

        let (status, _) = call(&router, None, "GET", &decision("full"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "E4");
    }
}
