// @formal surface requirement=protocol.acp.acp_normalization
// @formal reason: Declares the ACP Run executor over the single catalog in awaken-acp-catalog.
//! The ACP Run executor: launches an external ACP CLI described by one
//! `awaken-acp-catalog` row and drives it as a [`RunExecutor`].
//!
//! The catalog and session-blob custody live in `awaken-acp-catalog`, which
//! Coordinator and Control startup use without linking this crate; that
//! dependency edge, not a feature combination, is the boundary.

// Catalog facts read by the executor modules through `crate::` paths. They are
// owned and exported by `awaken-acp-catalog`; this crate does not re-export them.
#[cfg(test)]
use awaken_acp_catalog::FsSessionBlobStore;
use awaken_acp_catalog::{
    AcpAcquisition, AcpCli, AcpProbeCommand, AcpProbePredicate, AcpVersion,
    CredentialArtifactRequirement, SessionHomeKey, SessionKey, SessionPersistence,
    acp_adapter_policy, acp_cli, known_acp_clis, known_acp_publication_capabilities,
};

mod acp_cli;
#[cfg(test)]
#[path = "tests/cancellation_cause.rs"]
mod cancellation_cause_tests;
mod cleanup;
mod commit;
mod config_home;
mod entry;
#[cfg(test)]
#[path = "tests/failure_projection.rs"]
mod failure_projection_tests;
mod instructions;
mod mcp_context;
mod permission;
mod session_factory;
mod session_home;
mod subprocess;
#[cfg(test)]
mod tests;
mod tool_continuation;

include!("execution.rs");

pub mod host;
