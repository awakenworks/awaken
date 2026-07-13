//! The Managed **user-profiles** front door (`/v1/user_profiles`), the official
//! `@anthropic-ai/sdk` `beta.userProfiles.*` client's surface: create / retrieve /
//! update / list plus the `enrollment_url` action. A user profile represents the
//! entity behind an agent run (an end user, a resold company, or the platform
//! itself) and carries free-form metadata + trust grants.
//!
//! State is a neutral in-memory store (one process), mirroring [`crate::vaults`]:
//! a stable `uprof_…` id, per-parent nothing (profiles are flat), deterministic
//! ascending-id list order.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;

use crate::routes::ManagedJson;
use crate::types::user_profile::{
    EnrollmentUrl, Relationship, UserProfile, UserProfileCreateParams, UserProfileUpdateParams,
};
use crate::types::{ErrorResponse, Page, PageQuery, paginate};

/// Deterministic timestamps, matching the vault surface's convention.
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// The enrollment URL's fixed validity horizon (deterministic for tests).
const ENROLL_EXPIRES_AT: &str = "2026-12-31T23:59:59Z";

#[derive(Clone)]
struct Record {
    metadata: BTreeMap<String, String>,
    relationship: Relationship,
    external_id: Option<String>,
    name: Option<String>,
}

impl Record {
    fn project(&self, id: &str) -> UserProfile {
        UserProfile {
            id: id.to_string(),
            created_at: OBJECT_AT.to_string(),
            updated_at: OBJECT_AT.to_string(),
            metadata: self.metadata.clone(),
            relationship: self.relationship,
            trust_grants: BTreeMap::new(),
            object_type: "user_profile",
            external_id: self.external_id.clone(),
            name: self.name.clone(),
        }
    }
}

/// The user-profile surface's state.
#[derive(Default)]
pub struct UserProfileState {
    inner: Mutex<BTreeMap<String, Record>>,
    seq: AtomicU64,
}

impl UserProfileState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Mount the user-profile routes.
pub fn user_profiles_router(state: Arc<UserProfileState>) -> Router {
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
        .with_state(state)
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn not_found() -> WireError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "not_found_error",
            "user_profile not found",
        )),
    )
}

fn bad_request(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

fn check_len(field: &str, value: &Option<String>) -> Result<(), WireError> {
    if let Some(v) = value
        && v.len() > 255 {
            return Err(bad_request(format!(
                "{field} must be at most 255 characters"
            )));
        }
    Ok(())
}

async fn create_profile(
    State(state): State<Arc<UserProfileState>>,
    ManagedJson(params): ManagedJson<UserProfileCreateParams>,
) -> Result<Json<UserProfile>, WireError> {
    check_len("external_id", &params.external_id)?;
    check_len("name", &params.name)?;
    let n = state.seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("uprof_{n:016}");
    let record = Record {
        metadata: params.metadata,
        relationship: params.relationship,
        external_id: params.external_id,
        name: params.name,
    };
    let profile = record.project(&id);
    state.inner.lock().unwrap().insert(id, record);
    Ok(Json(profile))
}

async fn retrieve_profile(
    State(state): State<Arc<UserProfileState>>,
    Path(id): Path<String>,
) -> Result<Json<UserProfile>, WireError> {
    let store = state.inner.lock().unwrap();
    let record = store.get(&id).ok_or_else(not_found)?;
    Ok(Json(record.project(&id)))
}

/// `GET /v1/user_profiles` — one full page, ascending id order.
async fn list_profiles(
    State(state): State<Arc<UserProfileState>>,
    Query(page): Query<PageQuery>,
) -> Json<Page<UserProfile>> {
    let store = state.inner.lock().unwrap();
    // BTreeMap iterates in ascending-key order — deterministic + creation order
    // (`uprof_` is zero-padded).
    let data: Vec<UserProfile> = store.iter().map(|(id, r)| r.project(id)).collect();
    Json(paginate(data, &page, |p| p.id.as_str()))
}

async fn update_profile(
    State(state): State<Arc<UserProfileState>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<UserProfileUpdateParams>,
) -> Result<Json<UserProfile>, WireError> {
    check_len("external_id", &params.external_id)?;
    check_len("name", &params.name)?;
    let mut store = state.inner.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(not_found)?;
    if let Some(external_id) = params.external_id {
        record.external_id = Some(external_id);
    }
    if let Some(name) = params.name {
        record.name = Some(name);
    }
    if let Some(relationship) = params.relationship {
        record.relationship = relationship;
    }
    if let Some(patch) = params.metadata {
        // SDK convention: an empty-string value removes the key; else upsert.
        for (key, value) in patch {
            if value.is_empty() {
                record.metadata.remove(&key);
            } else {
                record.metadata.insert(key, value);
            }
        }
    }
    Ok(Json(record.project(&id)))
}

/// `POST /v1/user_profiles/:id/enrollment_url` — mint an enrollment URL for the
/// end user (`BetaUserProfileEnrollmentURL`). Deterministic on this surface: a
/// stable per-profile URL with a fixed validity horizon.
async fn enrollment_url(
    State(state): State<Arc<UserProfileState>>,
    Path(id): Path<String>,
) -> Result<Json<EnrollmentUrl>, WireError> {
    let store = state.inner.lock().unwrap();
    if !store.contains_key(&id) {
        return Err(not_found());
    }
    Ok(Json(EnrollmentUrl {
        object_type: "enrollment_url",
        url: format!("https://enroll.awaken.local/{id}"),
        expires_at: ENROLL_EXPIRES_AT,
    }))
}
