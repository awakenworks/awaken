//! Per-Work capability edge for the official Managed Environment Worker.
//!
//! The current WorkQueue lease remains the single authority. This module only
//! maps that lease to the exact Managed HTTP resources the leased Session needs;
//! it owns no token ledger, resource catalog, or Memory synchronization loop.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_environment_execution_application::EnvironmentExecutionApplication;
use awaken_session_contract::work_queue::WorkSessionAccess;
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
pub struct ManagedWorkSessionAccess {
    environments: Arc<EnvironmentExecutionApplication>,
    sessions: Arc<ManagedState>,
}

impl ManagedWorkSessionAccess {
    #[must_use]
    pub fn new(
        environments: Arc<EnvironmentExecutionApplication>,
        sessions: Arc<ManagedState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            environments,
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
    if !presented.starts_with("sk-ant-req-") {
        return next.run(req).await;
    }
    let token = RedactedString::from(presented.to_owned());
    let access = match state
        .environments
        .authenticate_session_access(&token, now_ms())
        .await
    {
        Ok(Some(access)) => access,
        Ok(None) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid or expired Work sessions token",
            );
        }
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "Work session authorization is unavailable",
            );
        }
    };
    let scope = match state.sessions.work_session_scope(&access.session_id).await {
        Ok(scope) => scope,
        Err(_) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "Work Session is unavailable",
            );
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
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
            lease_epoch: 2,
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
}
