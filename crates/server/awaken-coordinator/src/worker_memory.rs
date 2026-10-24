// @formal surface exclude=composition_wiring
// @formal owner crates/server/awaken-session-application/src/resource_effect_authorization.rs
// @formal reason: Static owner: Coordinator assembly of the Worker Memory transport over existing ports.
// @formal reason: Dynamic path: the transport asks SessionResourceEffectAuthorization, which decides from
// @formal reason: the Session root; this wiring makes no authorization decision itself.
//! Existing Worker Memory HTTP composition over the selected authorities.

use std::sync::Arc;

/// Assemble the private Worker Memory boundary with the same selected content,
/// claim authority, identity checks, and Session-root Resource-effect fences.
pub(crate) fn memory_router(
    resources: &awaken_resource_application::ResourceAuthorities,
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    dispatch: Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
    authorization: Arc<awaken_session_application::SessionResourceEffectAuthorization>,
    authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    directory: Arc<dyn crate::WorkerDirectory>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> axum::Router {
    awaken_resource_worker_http::worker_memory_router(Arc::new(
        awaken_resource_worker_http::WorkerMemoryService::from_resources(
            resources,
            registry,
            dispatch,
            authenticator,
            directory,
            clock,
        )
        .with_materialization_control(authorization.clone())
        .with_terminal_memory_control(authorization),
    ))
}
