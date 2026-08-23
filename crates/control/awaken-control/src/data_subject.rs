//! HTTP adapters for Data Subject consent, enrollment, capture decisions, and erasure.

use std::sync::Arc;

use awaken_data_subject_application::{
    CaptureDecisionRecord, ConsentRecord, DataSubjectApplication, DataSubjectApplicationError,
};
use awaken_runtime_contract::{ContentCapture, DataSubjectResolver, ErasureReceipt, Purpose};
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

#[derive(Debug, Serialize)]
struct CaptureDecisionView {
    requested: ContentCapture,
    ceiling: ContentCapture,
    consent: ContentCapture,
    effective: ContentCapture,
    reason: &'static str,
}

impl From<CaptureDecisionRecord> for CaptureDecisionView {
    fn from(record: CaptureDecisionRecord) -> Self {
        Self {
            requested: record.requested,
            ceiling: record.ceiling,
            consent: record.consent,
            effective: record.effective,
            reason: record.reason,
        }
    }
}

struct ConsentHttpState {
    application: Arc<DataSubjectApplication>,
    org: String,
    ceiling: ContentCapture,
}

pub fn consent_router(
    application: Arc<DataSubjectApplication>,
    org: String,
    ceiling: ContentCapture,
) -> Router {
    let state = Arc::new(ConsentHttpState {
        application,
        org,
        ceiling,
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

async fn capture_decision(
    State(state): State<Arc<ConsentHttpState>>,
    Path(id): Path<String>,
    Query(query): Query<DecisionQuery>,
) -> Result<Json<CaptureDecisionView>, StatusCode> {
    state
        .application
        .capture_decision(
            &state.org,
            &id,
            query.requested.unwrap_or(ContentCapture::Full),
            state.ceiling,
        )
        .await
        .map(CaptureDecisionView::from)
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
                name: None,
            })
            .await
            .unwrap();
        let state = Arc::new(ConsentHttpState {
            application,
            org: "org_1".into(),
            ceiling: ContentCapture::Full,
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
}
