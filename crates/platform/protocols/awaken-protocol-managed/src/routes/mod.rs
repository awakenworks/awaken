//! The routing concern: the axum routers and handlers for every Managed Agents
//! surface. Handlers only decode DTOs (from `types`), call the backing state, and
//! encode responses — no runtime or protocol logic.
//!
//! One submodule per surface. Each handler decodes wire DTOs and delegates to the
//! injected application owner; protocol modules do not own business stores:
//! - [`sessions`] — the core session/events/threads/resources surface, plus the
//!   shared HTTP plumbing ([`ManagedJson`], [`error_response`]) the others reuse.
//! - [`agents_registry`], [`deployments`], [`environments`], [`user_profiles`],
//!   [`vaults`] — the management-plane resource CRUD surfaces.

use crate::types::ErrorResponse;
use axum::{
    Json,
    http::{HeaderMap, StatusCode, header},
};
use sha2::{Digest, Sha256};

pub(crate) mod agents_registry;
pub(crate) mod deployments;
pub(crate) mod dreams;
pub(crate) mod environments;
mod extractors;
pub(crate) mod sessions;
pub(crate) mod tunnels;
pub(crate) mod user_profiles;
pub(crate) mod vaults;

// The session surface defines the shared error-envelope conventions; re-export the
// plumbing so sibling resource routers answer bad bodies and domain errors alike.
pub use dreams::dreams_router;
pub(crate) use extractors::{ManagedJson, ManagedMultipart, ManagedQuery};
pub use sessions::{
    close_event_admission, create_profiled_session, enforce_managed_beta,
    get_profiled_session_run_events, get_resource_manifest, get_session_content_capture,
    get_session_execution_evidence, list_thread_messages, release_profiled_session,
    replace_resource_manifest, resume_profiled_session_run, router, run_evidence_at,
    session_timeline, submit_profiled_session_run,
};
pub use tunnels::tunnels_router;

/// Domain-separated opaque identity used when a wire secret must select a
/// durable owner without persisting or logging the secret itself.
fn sha256_identity(domain: &str, parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.len().to_be_bytes());
    digest.update(domain.as_bytes());
    for part in parts {
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn parse_if_match(
    headers: &HeaderMap,
    representation: &str,
) -> Result<Option<u64>, (StatusCode, Json<ErrorResponse>)> {
    let invalid_revision = |message| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", message)),
        )
    };
    headers
        .get(header::IF_MATCH)
        .map(|value| {
            let raw = value.to_str().map_err(|_| {
                invalid_revision(format!(
                    "If-Match must be a quoted {representation} revision"
                ))
            })?;
            if raw == "*" {
                return Ok(None);
            }
            let revision = raw
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    invalid_revision(format!(
                        "If-Match must be `*` or a quoted {representation} revision"
                    ))
                })?;
            Ok(Some(revision))
        })
        .transpose()
        .map(Option::flatten)
}
