//! The single live execution environment owned by a Session.
//!
//! Host code depends on this capability object instead of retaining a concrete
//! sandbox in `SessionCtx`. Workdir is the first adapter; Namespace and Container
//! plug into the same owner without adding another Native/ACP lifecycle.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::{DiscoveredSkillFile, LocalSandbox};

/// Segregated capability needed by the ACP channel adapter. Keeping it beside
/// the Session owner avoids teaching the neutral provisioning contract about
/// async byte channels.
#[async_trait]
pub(crate) trait AgentSandbox: Send + Sync {
    fn materialize_inline(&self, logical: &str, contents: &[u8]) -> Result<(), pc::SandboxError>;

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
impl AgentSandbox for LocalSandbox {
    fn materialize_inline(&self, logical: &str, contents: &[u8]) -> Result<(), pc::SandboxError> {
        self.materialize_inline(logical, contents)
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

/// One realized sandbox shared by every Run attempt in a Session.
pub(crate) enum SessionEnvironment {
    Workdir(Arc<LocalSandbox>),
}

impl SessionEnvironment {
    #[must_use]
    pub(crate) fn workdir(sandbox: LocalSandbox) -> Self {
        Self::Workdir(Arc::new(sandbox))
    }

    /// Transitional access for child/skill composition. It returns the same live
    /// instance and therefore cannot create a second sandbox.
    #[must_use]
    pub(crate) fn workdir_handle(&self) -> Arc<LocalSandbox> {
        match self {
            Self::Workdir(sandbox) => sandbox.clone(),
        }
    }

    pub(crate) fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        match self {
            Self::Workdir(sandbox) => sandbox.rooted_tools(),
        }
    }

    pub(crate) fn provision_repo(
        &self,
        logical: &str,
        url: &str,
        git_ref: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.provision_repo(logical, url, git_ref, token),
        }
    }

    pub(crate) fn push_repo(
        &self,
        logical: &str,
        token: Option<&str>,
    ) -> Result<bool, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.push_repo(logical, token),
        }
    }

    pub(crate) fn list_files(&self, subdir: &str) -> Vec<(String, Vec<u8>)> {
        match self {
            Self::Workdir(sandbox) => sandbox.list_files(subdir),
        }
    }

    pub(crate) fn scan_skill_dir(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        match self {
            Self::Workdir(sandbox) => sandbox.scan_skill_dir(subdir),
        }
    }

    pub(crate) fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_inline(logical, contents),
        }
    }

    pub(crate) async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannelType>), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.spawn_agent(command).await,
        }
    }
}

#[async_trait]
impl AgentSandbox for SessionEnvironment {
    fn materialize_inline(&self, logical: &str, contents: &[u8]) -> Result<(), pc::SandboxError> {
        self.materialize_inline(logical, contents)
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

#[async_trait]
impl pc::Sandbox for SessionEnvironment {
    fn id(&self) -> &str {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::id(sandbox.as_ref()),
        }
    }

    fn handle(&self) -> pc::SandboxHandle {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::handle(sandbox.as_ref()),
        }
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::spawn(sandbox.as_ref(), command).await,
        }
    }

    async fn attach(
        &self,
        requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::attach(sandbox.as_ref(), requirement).await,
        }
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::artifacts(sandbox.as_ref()).await,
        }
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::read_artifact(sandbox.as_ref(), id).await,
        }
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::realized(sandbox.as_ref()),
        }
    }

    async fn process(
        &self,
        process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::process(sandbox.as_ref(), process_id).await,
        }
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::status(sandbox.as_ref()).await,
        }
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::renew_lease(sandbox.as_ref()).await,
        }
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::dispose(sandbox.as_ref()).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{
        IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxProvider, SandboxSpec,
    };
    use awaken_sandbox_local::LocalProvider;
    use tokio::io::AsyncReadExt;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "session-env".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            limits: ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[tokio::test]
    async fn native_process_and_agent_channel_share_one_live_environment() {
        let base = tempfile::tempdir().unwrap();
        let local = LocalProvider::new(base.path())
            .create_sandbox(&spec())
            .await
            .unwrap();
        let original = Sandbox::handle(&local);
        let environment = SessionEnvironment::workdir(local);
        assert_eq!(Sandbox::handle(&environment), original);

        let native = environment
            .spawn(pc::Command::new([
                "/bin/sh",
                "-c",
                "printf shared-state > marker",
            ]))
            .await
            .unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

        let (agent, mut channel) = environment
            .spawn_agent(pc::Command::new(["/bin/sh", "-c", "cat marker"]))
            .await
            .unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0));
        assert_eq!(output, "shared-state");

        environment.dispose().await.unwrap();
    }
}
