//! `RouteModule` trait and per-module-state implementations.
//!
//! Each module state knows its own route surface; [`crate::routes::build_router`]
//! folds available modules together without per-module imperative if-chains.

use awaken_server_contract::RequestSurface;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use axum::{Router, middleware};

use crate::app::{
    AdminModuleState, AdminRunRoutesState, ConfigRoutesState, EvalRoutesState, EventModuleState,
    ProtocolRoutesState, RunRoutesState, SystemRoutesState, TraceRoutesState,
};
use crate::routes::ApiError;
use crate::scope::HttpScopeProvider;

#[derive(Clone)]
struct ScopeMiddlewareState {
    provider: std::sync::Arc<dyn HttpScopeProvider>,
    surface: RequestSurface,
}

async fn require_scope_middleware(
    State(state): State<ScopeMiddlewareState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();
    let scope = state
        .provider
        .scope_for_http_request(state.surface, &parts)
        .await
        .map_err(|error| ApiError::Unauthorized(error.to_string()))?;
    parts.extensions.insert(scope);
    Ok(next.run(Request::from_parts(parts, body)).await)
}

async fn require_admin_auth_middleware(
    State(admin): State<AdminModuleState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&admin, &headers)?;
    Ok(next.run(request).await)
}

/// A self-contained router fragment that knows how to mount itself onto a
/// parent `Router`. See module docs.
pub(crate) trait RouteModule {
    fn mount(self, router: Router) -> Router;
}

/// Lift `Option<M: RouteModule>` into a no-op when the module is absent so
/// `build_router` can chain optional modules without `if let`.
impl<M: RouteModule> RouteModule for Option<M> {
    fn mount(self, router: Router) -> Router {
        match self {
            Some(module) => module.mount(router),
            None => router,
        }
    }
}

impl RouteModule for RunRoutesState {
    fn mount(self, router: Router) -> Router {
        let scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.scope_provider.clone(),
                surface: RequestSurface::AgentInvoke,
            },
            require_scope_middleware,
        );
        router
            .merge(crate::routes::health_routes().with_state(self.clone()))
            .merge(
                crate::routes::thread_routes()
                    .route_layer(scope.clone())
                    .with_state(self.clone()),
            )
            .merge(
                crate::routes::run_routes()
                    .route_layer(scope)
                    .with_state(self.clone()),
            )
    }
}

impl RouteModule for ProtocolRoutesState {
    fn mount(self, router: Router) -> Router {
        let scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.scope_provider.clone(),
                surface: RequestSurface::AgentInvoke,
            },
            require_scope_middleware,
        );
        let admin_scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.scope_provider.clone(),
                surface: RequestSurface::Admin,
            },
            require_scope_middleware,
        );
        let auth =
            middleware::from_fn_with_state(self.admin.clone(), require_admin_auth_middleware);
        router
            .merge(
                crate::protocols::ai_sdk_v6::http::ai_sdk_routes()
                    .route_layer(scope.clone())
                    .with_state(self.clone()),
            )
            .merge(
                crate::protocols::ai_sdk_v6::http::ai_sdk_admin_routes()
                    .route_layer(admin_scope)
                    .route_layer(auth)
                    .with_state(self.clone()),
            )
            .merge(
                crate::protocols::ag_ui::http::ag_ui_routes()
                    .route_layer(scope.clone())
                    .with_state(self.clone()),
            )
            .merge(
                crate::protocols::a2a::a2a_routes()
                    .route_layer(scope.clone())
                    .with_state(self.clone()),
            )
            .merge(
                crate::protocols::mcp::http::mcp_routes()
                    .route_layer(scope)
                    .with_state(self),
            )
    }
}

/// Newtype wrapper pinning `RouteModule` to `SystemRoutesState`. The trait
/// would otherwise have nothing else to dispatch on for single-purpose
/// state types; the wrapper signals intent at the call site.
pub(crate) struct SystemRoutes(pub SystemRoutesState);

impl RouteModule for SystemRoutes {
    fn mount(self, router: Router) -> Router {
        let auth =
            middleware::from_fn_with_state(self.0.admin.clone(), require_admin_auth_middleware);
        let scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.0.scope_provider.clone(),
                surface: RequestSurface::Admin,
            },
            require_scope_middleware,
        );
        router.merge(
            crate::system_routes::system_routes()
                .route_layer(scope)
                .route_layer(auth)
                .with_state(self.0),
        )
    }
}

pub(crate) struct AdminRunModule(pub AdminRunRoutesState);

impl RouteModule for AdminRunModule {
    fn mount(self, router: Router) -> Router {
        let auth =
            middleware::from_fn_with_state(self.0.admin.clone(), require_admin_auth_middleware);
        let scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.0.scope_provider.clone(),
                surface: RequestSurface::Admin,
            },
            require_scope_middleware,
        );
        router
            .merge(
                crate::admin_routes::admin_run_routes()
                    .route_layer(scope.clone())
                    .route_layer(auth.clone())
                    .with_state(self.0.clone()),
            )
            .merge(
                crate::services::run_service::summary_routes()
                    .route_layer(scope)
                    .route_layer(auth)
                    .with_state(self.0.run),
            )
    }
}

impl RouteModule for ConfigRoutesState {
    fn mount(self, router: Router) -> Router {
        let auth =
            middleware::from_fn_with_state(self.admin.clone(), require_admin_auth_middleware);
        let scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.scope_provider.clone(),
                surface: RequestSurface::Admin,
            },
            require_scope_middleware,
        );
        router
            .merge(
                crate::config_routes::config_routes()
                    .route_layer(scope.clone())
                    .route_layer(auth.clone())
                    .with_state(self.clone()),
            )
            .merge(
                crate::admin_routes::config_admin_routes()
                    .route_layer(scope)
                    .route_layer(auth)
                    .with_state(self),
            )
    }
}

impl RouteModule for EvalRoutesState {
    fn mount(self, router: Router) -> Router {
        let auth =
            middleware::from_fn_with_state(self.admin.clone(), require_admin_auth_middleware);
        let scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.scope_provider.clone(),
                surface: RequestSurface::Admin,
            },
            require_scope_middleware,
        );
        router.merge(
            crate::eval_router::eval_routes()
                .route_layer(scope)
                .route_layer(auth)
                .with_state(self),
        )
    }
}

impl RouteModule for TraceRoutesState {
    fn mount(self, router: Router) -> Router {
        let auth =
            middleware::from_fn_with_state(self.admin.clone(), require_admin_auth_middleware);
        let scope = middleware::from_fn_with_state(
            ScopeMiddlewareState {
                provider: self.scope_provider.clone(),
                surface: RequestSurface::Admin,
            },
            require_scope_middleware,
        );
        router.merge(
            crate::routes::trace_routes()
                .route_layer(scope)
                .route_layer(auth)
                .with_state(self),
        )
    }
}

impl RouteModule for EventModuleState {
    fn mount(self, router: Router) -> Router {
        router.merge(crate::event_routes::event_routes().with_state(self))
    }
}
