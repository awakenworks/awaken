//! Process-local transport realization of the closed Sandbox control protocol.
//!
//! `awaken-sandbox-control` owns the protocol vocabulary (service kinds,
//! request/response messages, the reserved in-Sandbox directory) and the
//! publication ports. This crate owns what every provider and the in-Sandbox
//! helper share to *carry* that protocol: the bounded u32 length-prefixed JSON
//! frame codec, the rendezvous endpoints inside the reserved directory, and the
//! provider-neutral publication lifecycle (slot + task) that Namespace and
//! container providers realize unchanged.
//!
//! It deliberately stays out of the contract: framing, Tokio tasks, and socket
//! file names are execution mechanics, not domain vocabulary.

mod codec;
mod publication;

pub use codec::{
    MAX_SANDBOX_CONTROL_FRAME_BYTES, read_frame, serve_one, serve_one_after_first_byte, write_frame,
};
pub use publication::{
    SandboxControlPublicationLease, SandboxControlPublicationSlot, SandboxControlPublicationTask,
};

/// One stable in-Sandbox endpoint shared by Namespace and hosted Kubernetes.
/// Providers project it differently outside the Sandbox, but Agent/Hand
/// processes inherit one transport-independent helper overlay.
pub const REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH: &str =
    "/run/awaken/control/repository-git-credential.sock";

/// Sidecar-private readiness evidence adjacent to the stable logical socket.
/// The workload can read but never write this file; only the trusted forwarder
/// creates it after both of its listeners are bound.
pub const REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH: &str =
    "/run/awaken/control/repository-git-credential.ready";

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_sandbox_control::SANDBOX_CONTROL_DIRECTORY_PATH;

    #[test]
    fn rendezvous_endpoints_live_directly_in_the_reserved_control_directory() {
        // RV1: spec admission reserves only SANDBOX_CONTROL_DIRECTORY_PATH, so
        // every endpoint must be its direct child or a user mount could shadow it.
        for endpoint in [
            REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH,
            REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH,
        ] {
            assert_eq!(
                std::path::Path::new(endpoint).parent(),
                Some(std::path::Path::new(SANDBOX_CONTROL_DIRECTORY_PATH)),
                "RV1 {endpoint}"
            );
        }
    }
}
