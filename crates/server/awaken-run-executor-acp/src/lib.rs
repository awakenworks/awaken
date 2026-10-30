// @formal surface requirement=protocol.acp.acp_normalization
// @formal reason: Declares the single catalog and opaque durable blob custody; effectful execution is explicitly feature-gated.
//! One ACP catalog and opaque session-blob custodian, with explicit execution roles.
//!
//! Coordinator/Control startup consumes catalog facts and the same durable blob
//! store without compiling host preparation or the Run executor. Worker and CLI
//! composition explicitly enable those implementations; features are additive,
//! so their isolated product builds and the combined workspace are checked apart.

#![cfg_attr(
    not(feature = "execution"),
    doc = r#"
Compile boundary B1: catalog and durable custody remain available with no
execution feature; this type-only case starts no process and reads no store.

```no_run
use awaken_run_executor_acp::{FsSessionBlobStore, SessionBlobStore, SessionHomeKey, known_acp_publication_capabilities};
let store = FsSessionBlobStore::new(std::path::PathBuf::from("opaque-session-blobs"));
let _: &dyn SessionBlobStore = &store;
let _ = known_acp_publication_capabilities();
let _ = SessionHomeKey { data_subject_id: None, thread_id: "t".into(), adapter: "codex".into() };
```

Compile boundary B2: a metadata/blob consumer cannot name the physical executor.

```compile_fail
use awaken_run_executor_acp::AcpRunExecutor;
```
"#
)]
#![cfg_attr(
    not(feature = "host-preparation"),
    doc = r#"
Compile boundary B3: a consumer without host preparation cannot name its adapter.

```compile_fail
use awaken_run_executor_acp::host::prepare_host_acp_capabilities_with;
```
"#
)]
#![cfg_attr(
    all(feature = "execution", not(feature = "host-preparation")),
    doc = r#"
Compile boundary B5: Sandbox-backed execution need not compile trusted-host
preparation. This type-only case constructs no executor or process.

```no_run
let _: Option<awaken_run_executor_acp::AcpRunExecutor> = None;
```
"#
)]

mod acp_cli;

mod adapter_policy;
mod discovery_spec;
mod instructions;
mod session_home;

pub use acp_cli::{
    AcpAcquisition, AcpCli, AcpImageRequirement, BackendModelInterface, CredentialArtifactCodec,
    CredentialArtifactRequirement, CredentialArtifactSpec, ManagedCredentialDelivery,
    ManagedModelInterface, ManagedProviderConfigCodec, ManagedProviderConfigDelivery,
    ModelDelivery, SessionKey, SessionPersistence, acp_cli, image_runtime_contract_json,
    known_acp_clis, known_acp_publication_capabilities,
};
pub use adapter_policy::acp_adapter_policy;
pub use discovery_spec::{
    AcpDiscoverySpec, AcpLoginProbe, AcpLoginRule, AcpProbeCommand, AcpProbePredicate, AcpVersion,
};
pub use instructions::{InstructionsChannel, InstructionsDelivery};
pub use session_home::{FsSessionBlobStore, SessionBlobStore};

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

#[cfg(feature = "execution")]
mod cleanup;
#[cfg(feature = "execution")]
mod commit;
#[cfg(feature = "execution")]
mod config_home;
#[cfg(feature = "execution")]
mod mcp_context;
#[cfg(feature = "execution")]
mod permission;
#[cfg(feature = "execution")]
mod session_factory;
#[cfg(feature = "execution")]
mod subprocess;
#[cfg(all(test, feature = "execution"))]
mod tests;
#[cfg(feature = "execution")]
mod tool_continuation;

#[cfg(feature = "execution")]
include!("execution.rs");

#[cfg(feature = "host-preparation")]
pub mod host;
