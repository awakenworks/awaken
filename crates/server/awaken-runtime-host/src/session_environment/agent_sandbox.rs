//! ACP-facing capabilities of the one Session environment.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use super::SessionEnvironment;

/// Segregated capability needed by the ACP channel adapter. Keeping it beside
/// the Session owner avoids teaching the neutral provisioning contract about
/// async byte channels.
#[async_trait]
pub(crate) trait AgentSandbox: Send + Sync {
    fn is_container(&self) -> bool;

    /// Whether a child runs as the trusted host user and can consume that user's
    /// PATH/HOME-owned CLI identity. Namespace and container environments cannot
    /// truthfully provide this without mounting/copying credentials.
    fn supports_host_identity(&self) -> bool;

    fn config_home(&self) -> String;

    /// Jail-relative/interior path used to materialize the config home. This is
    /// distinct from [`Self::config_home`] on the Workdir tier, where the child
    /// sees an absolute host path but the file writer accepts only a logical
    /// path below the Session root.
    fn config_home_logical(&self) -> String;

    /// Workspace path understood by the ACP agent inside this environment.
    fn workspace_cwd(&self) -> String;

    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError>;

    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    >;
}

#[async_trait]
impl AgentSandbox for SessionEnvironment {
    fn is_container(&self) -> bool {
        matches!(self, Self::Container { .. })
    }

    fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir(_))
    }

    fn config_home(&self) -> String {
        match self {
            // A bound container is already running and its root filesystem may
            // be read-only. Late ACP config therefore lives in the one writable,
            // Session-owned workspace rather than the creation-time /acp-config
            // mount used by the legacy one-shot source.
            Self::Container { .. } | Self::Namespace { .. } => {
                pc::WorkspaceLayout::ACP_CONFIG_ROOT.to_string()
            }
            Self::Workdir(sandbox) => sandbox
                .workspace_path()
                .join(pc::WorkspaceLayout::ACP_CONFIG_SUBDIR)
                .to_string_lossy()
                .into_owned(),
        }
    }

    fn config_home_logical(&self) -> String {
        match self {
            Self::Container { .. } | Self::Namespace { .. } => {
                pc::WorkspaceLayout::ACP_CONFIG_ROOT.to_string()
            }
            Self::Workdir(_) => pc::WorkspaceLayout::ACP_CONFIG_SUBDIR.to_string(),
        }
    }

    fn workspace_cwd(&self) -> String {
        match self {
            Self::Workdir(sandbox) => sandbox.workspace_path().to_string_lossy().into_owned(),
            Self::Namespace { .. } | Self::Container { .. } => {
                pc::WorkspaceLayout::ROOT.to_string()
            }
        }
    }

    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        self.materialize_inline(logical, contents).await
    }

    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    > {
        self.spawn_agent(command).await
    }
}
