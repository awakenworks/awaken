//! One active-active refresh boundary for Coordinator executable projections.

use std::sync::Arc;

use awaken_executable_agent_catalog::PostgresExecutableAgentRegistrar;
use awaken_executable_environment_catalog::PostgresExecutableEnvironmentRegistrar;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

#[derive(Clone)]
struct ExecutableProjectionRefreshers {
    agents: Option<Arc<PostgresExecutableAgentRegistrar>>,
    environments: Option<Arc<PostgresExecutableEnvironmentRegistrar>>,
}

/// Install one active-active reconciliation middleware for both durable
/// executable projections. AllInOne has no cross-replica projection and skips
/// the layer entirely.
pub(crate) fn layer(
    router: Router,
    agents: Option<Arc<PostgresExecutableAgentRegistrar>>,
    environments: Option<Arc<PostgresExecutableEnvironmentRegistrar>>,
) -> Router {
    if agents.is_none() && environments.is_none() {
        return router;
    }
    router.layer(axum::middleware::from_fn_with_state(
        ExecutableProjectionRefreshers {
            agents,
            environments,
        },
        refresh_before_runtime_use,
    ))
}

fn requires_projection_refresh(method: &Method, path: &str) -> bool {
    method == Method::POST
        && (path.ends_with("/v1/sessions")
            || path.contains("/v1/sessions/")
            || path.ends_with("/v1/deployments")
            || path.contains("/v1/deployments/"))
}

/// Rebuild both projections before a request can admit or resume Runtime work.
/// A failure in either projection fails the request closed with no partial
/// Session admission; read-only operations stay off this database read path.
async fn refresh_before_runtime_use(
    State(refreshers): State<ExecutableProjectionRefreshers>,
    request: Request,
    next: Next,
) -> Response {
    if requires_projection_refresh(request.method(), request.uri().path()) {
        if let Some(agents) = refreshers.agents
            && let Err(error) = agents.refresh_projection().await
        {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
        if let Some(environments) = refreshers.environments
            && let Err(error) = environments.refresh_projection().await
        {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_scope_covers_every_runtime_write_and_excludes_reads() {
        // Causes: C1 Session create, C2 existing-Session POST that can realize or
        // resume work, C3 Deployment create/run, C4 read-only Session request,
        // C5 unrelated POST. Effects: E1 refresh both installed executable
        // projections before continuing; E2 perform no projection database read.
        // Decision rules R1-R3 map C1-C3 to E1; R4-R5 map C4-C5 to E2.
        assert!(
            requires_projection_refresh(&Method::POST, "/v1/sessions"),
            "R1"
        );
        assert!(
            requires_projection_refresh(&Method::POST, "/v1/sessions/sesn_1/events"),
            "R2"
        );
        assert!(
            requires_projection_refresh(&Method::POST, "/v1/deployments/depl_1/run"),
            "R3"
        );
        assert!(
            !requires_projection_refresh(&Method::GET, "/v1/sessions/sesn_1"),
            "R4"
        );
        assert!(
            !requires_projection_refresh(&Method::POST, "/v1/agents"),
            "R5"
        );
    }
}
