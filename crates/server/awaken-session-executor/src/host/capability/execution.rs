// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Holds the exact credential materializer and ACP session-blob adapter shared by attempt
// @formal reason: executors; classified against the plaintext boundary; this link is not a formal-proof claim.
//! EXEC capability of [`SharedHost`]: the ACP runtime backend, the tool
//! exporter and attempt decorator around the per-Session attempt router, and
//! the credential/session-blob adapters those executors share.

use crate::host::*;

/// Attempt-executor wiring. Consumer: the per-Session attempt router built in
/// Session context assembly and ACP backend realization.
pub(crate) struct AttemptBackends {
    /// ACP runtime backend (R3/R4): serves `acp:*` sessions on an external CLI.
    pub(crate) acp: Option<Arc<dyn awaken_session_environment_contract::AcpSessionExecutorFactory>>,
    /// Coordinator-only read of the same catalog contract, without installing
    /// an executor or becoming a second physical-effect owner.
    pub(crate) acp_projection_policy: Option<
        &'static dyn awaken_acp_contract::AcpAdapterPolicy<
            awaken_runtime_contract::resolved::ProviderServerTool,
        >,
    >,
    /// Higher-layer transport adapter for Session-owned tools exposed to ACP.
    /// The Host names only this contract; concrete MCP server setup remains in
    /// `awaken-coordinator` and does not add a protocol dependency to the substrate.
    pub(crate) acp_tool_exporter:
        Option<Arc<dyn awaken_runtime_contract::tool_export::AcpToolExporter>>,
    /// Optional application wrapper around the complete per-Session attempt
    /// router. It cannot replace or bypass the built-in backend registry.
    pub(crate) attempt_decorator: Option<AttemptExecutorDecorator>,
    /// Canonical exact materializer used by runtime extensions. The Managed
    /// adapter may retain another clone, but both share the same repositories,
    /// secret store, revision checks, and extension-consumer registry.
    pub(crate) credential_materializer: Option<crate::PinnedCredentialMaterializer>,
}

impl AttemptBackends {
    /// No ACP backend or decorator yet; process composition installs the
    /// executor and its optional portable session-home adapter.
    pub(in crate::host) fn new(_deployment: &crate::deployment_config::DeploymentConfig) -> Self {
        Self {
            acp: None,
            acp_projection_policy: None,
            acp_tool_exporter: None,
            attempt_decorator: None,
            credential_materializer: None,
        }
    }
}

impl SharedHost {
    #[must_use]
    pub fn with_acp_tool_exporter(
        mut self,
        exporter: Arc<dyn awaken_runtime_contract::tool_export::AcpToolExporter>,
    ) -> Self {
        self.exec.acp_tool_exporter = Some(exporter);
        self
    }

    /// Install the canonical exact credential materializer used by runtime
    /// extensions such as paid WebSearch. The value is cloned from the same
    /// process-startup instance used by Managed MCP/Resource realization.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: crate::PinnedCredentialMaterializer,
    ) -> Self {
        self.exec.credential_materializer = Some(materializer);
        self
    }

    /// Wrap the complete per-Session Native/ACP/A2A attempt boundary.
    ///
    /// The Host always constructs the authoritative backend router first. The
    /// application can adapt envelopes, capabilities, and business results
    /// around it, but cannot replace backend selection.
    #[must_use]
    pub fn with_attempt_decorator(mut self, decorator: AttemptExecutorDecorator) -> Self {
        self.exec.attempt_decorator = Some(decorator);
        self
    }
}
