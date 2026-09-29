// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This source implements the repository-controlled remote worker product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! ACP-facing capabilities of the one Session environment.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use super::SandboxEnvironment;

use awaken_session_environment_contract::AgentProcessEnvironment;
pub(crate) use awaken_session_environment_contract::{AgentFilesystemClient, AgentFilesystemView};

#[async_trait]
impl AgentProcessEnvironment for SandboxEnvironment {
    fn channel_helper(&self) -> Option<String> {
        match self {
            Self::Container { hand, .. } | Self::Namespace { hand, .. } => {
                Some(hand.binary_path().to_string())
            }
            Self::Workdir { .. } => None,
        }
    }
    fn is_container(&self) -> bool {
        matches!(self, Self::Container { .. })
    }

    fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir { .. })
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
            Self::Workdir { sandbox, .. } => sandbox
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
            Self::Workdir { .. } => pc::WorkspaceLayout::ACP_CONFIG_SUBDIR.to_string(),
        }
    }

    fn workspace_cwd(&self) -> String {
        match self {
            Self::Workdir { sandbox, .. } => {
                sandbox.workspace_path().to_string_lossy().into_owned()
            }
            Self::Namespace { .. } | Self::Container { .. } => {
                pc::WorkspaceLayout::ROOT.to_string()
            }
        }
    }

    fn filesystem_view(&self, working_directory: Option<&str>) -> AgentFilesystemView {
        self.agent_filesystem_view(working_directory, AgentFilesystemClient::AcpProcess)
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
            Box<dyn awaken_agent_channel::AgentChannel>,
        ),
        pc::SandboxError,
    > {
        self.spawn_agent(command).await
    }
}
