//! Worker-owned runtime transport adapters over the protocol-neutral SharedHost.

mod application_control;
mod claimed_commit_client;
mod dispatch_client;
mod repository_binding_client;
mod worker_control_client;

pub use application_control::WorkerControlApplicationSessionClient;
pub use claimed_commit_client::{RemoteClaimedRunCommit, remote_claimed_commit};
pub use dispatch_client::{
    HttpDispatchQueue, dispatch_transport_with_upstream, worker_transports_with_upstream,
};
pub use repository_binding_client::HttpRepositoryBindingVerifier;
pub use worker_control_client::{WorkerControlClient, WorkerRegistrationError};
