//! The Worker's authenticated HTTP clients of the Coordinator: dispatch queue,
//! claimed commits, Worker lifecycle and Session control.

mod claimed_commit_client;
mod dispatch_client;
mod session_control;
mod worker_control_client;

pub use claimed_commit_client::{RemoteClaimedRunCommit, remote_claimed_commit};
pub use dispatch_client::{
    HttpDispatchQueue, dispatch_transport_with_upstream, worker_transports_with_upstream,
};
pub use session_control::WorkerControlSessionClient;
pub use worker_control_client::{WorkerControlClient, WorkerRegistrationError};
