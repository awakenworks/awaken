// @formal surface obligation=authz.method_mapping_total
// @formal surface obligation=authz.decision_collapse_fail_closed
// @formal surface obligation=work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal reason: Authenticates the current Work lease and maps it to an exact Session, Work, frozen
// @formal reason: Skill, and attached Memory route capability; it introduces no token ledger or resource
// @formal reason: authority.
//! Per-Work capability edge for the official Managed Environment Worker.
//!
//! The current WorkQueue lease remains the single authority. This module only
//! maps that lease to the exact Managed HTTP resources the leased Session needs;
//! it owns no token ledger, resource catalog, or Memory synchronization loop.

use awaken_time::WallClockExt as _;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_environment_application::execution::SessionAccessAuthenticator;
use awaken_executable_environment_contract::WorkSessionAccess;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::ManagedState;
use crate::state::work_session_access::ManagedWorkSessionScope;
use crate::types::ErrorResponse;
use crate::types::resource::ResourceAccess;

/// Dependencies for the one Work-session capability guard.
/// Only current-lease authentication is available here; claim, metadata and
/// registration capabilities stay in their own callers.
///
/// ```
/// use std::sync::Arc;
/// use awaken_environment_application::execution::SessionAccessAuthenticator;
/// fn guard(
///     authentication: SessionAccessAuthenticator,
///     sessions: Arc<awaken_protocol_managed::ManagedState>,
/// ) -> Arc<awaken_protocol_managed::ManagedWorkSessionAccess> {
///     awaken_protocol_managed::ManagedWorkSessionAccess::new(authentication, sessions)
/// }
/// ```
pub struct ManagedWorkSessionAccess {
    authentication: SessionAccessAuthenticator,
    sessions: Arc<ManagedState>,
}

impl ManagedWorkSessionAccess {
    #[must_use]
    pub fn new(
        authentication: SessionAccessAuthenticator,
        sessions: Arc<ManagedState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            authentication,
            sessions,
        })
    }
}

fn bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().split_once(' '))
        .filter(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty())
        .map(|(_, token)| token.trim())
}

fn error(status: StatusCode, kind: &'static str, message: &'static str) -> Response {
    (status, Json(ErrorResponse::new(kind, message))).into_response()
}

fn segments(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').collect()
}

fn permits(
    method: &Method,
    path: &str,
    access: &WorkSessionAccess,
    scope: &ManagedWorkSessionScope,
) -> bool {
    let parts = segments(path);
    match parts.as_slice() {
        ["v1", "environments", environment, "work", work]
            if environment == &access.environment_id && work == &access.work_id =>
        {
            method == Method::GET
        }
        ["v1", "environments", environment, "work", work, operation]
            if environment == &access.environment_id
                && work == &access.work_id
                && matches!(*operation, "ack" | "heartbeat" | "stop") =>
        {
            method == Method::POST
        }
        ["v1", "sessions", session] if session == &access.session_id => method == Method::GET,
        ["v1", "sessions", session, "events"] if session == &access.session_id => {
            method == Method::GET || method == Method::POST
        }
        ["v1", "sessions", session, "events", "stream"] if session == &access.session_id => {
            method == Method::GET
        }
        ["v1", "skills", skill] => {
            method == Method::GET && scope.skill_versions.contains_key(*skill)
        }
        ["v1", "skills", skill, "versions"] => {
            method == Method::GET && scope.skill_versions.contains_key(*skill)
        }
        ["v1", "skills", skill, "versions", version]
        | ["v1", "skills", skill, "versions", version, "content"]
            if method == Method::GET =>
        {
            scope
                .skill_versions
                .get(*skill)
                .is_some_and(|frozen| frozen.as_deref().is_none_or(|v| v == *version))
        }
        ["v1", "skills", skill, "versions", version, "files", ..] if method == Method::GET => scope
            .skill_versions
            .get(*skill)
            .is_some_and(|frozen| frozen.as_deref().is_none_or(|v| v == *version)),
        ["v1", "memory_stores", store] => {
            method == Method::GET && scope.memory_stores.contains_key(*store)
        }
        ["v1", "memory_stores", store, "memories"] => scope
            .memory_stores
            .get(*store)
            .is_some_and(|access| method == Method::GET || *access == ResourceAccess::ReadWrite),
        ["v1", "memory_stores", store, "memories", _] => scope
            .memory_stores
            .get(*store)
            .is_some_and(|access| method == Method::GET || *access == ResourceAccess::ReadWrite),
        _ => false,
    }
}

