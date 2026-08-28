//! Coordinator-owned HTTP interfaces over protocol-neutral application ports.
//!
//! Durable operations and the complete registered-Worker dispatch surface are
//! assembled here. The runtime Host supplies only execution/recovery ports; it
//! does not own these routes.

mod claimed_commit_http;
mod durable_ops;
mod worker_dispatch;

pub use claimed_commit_http::{ClaimedCommitHttpService, claimed_commit_router};
pub use durable_ops::{durable_ops_router, durable_ops_router_with_session_admission};
pub use worker_dispatch::{
    RegisteredDispatchDependencies, WorkerDispatchService, dispatch_transport_router_with_service,
    registered_dispatch_router, registered_worker_transport_router_with_services,
    worker_environment_warmup_router, worker_environment_warmup_router_with_clock,
};
