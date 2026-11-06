//! The Managed protocol's Org admission encoding: which Managed routes consume
//! which Org bucket, and the Anthropic wire shape of every admission outcome
//! (the `rate_limit_error` envelope, `retry-after` and the
//! `anthropic-ratelimit-requests-*` headers). The bucket decision itself is the
//! access edge's (`awaken_authz_enforce::admit_by_org_bucket`); composition
//! roots mount it with [`managed_org_admission_class`] and the encoders below.

use axum::Json;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use awaken_runtime_contract::runs::request::{
    OrgAdmissionDecision, OrgAdmissionUnavailable, OrgOperation,
};

use crate::types::ErrorResponse;

/// A metered Managed request without an edge-authored Workspace scope.
#[must_use]
pub fn managed_admission_scope_missing(_path: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::new(
            "api_error",
            "Managed request has no trusted Workspace scope",
        )),
    )
        .into_response()
}

/// The Org limiter could not decide a Managed request.
#[must_use]
pub fn managed_admission_unavailable(_path: &str, error: &OrgAdmissionUnavailable) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::new(
            "api_error",
            format!("Managed request limiter is unavailable: {}", error.message),
        )),
    )
        .into_response()
}

/// The Org bucket refused a Managed request.
#[must_use]
pub fn managed_admission_rejected(
    _path: &str,
    operation: OrgOperation,
    _decision: &OrgAdmissionDecision,
) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ErrorResponse::new(
            "rate_limit_error",
            format!("Managed Agents {operation:?} request limit exceeded"),
        )),
    )
        .into_response()
}

/// Publish the bucket state on every metered Managed response.
pub fn annotate_managed_rate_limit(headers: &mut HeaderMap, decision: &OrgAdmissionDecision) {
    apply_headers(headers, *decision);
}

/// The Org bucket and resource family of one Managed (or Managed-extension
/// `/v1/awaken`) request; Managed mutations that are neither Create nor Read
/// pass through unmetered.
#[must_use]
pub fn managed_org_admission_class(
    method: &Method,
    path: &str,
) -> Option<(OrgOperation, &'static str)> {
    classify(method, path)
}

fn apply_headers(headers: &mut HeaderMap, decision: OrgAdmissionDecision) {
    insert_header(
        headers,
        "anthropic-ratelimit-requests-limit",
        u64::from(decision.limit),
    );
    insert_header(
        headers,
        "anthropic-ratelimit-requests-remaining",
        u64::from(decision.remaining),
    );
    if let Ok(value) = HeaderValue::from_str(&awaken_time::utc::epoch_millis_to_rfc3339(
        decision.reset_at_unix_ms,
    )) {
        headers.insert("anthropic-ratelimit-requests-reset", value);
    }
    if let Some(retry_after) = decision.retry_after {
        insert_header(headers, "retry-after", retry_after);
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: u64) {
    if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
        headers.insert(name, value);
    }
}

fn classify(method: &Method, path: &str) -> Option<(OrgOperation, &'static str)> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let resource = managed_family(&segments)?;
    if matches!(*method, Method::GET | Method::HEAD) {
        return Some((OrgOperation::Read, resource));
    }
    (*method == Method::POST && is_create_endpoint(&segments))
        .then_some((OrgOperation::Create, resource))
}

fn managed_family(segments: &[&str]) -> Option<&'static str> {
    if segments.first() != Some(&"v1") {
        return None;
    }
    match segments.get(1).copied()? {
        "agents" => Some("agents"),
        "sessions" => Some("sessions"),
        "environments" => Some("environments"),
        "deployments" => Some("deployments"),
        "deployment_runs" => Some("deployment_runs"),
        "vaults" => Some("vaults"),
        "memory_stores" => Some("memory_stores"),
        "skills" => Some("skills"),
        "user_profiles" => Some("user_profiles"),
        "dreams" => Some("dreams"),
        "files" => Some("files"),
        "models" => Some("models"),
        "application-access-tokens" => Some("application_access_tokens"),
        "tunnels" => Some("tunnels"),
        "organizations" if segments.get(2) == Some(&"tunnels") => Some("tunnels"),
        // Awaken's Managed-protocol extensions share the Org buckets.
        "awaken" => Some(match segments.get(2).copied() {
            Some("sessions") => "awaken_sessions",
            Some("repositories") => "awaken_repositories",
            Some("environments") => "awaken_environments",
            Some("memory-stores") => "awaken_memory_stores",
            _ => "awaken",
        }),
        _ => None,
    }
}

