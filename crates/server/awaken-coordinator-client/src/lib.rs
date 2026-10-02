//! Worker-owned attempt context and transport adapters behind the neutral Host.

mod claimed_commit_client;
mod dispatch_client;
mod session_control;
mod worker_control_client;

// Existing external module paths remain aliases of the one shared owner while
// transport callers migrate; no worker-side copy of these types is retained.
#[doc(hidden)]
pub use awaken_dispatch_execution::{claimed_stream, execution_capacity, worker_context};

pub use claimed_commit_client::{RemoteClaimedRunCommit, remote_claimed_commit};
pub use dispatch_client::{
    HttpDispatchQueue, dispatch_transport_with_upstream, worker_transports_with_upstream,
};
pub use session_control::WorkerControlSessionClient;
pub use worker_control_client::{WorkerControlClient, WorkerRegistrationError};
