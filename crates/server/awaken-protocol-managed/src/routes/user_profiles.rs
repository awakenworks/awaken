//! Managed User Profiles HTTP adapter over the Control-owned Data Subject application.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_data_subject_application::{
    CreateUserProfileCommand, DataSubjectApplication, DataSubjectApplicationError,
    EnrollmentTicket, UpdateUserProfileCommand, UserProfileRecord, UserProfileRelationship,
    UserProfileTrustGrant, UserProfileTrustGrantStatus,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::routes::ManagedJson;
use crate::types::user_profile::{
    EnrollmentUrl, Relationship, TrustGrant, TrustGrantStatus, UserProfile,
    UserProfileCreateParams, UserProfileUpdateParams,
};
use crate::types::{ErrorResponse, Page, PageQuery, paginate};

struct UserProfileHttpState {
    application: Arc<DataSubjectApplication>,
    org: String,
}

/// Mount the User Profiles wire adapter over the one Data Subject application.
pub fn user_profiles_router(application: Arc<DataSubjectApplication>, org: String) -> Router {
    Router::new()
        .route("/v1/user_profiles", post(create_profile).get(list_profiles))
        .route(
            "/v1/user_profiles/{id}",
            get(retrieve_profile).post(update_profile),
        )
        .route(
            "/v1/user_profiles/{id}/enrollment_url",
            post(enrollment_url),
        )
        .with_state(Arc::new(UserProfileHttpState { application, org }))
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn error_response(error: DataSubjectApplicationError) -> WireError {
    let (status, kind, message) = match error {
        DataSubjectApplicationError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found_error",
            "user_profile not found".to_string(),
        ),
        DataSubjectApplicationError::InvalidEnrollment => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            error.to_string(),
        ),
        DataSubjectApplicationError::Conflict => {
            (StatusCode::CONFLICT, "conflict_error", error.to_string())
        }
        DataSubjectApplicationError::WeakEnrollmentKey
        | DataSubjectApplicationError::Erasure(_)
        | DataSubjectApplicationError::Repository(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            error.to_string(),
        ),
    };
    (status, Json(ErrorResponse::new(kind, message)))
}

fn bad_request(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

fn check_len(field: &str, value: Option<&str>) -> Result<(), WireError> {
    if let Some(value) = value
        && value.len() > 255
    {
        return Err(bad_request(format!(
            "{field} must be at most 255 characters"
        )));
    }
    Ok(())
}

fn relationship_to_application(value: Relationship) -> UserProfileRelationship {
    match value {
        Relationship::External => UserProfileRelationship::External,
        Relationship::Resold => UserProfileRelationship::Resold,
        Relationship::Internal => UserProfileRelationship::Internal,
    }
}

fn relationship_to_wire(value: UserProfileRelationship) -> Relationship {
    match value {
        UserProfileRelationship::External => Relationship::External,
        UserProfileRelationship::Resold => Relationship::Resold,
        UserProfileRelationship::Internal => Relationship::Internal,
    }
}

fn grant_to_application(value: TrustGrant) -> UserProfileTrustGrant {
    UserProfileTrustGrant {
        status: match value.status {
            TrustGrantStatus::Active => UserProfileTrustGrantStatus::Active,
            TrustGrantStatus::Pending => UserProfileTrustGrantStatus::Pending,
            TrustGrantStatus::Rejected => UserProfileTrustGrantStatus::Rejected,
        },
    }
}

fn grant_to_wire(value: UserProfileTrustGrant) -> TrustGrant {
    TrustGrant {
        status: match value.status {
            UserProfileTrustGrantStatus::Active => TrustGrantStatus::Active,
            UserProfileTrustGrantStatus::Pending => TrustGrantStatus::Pending,
            UserProfileTrustGrantStatus::Rejected => TrustGrantStatus::Rejected,
        },
    }
}

fn project(record: UserProfileRecord) -> UserProfile {
    UserProfile {
        id: record.id,
        created_at: awaken_session_contract::epoch_millis_to_rfc3339(
            record.created_at.max(0) as u64
        ),
        updated_at: awaken_session_contract::epoch_millis_to_rfc3339(
            record.updated_at.max(0) as u64
        ),
        metadata: record.metadata,
        relationship: relationship_to_wire(record.relationship),
        trust_grants: record
            .trust_grants
            .into_iter()
            .map(|(name, grant)| (name, grant_to_wire(grant)))
            .collect(),
        object_type: "user_profile",
        external_id: record.external_id,
        name: record.name,
    }
}

async fn create_profile(
    State(state): State<Arc<UserProfileHttpState>>,
    ManagedJson(params): ManagedJson<UserProfileCreateParams>,
) -> Result<Json<UserProfile>, WireError> {
    check_len("external_id", params.external_id.as_deref())?;
    check_len("name", params.name.as_deref())?;
    state
        .application
        .create_user_profile(CreateUserProfileCommand {
            org: state.org.clone(),
            metadata: params.metadata,
            relationship: relationship_to_application(params.relationship),
            external_id: params.external_id,
            name: params.name,
        })
        .await
        .map(project)
        .map(Json)
        .map_err(error_response)
}

async fn retrieve_profile(
    State(state): State<Arc<UserProfileHttpState>>,
    Path(id): Path<String>,
) -> Result<Json<UserProfile>, WireError> {
    state
        .application
        .get_user_profile(&state.org, &id)
        .await
        .map(project)
        .map(Json)
        .map_err(error_response)
}

async fn list_profiles(
    State(state): State<Arc<UserProfileHttpState>>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<UserProfile>>, WireError> {
    let data = state
        .application
        .list_user_profiles(&state.org)
        .await
        .map_err(error_response)?
        .into_iter()
        .map(project)
        .collect();
    Ok(Json(paginate(data, &page, |profile| profile.id.as_str())))
}

async fn update_profile(
    State(state): State<Arc<UserProfileHttpState>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<UserProfileUpdateParams>,
) -> Result<Json<UserProfile>, WireError> {
    check_len(
        "external_id",
        params
            .external_id
            .as_ref()
            .and_then(|value| value.as_deref()),
    )?;
    check_len(
        "name",
        params.name.as_ref().and_then(|value| value.as_deref()),
    )?;
    let trust_grants = params.trust_grants.map(|grants| {
        grants
            .into_iter()
            .map(|(name, grant)| (name, grant_to_application(grant)))
            .collect::<BTreeMap<_, _>>()
    });
    state
        .application
        .update_user_profile(
            &state.org,
            &id,
            UpdateUserProfileCommand {
                metadata: params.metadata,
                relationship: params
                    .relationship
                    .map(|value| relationship_to_application(value.unwrap_or_default())),
                trust_grants,
                external_id: params.external_id,
                name: params.name,
            },
        )
        .await
        .map(project)
        .map(Json)
        .map_err(error_response)
}

async fn enrollment_url(
    State(state): State<Arc<UserProfileHttpState>>,
    Path(id): Path<String>,
) -> Result<Json<EnrollmentUrl>, WireError> {
    state
        .application
        .mint_enrollment(&state.org, &id)
        .await
        .map(|EnrollmentTicket { url, expires_at }| {
            Json(EnrollmentUrl {
                object_type: "enrollment_url",
                url,
                expires_at: awaken_session_contract::epoch_millis_to_rfc3339(
                    expires_at.max(0) as u64
                ),
            })
        })
        .map_err(error_response)
}
