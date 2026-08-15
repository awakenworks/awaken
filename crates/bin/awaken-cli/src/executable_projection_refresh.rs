//! One active-active refresh boundary for Coordinator executable projections.

use std::sync::Arc;

use awaken_executable_agent_catalog::PostgresExecutableAgentRegistrar;
use awaken_executable_environment_catalog::PostgresExecutableEnvironmentRegistrar;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

#[async_trait::async_trait]
trait ExecutableProjectionRefresher: Send + Sync {
    async fn refresh(&self) -> Result<(), String>;
}

#[async_trait::async_trait]
impl ExecutableProjectionRefresher for PostgresExecutableAgentRegistrar {
    async fn refresh(&self) -> Result<(), String> {
        self.refresh_projection()
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl ExecutableProjectionRefresher for PostgresExecutableEnvironmentRegistrar {
    async fn refresh(&self) -> Result<(), String> {
        self.refresh_projection()
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct ExecutableProjectionRefreshers {
    agents: Option<Arc<dyn ExecutableProjectionRefresher>>,
    environments: Option<Arc<dyn ExecutableProjectionRefresher>>,
}

/// Install one active-active reconciliation middleware for both durable
/// executable projections. AllInOne has no cross-replica projection and skips
/// the layer entirely.
pub(crate) fn layer(
    router: Router,
    agents: Option<Arc<PostgresExecutableAgentRegistrar>>,
    environments: Option<Arc<PostgresExecutableEnvironmentRegistrar>>,
) -> Router {
    layer_with_refreshers(
        router,
        agents.map(|value| value as Arc<_>),
        environments.map(|value| value as Arc<_>),
    )
}

fn layer_with_refreshers(
    router: Router,
    agents: Option<Arc<dyn ExecutableProjectionRefresher>>,
    environments: Option<Arc<dyn ExecutableProjectionRefresher>>,
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

/// Advance both projections to their durable high-water marks before a request
/// can admit or resume Runtime work.
/// A failure in either projection fails the request closed with no partial
/// Session admission; read-only operations stay off this database read path.
async fn refresh_before_runtime_use(
    State(refreshers): State<ExecutableProjectionRefreshers>,
    request: Request,
    next: Next,
) -> Response {
    if requires_projection_refresh(request.method(), request.uri().path()) {
        if let Some(agents) = refreshers.agents
            && let Err(error) = agents.refresh().await
        {
            return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
        }
        if let Some(environments) = refreshers.environments
            && let Err(error) = environments.refresh().await
        {
            return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::post;
    use tower::ServiceExt as _;

    struct RecordingRefresher {
        calls: Arc<AtomicUsize>,
        result: Result<(), String>,
    }

    #[async_trait::async_trait]
    impl ExecutableProjectionRefresher for RecordingRefresher {
        async fn refresh(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn recorder(
        result: Result<(), &str>,
    ) -> (Arc<AtomicUsize>, Arc<dyn ExecutableProjectionRefresher>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            calls.clone(),
            Arc::new(RecordingRefresher {
                calls,
                result: result.map_err(str::to_string),
            }),
        )
    }

    #[test]
    fn refresh_scope_covers_every_runtime_write_and_excludes_reads() {
        // Causes: C1 Session create, C2 existing-Session POST that can realize or
        // resume work, C3 Deployment create/run, C4 read-only Session request,
        // C5 live-inbox edit of an already-active attempt, C6 unrelated POST.
        // Effects: E1 refresh both installed executable projections before
        // continuing; E2 perform no projection database read. Decision rules
        // R1-R3 map C1-C3 to E1; R4-R6 map C4-C6 to E2.
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
            !requires_projection_refresh(&Method::POST, "/v1/awaken/sessions/sesn_1/live-inbox"),
            "R5"
        );
        assert!(
            !requires_projection_refresh(&Method::POST, "/v1/agents"),
            "R6"
        );
    }

    #[tokio::test]
    async fn middleware_refreshes_both_projections_before_runtime_admission() {
        // Cause/effect graph: C1=request is a Runtime-admitting write;
        // C2=Agent refresh succeeds; C3=Environment refresh succeeds. Effects:
        // E1=each required projection refreshes in order, E2=handler executes,
        // E3=503 and handler does not execute. Constraints: Environment refresh
        // occurs only after Agent succeeds; handler occurs only after both succeed.
        //
        // | Rule | C1 | C2 | C3 | Agent calls | Env calls | Handler | Status |
        // | M1 | T | T | T | 1 | 1 | 1 | 200 |
        // | M2 | T | F | - | 1 | 0 | 0 | 503 |
        // | M3 | T | T | F | 1 | 1 | 0 | 503 |
        // | M4 | F | - | - | 0 | 0 | 1 | 200 |
        async fn exercise(
            method: Method,
            agent_result: Result<(), &str>,
            environment_result: Result<(), &str>,
        ) -> (StatusCode, usize, usize, usize) {
            let (agent_calls, agents) = recorder(agent_result);
            let (environment_calls, environments) = recorder(environment_result);
            let handler_calls = Arc::new(AtomicUsize::new(0));
            let post_calls = handler_calls.clone();
            let get_calls = handler_calls.clone();
            let router = Router::new().route(
                "/v1/sessions",
                post(move || {
                    let calls = post_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::OK
                    }
                })
                .get(move || {
                    let calls = get_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::OK
                    }
                }),
            );
            let response = layer_with_refreshers(router, Some(agents), Some(environments))
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri("/v1/sessions")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            (
                response.status(),
                agent_calls.load(Ordering::SeqCst),
                environment_calls.load(Ordering::SeqCst),
                handler_calls.load(Ordering::SeqCst),
            )
        }

        assert_eq!(
            exercise(Method::POST, Ok(()), Ok(())).await,
            (StatusCode::OK, 1, 1, 1),
            "M1"
        );
        assert_eq!(
            exercise(Method::POST, Err("agent offline"), Ok(())).await,
            (StatusCode::SERVICE_UNAVAILABLE, 1, 0, 0),
            "M2"
        );
        assert_eq!(
            exercise(Method::POST, Ok(()), Err("environment offline")).await,
            (StatusCode::SERVICE_UNAVAILABLE, 1, 1, 0),
            "M3"
        );
        assert_eq!(
            exercise(Method::GET, Err("unused"), Err("unused")).await,
            (StatusCode::OK, 0, 0, 1),
            "M4"
        );
    }
}