/// Authenticate a `sessions_token` and publish its exact Work/Session scope.
/// Other credentials fall through unchanged to the existing management IAM.
pub async fn work_session_guard(
    State(state): State<Arc<ManagedWorkSessionAccess>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(presented) = bearer(&req) else {
        return next.run(req).await;
    };
    if !presented.starts_with(awaken_environment_application::execution::SESSION_TOKEN_PREFIX) {
        return next.run(req).await;
    }
    let token = RedactedString::from(presented.to_owned());
    let access = match state
        .authentication
        // The token's expiry is judged on the Session application's clock.
        .authenticate(&token, state.sessions.roles.clock.now_ms())
        .await
    {
        Ok(Some(access)) => access,
        Ok(None) => {
            return crate::with_unscoped_managed_response_context(error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid or expired Work sessions token",
            ));
        }
        Err(_) => {
            return crate::with_unscoped_managed_response_context(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "Work session authorization is unavailable",
            ));
        }
    };
    let scope = match state
        .sessions
        .work_session_scope(access.session_id.as_str())
        .await
    {
        Ok(scope) => scope,
        Err(crate::state::StateError::NotFound) => {
            return crate::with_unscoped_managed_response_context(error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Work Session is unavailable",
            ));
        }
        Err(_) => {
            return crate::with_unscoped_managed_response_context(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "Work Session authorization is unavailable",
            ));
        }
    };
    if !permits(req.method(), req.uri().path(), &access, &scope) {
        return crate::with_managed_workspace_header(
            error(
                StatusCode::FORBIDDEN,
                "permission_error",
                "Work sessions token does not authorize this resource",
            ),
            &scope.workspace_id,
        );
    }
    req.extensions_mut()
        .insert(awaken_tenancy::WorkspaceScope(scope.workspace_id));
    req.extensions_mut().insert(access);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn access() -> WorkSessionAccess {
        WorkSessionAccess {
            work_id: "work_1".into(),
            environment_id: "env_1".into(),
            session_id: "sesn_1".into(),
            lease_owner: "owner".into(),
            lease_epoch: awaken_lease_contract::fence::Epoch::from_persisted(2).unwrap(),
            expires_at_unix_ms: 100,
        }
    }

    fn scope() -> ManagedWorkSessionScope {
        ManagedWorkSessionScope {
            workspace_id: "ws_1".into(),
            skill_versions: BTreeMap::from([("skill_1".into(), Some("3".into()))]),
            memory_stores: BTreeMap::from([
                ("mem_rw".into(), ResourceAccess::ReadWrite),
                ("mem_ro".into(), ResourceAccess::ReadOnly),
            ]),
        }
    }

    #[test]
    fn work_session_capability_is_exact_and_read_only_memory_cannot_write() {
        // Cause/effect graph: C1 path addresses the leased Work or Session;
        // C2 Skill is frozen at an exact version; C3 Memory is attached RW/RO;
        // C4 path addresses another resource. Effects: E1 exact Work/Session
        // operations pass; E2 only the frozen Skill version passes; E3 RW pulls
        // and pushes while RO only pulls; E4 unrelated resources fail closed.
        // Constraints: K1 this classifier consumes a current WorkQueue proof and
        // owns no token/resource state; K2 native Memory mounting is unchanged.
        // Decision rows: R1=C1->E1, R2=C2->E2, R3=C3(RW)->read+write,
        // R4=C3(RO)->read only, R5=C4->E4.
        let access = access();
        let scope = scope();
        for (method, path) in [
            (Method::GET, "/v1/environments/env_1/work/work_1"),
            (Method::POST, "/v1/environments/env_1/work/work_1/heartbeat"),
            (Method::GET, "/v1/sessions/sesn_1"),
            (Method::POST, "/v1/sessions/sesn_1/events"),
            (Method::GET, "/v1/skills/skill_1/versions/3/content"),
            (Method::GET, "/v1/memory_stores/mem_ro/memories"),
            (Method::POST, "/v1/memory_stores/mem_rw/memories"),
        ] {
            assert!(permits(&method, path, &access, &scope), "{method} {path}");
        }
        for (method, path) in [
            (Method::GET, "/v1/sessions/sesn_other"),
            (Method::GET, "/v1/skills/skill_1/versions/4/content"),
            (Method::GET, "/v1/skills/skill_other"),
            (Method::POST, "/v1/memory_stores/mem_ro/memories"),
            (Method::GET, "/v1/memory_stores/mem_other/memories"),
            (Method::POST, "/v1/agents"),
        ] {
            assert!(!permits(&method, path, &access, &scope), "{method} {path}");
        }
    }

    async fn bounded_guard_response(
        app: axum::Router,
        request: axum::extract::Request,
    ) -> Response {
        use tower::ServiceExt as _;
        tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(request))
            .await
            .expect("guard response deadline")
            .unwrap()
    }

    /// Only the lease-authentication SPI is implemented. Constructing the guard
    /// must not require claim, registration or owner-only mutation capabilities.
    #[derive(Default)]
    struct AuthenticationOnly {
        attempts: std::sync::atomic::AtomicUsize,
        times: std::sync::Mutex<Vec<u64>>,
        unavailable: std::sync::atomic::AtomicBool,
        current_lease: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl awaken_executable_environment_contract::SessionAccessAuthenticator for AuthenticationOnly {
        async fn authenticate_session_access(
            &self,
            _presented: &awaken_executable_environment_contract::SessionAccessDigest,
            now_ms: u64,
        ) -> Result<Option<WorkSessionAccess>, awaken_executable_environment_contract::WorkQueueError>
        {
            use std::sync::atomic::Ordering;
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.times.lock().unwrap().push(now_ms);
            if self.unavailable.load(Ordering::SeqCst) {
                Err(
                    awaken_executable_environment_contract::WorkQueueError::Storage(
                        "private lease backend diagnostic".into(),
                    ),
                )
            } else if self.current_lease.load(Ordering::SeqCst) {
                let mut lease = access();
                lease.expires_at_unix_ms = now_ms.checked_add(60_000).unwrap();
                Ok(Some(lease))
            } else {
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn authentication_only_guard_preserves_management_fallthrough_and_fail_closed_errors() {
        use http_body_util::BodyExt as _;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // R1 missing/management credentials -> management ingress, no lease read.
        // R2 invalid Session bearer -> 401, no management fallback.
        // R3 unavailable authority -> 503, no management fallback or error leak.
        // The same selected authentication role handles both bearer requests.
        let selected = Arc::new(AuthenticationOnly::default());
        let sessions =
            Arc::new(ManagedState::new(crate::state::test_support::RehydrateFake::default()).await);
        let guard = ManagedWorkSessionAccess::new(
            SessionAccessAuthenticator::new(selected.clone()),
            sessions,
        );
        let management_calls = Arc::new(AtomicUsize::new(0));
        let calls = management_calls.clone();
        let app = axum::Router::new()
            .route(
                "/v1/sessions/sesn_1",
                axum::routing::get(move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                guard,
                work_session_guard,
            ));
        for credential in [None, Some("Bearer platform-management")] {
            let mut request = axum::http::Request::builder().uri("/v1/sessions/sesn_1");
            if let Some(credential) = credential {
                request = request.header(axum::http::header::AUTHORIZATION, credential);
            }
            let response = bounded_guard_response(
                app.clone(),
                request.body(axum::body::Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "R1");
        }
        assert_eq!(management_calls.load(Ordering::SeqCst), 2, "R1");
        assert_eq!(selected.attempts.load(Ordering::SeqCst), 0, "R1");
        for (unavailable, expected, kind, attempts) in [
            (false, StatusCode::UNAUTHORIZED, "authentication_error", 1),
            (true, StatusCode::SERVICE_UNAVAILABLE, "api_error", 2),
        ] {
            selected.unavailable.store(unavailable, Ordering::SeqCst);
            let response = bounded_guard_response(
                app.clone(),
                axum::http::Request::builder()
                    .uri("/v1/sessions/sesn_1")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        "Bearer sk-ant-req-invalid",
                    )
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), expected, "R2/R3");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], kind);
            assert!(
                !body
                    .to_string()
                    .contains("private lease backend diagnostic")
            );
            assert!(!body.to_string().contains("sk-ant-req-invalid"));
            assert_eq!(selected.attempts.load(Ordering::SeqCst), attempts);
            assert_eq!(management_calls.load(Ordering::SeqCst), 2, "no fallback");
        }
    }

    #[tokio::test]
    async fn session_scope_read_fault_is_unavailable_and_never_an_invalid_credential() {
        use crate::state::test_support::{
            ConflictInjectingRepo, ReadControl, RehydrateFake, managed_state_from_parts,
        };
        use http_body_util::BodyExt as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = ReadControl::default();
        let repository = Arc::new(ConflictInjectingRepo::new(
            Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            ),
            0,
            reads.clone(),
        ));
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000));
        let sessions = Arc::new(managed_state_from_parts(
            Arc::new(RehydrateFake::default()),
            None,
            repository,
            awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
            clock,
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        ));
        let selected = Arc::new(AuthenticationOnly::default());
        selected.current_lease.store(true, Ordering::SeqCst);
        let guard = ManagedWorkSessionAccess::new(
            SessionAccessAuthenticator::new(selected.clone()),
            sessions,
        );
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let calls = handler_calls.clone();
        let app = axum::Router::new()
            .route(
                "/v1/sessions/sesn_1",
                axum::routing::get(move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                guard,
                work_session_guard,
            ));
        // A current lease is a known fact from the selected authentication SPI.
        // S1 absent Session ->401. S2 its real SQLite authority cannot be read
        // ->503, never reclassify a verified credential as invalid. S3 the fault
        // clears ->the same lease still sees true absence401, no new authority.
        for (unavailable, status, kind, attempts) in [
            (false, StatusCode::UNAUTHORIZED, "authentication_error", 1),
            (true, StatusCode::SERVICE_UNAVAILABLE, "api_error", 2),
            (false, StatusCode::UNAUTHORIZED, "authentication_error", 3),
        ] {
            reads.fail("session", unavailable);
            reads.fail("owner", unavailable);
            let previous_reads = reads.trace().len();
            let response = bounded_guard_response(
                app.clone(),
                axum::http::Request::builder()
                    .uri("/v1/sessions/sesn_1")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        "Bearer sk-ant-req-current",
                    )
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(selected.attempts.load(Ordering::SeqCst), attempts);
            assert_eq!(
                selected.times.lock().unwrap().as_slice(),
                vec![1_000; attempts]
            );
            assert_eq!(
                handler_calls.load(Ordering::SeqCst),
                0,
                "all scope failures deny before dispatch"
            );
            if unavailable {
                assert!(
                    reads.trace().len() > previous_reads,
                    "the actual Session read fault must be reached"
                );
            }
            assert_eq!(
                response.status(),
                status,
                "Session absence and authority unavailability are distinct"
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], kind);
            assert!(!body.to_string().contains("scripted"));
            assert!(!body.to_string().contains("sk-ant-req-current"));
        }
    }
}