fn is_create_endpoint(segments: &[&str]) -> bool {
    matches!(
        segments,
        ["v1", "agents"]
            | ["v1", "sessions"]
            | ["v1", "environments"]
            | ["v1", "deployments"]
            | ["v1", "vaults"]
            | ["v1", "memory_stores"]
            | ["v1", "skills"]
            | ["v1", "user_profiles"]
            | ["v1", "dreams"]
            | ["v1", "sessions", _, "resources"]
            | ["v1", "vaults", _, "credentials"]
            | ["v1", "memory_stores", _, "memories"]
            | ["v1", "skills", _, "versions"]
            | ["v1", "user_profiles", _, "enrollment_url"]
            | ["v1", "files"]
            | ["v1", "application-access-tokens"]
            | ["v1", "tunnels"]
            | ["v1", "tunnels", _, "certificates"]
            | ["v1", "organizations", "tunnels"]
            | ["v1", "organizations", "tunnels", _, "certificates"]
            | ["v1", "awaken", "sessions"]
            | ["v1", "awaken", "repositories"]
            | ["v1", "awaken", "environments"]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_classification_is_generated_from_the_operation_decision_table() {
        // Cause/effect graph:
        // C1 known Managed family; C2 GET/HEAD; C3 POST names an API operation
        // whose documented verb is Create. C1&C2 -> read bucket; C1&C3 -> create
        // bucket; every other combination -> no Managed request bucket.
        //
        // | Rule | family | method/operation | effect |
        // |---|---|---|---|
        // | C1 | Managed | GET/HEAD retrieve/list/stream | Read |
        // | C2 | Managed | POST exact collection/nested Create | Create |
        // | C3 | Managed | update/archive/action/delete | pass through |
        // | C4 | unknown extension or non-Managed | any | pass through |
        for path in [
            "/v1/agents",
            "/v1/sessions",
            "/v1/environments",
            "/v1/deployments",
            "/v1/vaults",
            "/v1/memory_stores",
            "/v1/skills",
            "/v1/user_profiles",
            "/v1/dreams",
            "/v1/sessions/ses_1/resources",
            "/v1/vaults/vlt_1/credentials",
            "/v1/memory_stores/mem_1/memories",
            "/v1/skills/sk_1/versions",
            "/v1/user_profiles/usr_1/enrollment_url",
            "/v1/files",
            "/v1/application-access-tokens",
            "/v1/tunnels",
            "/v1/tunnels/tnl_1/certificates",
        ] {
            assert_eq!(
                classify(&Method::POST, path),
                Some((
                    OrgOperation::Create,
                    if path == "/v1/application-access-tokens" {
                        "application_access_tokens"
                    } else {
                        path.trim_start_matches("/v1/").split('/').next().unwrap()
                    }
                )),
                "C2 {path}"
            );
        }
        for path in [
            "/v1/organizations/tunnels",
            "/v1/organizations/tunnels/tnl_1/certificates",
        ] {
            assert_eq!(
                classify(&Method::POST, path),
                Some((OrgOperation::Create, "tunnels")),
                "legacy Tunnel Create shares the canonical Tunnel bucket: {path}"
            );
        }
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                classify(&method, "/v1/organizations/tunnels/tnl_1"),
                Some((OrgOperation::Read, "tunnels")),
                "legacy Tunnel reads share the canonical Tunnel bucket"
            );
        }
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                classify(&method, "/v1/sessions/ses_1/events/stream"),
                Some((OrgOperation::Read, "sessions")),
                "C1"
            );
        }
        for path in [
            "/v1/agents/a_1/archive",
            "/v1/sessions/s_1/events",
            "/v1/deployments/d_1/run",
            "/v1/vaults/v_1",
            "/v1/application-access-tokens/aat_1",
        ] {
            assert_eq!(classify(&Method::POST, path), None, "C3 {path}");
        }
        for path in ["/v1/extensions", "/healthz"] {
            assert_eq!(classify(&Method::GET, path), None, "C4 {path}");
        }
        assert_eq!(
            classify(&Method::GET, "/v1/models"),
            Some((OrgOperation::Read, "models")),
            "C1 models"
        );
    }

    #[test]
    fn awaken_extension_routes_share_the_org_buckets() {
        // Managed-protocol extensions under /v1/awaken are Managed entries:
        // | Rule | method | route | effect |
        // | X1 | GET/HEAD | any /v1/awaken route | Read |
        // | X2 | POST | exact profiled-Session, Repository, Environment collection | Create |
        // | X3 | POST/PUT | Run submit/resume, release, resource manifest | pass through |
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                classify(&method, "/v1/awaken/sessions/ses_1/execution-evidence"),
                Some((OrgOperation::Read, "awaken_sessions")),
                "X1 existing execution evidence",
            );
            assert_eq!(
                classify(&method, "/v1/awaken/sessions/ses_1/runs/run_1/dispatch"),
                Some((OrgOperation::Read, "awaken_sessions")),
                "X1"
            );
            assert_eq!(
                classify(&method, "/v1/awaken/memory-stores/mem_1/copy-conflicts"),
                Some((OrgOperation::Read, "awaken_memory_stores")),
                "X1"
            );
        }
        for (path, family) in [
            ("/v1/awaken/sessions", "awaken_sessions"),
            ("/v1/awaken/repositories", "awaken_repositories"),
            ("/v1/awaken/environments", "awaken_environments"),
        ] {
            assert_eq!(
                classify(&Method::POST, path),
                Some((OrgOperation::Create, family)),
                "X2 {path}"
            );
        }
        for path in [
            "/v1/awaken/sessions/ses_1/runs",
            "/v1/awaken/sessions/ses_1/runs/run_1/resume",
            "/v1/awaken/sessions/ses_1/release",
        ] {
            assert_eq!(classify(&Method::POST, path), None, "X3 {path}");
        }
        assert_eq!(
            classify(&Method::PUT, "/v1/awaken/sessions/ses_1/resources"),
            None,
            "X3"
        );
    }
}
