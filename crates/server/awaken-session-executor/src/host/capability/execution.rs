// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Holds the exact credential materializer and ACP session-blob adapter shared by attempt
// @formal reason: executors; classified against the plaintext boundary; this link is not a formal-proof claim.
//! EXEC capability of [`ExecutorComponents`]: the ACP runtime backend, the tool
//! exporter and attempt decorator around the per-Session attempt router, and
//! the credential/session-blob adapters those executors share.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use crate::host::*;

/// Attempt-executor wiring. Consumer: the per-Session attempt router built in
/// Session context assembly and ACP backend realization.
pub(crate) struct AttemptBackends {
    /// The one ACP role this process holds.
    pub(crate) acp: AcpBackend,
    /// Higher-layer transport adapter for Session-owned tools exposed to ACP.
    /// The Host names only this contract; concrete MCP server setup remains in
    /// `awaken-coordinator` and does not add a protocol dependency to the substrate.
    pub(crate) acp_tool_exporter:
        Option<Arc<dyn awaken_session_environment_contract::tool_export::AcpToolExporter>>,
    /// Optional application wrapper around the complete per-Session attempt
    /// router. It cannot replace or bypass the built-in backend registry.
    pub(crate) attempt_decorator: Option<AttemptExecutorDecorator>,
    /// Canonical exact materializer used by runtime extensions. The Managed
    /// adapter may retain another clone, but both share the same repositories,
    /// secret store, revision checks, and extension-consumer registry.
    pub(crate) credential_materializer: Option<crate::PinnedCredentialMaterializer>,
    /// The exact ACP adapters this process advertises; `None` advertises the
    /// installed executor's whole catalog for MCP injection and no ACP CLI.
    pub(crate) acp_profile: Option<awaken_acp_contract::AcpWorkerProfile>,
}

impl AttemptBackends {
    /// No ACP backend or decorator yet; process composition installs the
    /// executor and its optional portable session-home adapter.
    pub(in crate::host) fn new(acp_profile: Option<awaken_acp_contract::AcpWorkerProfile>) -> Self {
        Self {
            acp_profile,
            acp: AcpBackend::Absent,
            acp_tool_exporter: None,
            attempt_decorator: None,
            credential_materializer: None,
        }
    }
}

/// Which ACP role a process holds, selected once by process composition. A
/// Coordinator that delegates physical execution to a registered Worker reads
/// only the catalog; a process that runs ACP CLIs holds the one execution
/// factory. The cases exclude each other, so a catalog read can never shadow
/// an installed executor.
///
/// Decision table (`acp_backend::tests::coordinator_catalog_policy_never_installs_an_acp_executor`):
/// | Rule | case | catalog policy | can launch an ACP CLI |
/// |---|---|---|---|
/// | P1 | `Absent` | refused: "ACP backend is not installed" | no |
/// | P2 | `Projection` | the static catalog | no |
/// | P3 | `Installed` | the factory's catalog (same instance) | yes |
pub(crate) enum AcpBackend {
    /// No ACP role.
    Absent,
    /// ACP runtime backend (R3/R4): serves `acp:*` sessions on an external CLI.
    Installed(Arc<dyn awaken_session_environment_contract::AcpSessionExecutorFactory>),
    /// Coordinator-only read of the same catalog contract, without installing
    /// an executor or becoming a second physical-effect owner.
    Projection(
        &'static dyn awaken_acp_contract::AcpAdapterPolicy<
            awaken_runtime_contract::resolved::ProviderServerTool,
        >,
    ),
}

impl AcpBackend {
    /// The installed executor factory, the only role that executes `acp:*`
    /// sessions or realizes their credentials (P3).
    pub(crate) fn installed(
        &self,
    ) -> Option<&Arc<dyn awaken_session_environment_contract::AcpSessionExecutorFactory>> {
        match self {
            Self::Installed(factory) => Some(factory),
            Self::Absent | Self::Projection(_) => None,
        }
    }
}
