// @formal surface requirement=protocol.acp.acp_normalization
// @formal reason: Declares the single ACP catalog and opaque durable session-blob custody; it starts no process.
//! The ACP adapter catalog and the durable custody of each adapter's opaque
//! session blobs.
//!
//! One data row per external ACP CLI says how it is acquired, discovered,
//! launched and published. Coordinator and Control read these facts and erase
//! session blobs through [`FsSessionBlobStore`]; the ACP executor
//! (`awaken-run-executor-acp`) projects a row onto a process launch. Keeping the
//! catalog in its own crate is what keeps the executor out of the Coordinator
//! and Control dependency graphs.

mod acp_cli;
mod adapter_policy;
mod discovery_spec;
mod instructions;
mod session_blob;

pub use acp_cli::{
    AcpAcquisition, AcpCli, AcpImageRequirement, BackendModelInterface, CredentialArtifactCodec,
    CredentialArtifactRequirement, CredentialArtifactSpec, ManagedCredentialDelivery,
    ManagedModelInterface, ManagedProviderConfigCodec, ManagedProviderConfigDelivery,
    ModelDelivery, ProviderServerToolRefusal, SessionKey, SessionPersistence, acp_cli,
    image_runtime_contract_json, known_acp_clis, known_acp_publication_capabilities,
};
pub use adapter_policy::acp_adapter_policy;
pub use discovery_spec::{
    AcpDiscoverySpec, AcpLoginProbe, AcpLoginRule, AcpProbeCommand, AcpProbePredicate, AcpVersion,
};
pub use instructions::{InstructionsChannel, InstructionsDelivery};
pub use session_blob::{FsSessionBlobStore, SessionBlobStore};

/// Identifies a CLI's portable session-home: request owner, conversation, and
/// adapter. Subject scope is explicit because session blobs contain opaque model
/// content and must be independently erasable; it is never inferred by the
/// executor or read from ambient process state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHomeKey {
    pub data_subject_id: Option<String>,
    pub thread_id: String,
    pub adapter: String,
}
