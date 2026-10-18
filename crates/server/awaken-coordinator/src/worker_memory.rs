//! Existing Worker Memory HTTP composition over the selected authorities.

use std::sync::Arc;

/// Assemble the private Worker Memory boundary with the same selected content,
/// claim authority, identity checks, and Session-owned materialization fences.
pub(crate) fn memory_router(
    resources: &awaken_resource_application::ResourceAuthorities,
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    dispatch: Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
    sessions: Arc<awaken_session_application::SessionApplication>,
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
        .with_materialization_control(sessions.clone())
        .with_terminal_memory_control(sessions),
    ))
}
