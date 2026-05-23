//! `RouteModule` trait and per-module-state implementations.
//!
//! Each module state knows its own route surface; [`crate::routes::build_router`]
//! folds available modules together without per-module imperative if-chains.

use axum::Router;

use crate::app::{
    AdminRunRoutesState, ConfigRoutesState, EvalRoutesState, EventModuleState, ProtocolRoutesState,
    RunRoutesState, SystemRoutesState, TraceRoutesState,
};

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
        router
            .merge(crate::routes::health_routes().with_state(self.clone()))
            .merge(crate::routes::thread_routes().with_state(self.clone()))
            .merge(crate::routes::run_routes().with_state(self))
    }
}

impl RouteModule for ProtocolRoutesState {
    fn mount(self, router: Router) -> Router {
        router
            .merge(crate::protocols::ai_sdk_v6::http::ai_sdk_routes().with_state(self.clone()))
            .merge(crate::protocols::ag_ui::http::ag_ui_routes().with_state(self.clone()))
            .merge(crate::protocols::a2a::a2a_routes().with_state(self.clone()))
            .merge(crate::protocols::mcp::http::mcp_routes().with_state(self))
    }
}

/// Newtype wrapper pinning `RouteModule` to `SystemRoutesState`. The trait
/// would otherwise have nothing else to dispatch on for single-purpose
/// state types; the wrapper signals intent at the call site.
pub(crate) struct SystemRoutes(pub SystemRoutesState);

impl RouteModule for SystemRoutes {
    fn mount(self, router: Router) -> Router {
        router.merge(crate::system_routes::system_routes().with_state(self.0))
    }
}

pub(crate) struct AdminRunModule(pub AdminRunRoutesState);

impl RouteModule for AdminRunModule {
    fn mount(self, router: Router) -> Router {
        router.merge(crate::admin_routes::admin_run_routes().with_state(self.0))
    }
}

impl RouteModule for ConfigRoutesState {
    fn mount(self, router: Router) -> Router {
        router
            .merge(crate::config_routes::config_routes().with_state(self.clone()))
            .merge(crate::admin_routes::config_admin_routes().with_state(self))
    }
}

impl RouteModule for EvalRoutesState {
    fn mount(self, router: Router) -> Router {
        router.merge(crate::eval_router::eval_routes().with_state(self))
    }
}

impl RouteModule for TraceRoutesState {
    fn mount(self, router: Router) -> Router {
        router.merge(crate::routes::trace_routes().with_state(self))
    }
}

impl RouteModule for EventModuleState {
    fn mount(self, router: Router) -> Router {
        router.merge(crate::event_routes::event_routes().with_state(self))
    }
}
