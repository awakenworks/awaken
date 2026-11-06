// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: Existing native scope/error or route assembly projection, without a second Session or authorization owner.
//! Assemble profiled and native Session read routes over their original owners.
use crate::{WorkerTransportBuildError, awaken_extensions, session_read_extensions};
use awaken_protocol_managed::ManagedState;
use awaken_session_application::SessionApplication;
use axum::Router;
use std::sync::Arc;
pub(super) fn session_extensions(
    managed: Arc<ManagedState>,
    sessions: Arc<SessionApplication>,
    dispatch: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
) -> Result<Router, WorkerTransportBuildError> {
    let session_extensions = awaken_extensions::profiled_session_router(
        awaken_protocol_managed::create_profiled_session,
    )
    .merge(awaken_extensions::profiled_session_run_router(
        awaken_protocol_managed::submit_profiled_session_run,
    ))
    .merge(awaken_extensions::profiled_session_run_events_router(
        awaken_protocol_managed::get_profiled_session_run_events,
    ))
    .merge(awaken_extensions::profiled_session_run_resume_router(
        awaken_protocol_managed::resume_profiled_session_run,
    ))
    .merge(awaken_extensions::profiled_session_release_router(
        awaken_protocol_managed::release_profiled_session,
    ))
    .merge(session_read_extensions())
    .with_state(managed);
    let reads = crate::session_dispatch_reads::SessionRunDispatchReads::new(&sessions, dispatch)
        .map_err(|error| WorkerTransportBuildError::Dispatch(error.into()))?;
    Ok(
        session_extensions.merge(awaken_protocol_awaken::session_run_dispatch_router(
            Arc::new(reads),
        )),
    )
}
