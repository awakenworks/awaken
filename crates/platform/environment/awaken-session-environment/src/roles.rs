// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The sandbox-backed environment plays each consuming role by delegating to its one
// @formal reason: lifecycle owner; fences and authorities pass through unchanged and scoped execution is
// @formal reason: exposed only when the realization runs one. This link is not a formal-proof claim.
//! The sandbox-backed environment playing each consuming role of the Session
//! environment contract, and the record of handles the Host carries it as.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_session_environment_contract::{
    AgentFilesystemClient, AgentFilesystemView, AgentToolEnvironment, CapturedArtifact,
    DiscoveredSkillFile, EnvironmentHarvest, EnvironmentLifecycle, EnvironmentProjection,
    EnvironmentQuiescenceProof, EnvironmentStopProof, LiveProjectionUpdate, SessionEnvironment,
    SkillFilesystem,
};
use awaken_tool_contract::tool::{RawTool, ToolExecutor};

use super::SandboxEnvironment;

impl SandboxEnvironment {
    /// Hand this environment to the Host as one handle per consuming role.
    #[must_use]
    pub fn into_session_environment(self) -> SessionEnvironment {
        let environment = Arc::new(self);
        let scoped_execution = scoped_execution(&environment);
        SessionEnvironment::new(environment, scoped_execution)
    }
}

#[cfg(target_os = "linux")]
fn scoped_execution(
    environment: &Arc<SandboxEnvironment>,
) -> Option<Arc<dyn awaken_session_environment_contract::ScopedExecution>> {
    match environment.as_ref() {
        SandboxEnvironment::Workdir { sandbox, .. } if sandbox.has_scoped_execution() => {
            Some(Arc::new(WorkdirScopedExecution(sandbox.clone()))
                as Arc<
                    dyn awaken_session_environment_contract::ScopedExecution,
                >)
        }
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn scoped_execution(
    _environment: &Arc<SandboxEnvironment>,
) -> Option<Arc<dyn awaken_session_environment_contract::ScopedExecution>> {
    None
}

/// The scoped execution tree of a Linux TrustedHost Workdir.
#[cfg(target_os = "linux")]
struct WorkdirScopedExecution(Arc<awaken_sandbox_local::LocalSandbox>);

#[cfg(target_os = "linux")]
#[async_trait]
impl awaken_session_environment_contract::ScopedExecution for WorkdirScopedExecution {
    fn needs_execution_authority(&self) -> bool {
        self.0.needs_execution_authority()
    }

    fn bind_execution_authority(
        &self,
        authority: Arc<dyn pc::CurrentExecutionAuthority>,
    ) -> Result<bool, pc::SandboxError> {
        self.0.bind_execution_authority(authority)
    }

    async fn refresh_scoped_execution(&self) -> Result<(), pc::SandboxError> {
        self.0.refresh_scoped_execution().await
    }
}

#[async_trait]
impl EnvironmentLifecycle for SandboxEnvironment {
    fn handle(&self) -> pc::SandboxHandle {
        SandboxEnvironment::handle(self)
    }

    async fn stop_bound_processes(&self) -> Result<EnvironmentStopProof, pc::SandboxError> {
        SandboxEnvironment::stop_bound_processes(self).await
    }

    async fn quiesce(&self) -> Result<EnvironmentQuiescenceProof, pc::SandboxError> {
        SandboxEnvironment::quiesce(self).await
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::dispose(self).await
    }

    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::acknowledge_memory_reconciliation(
            self,
            effect_fence,
            complete_materializations,
            authority,
        )
        .await
    }

    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        SandboxEnvironment::prepare_disposal_for_effect(self, effect_fence, authority).await
    }

    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::dispose_for_effect(self, authorization, authority).await
    }

    async fn checkpoint_for_effect(
        &self,
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        SandboxEnvironment::checkpoint_for_effect(self, request, store, effect_fence, authority)
            .await
    }

    async fn cleanup_checkpoint_for_terminal(
        &self,
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
        expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::cleanup_checkpoint_for_terminal(
            self,
            request,
            store,
            expected_effect_fence,
            terminal_effect_fence,
            authority,
        )
        .await
    }
}

impl LiveProjectionUpdate for super::session_hand::HandProjectionUpdate<'_> {
    fn commit(self: Box<Self>) {
        super::session_hand::HandProjectionUpdate::commit(*self);
    }
}

#[async_trait]
impl EnvironmentProjection for SandboxEnvironment {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        SandboxEnvironment::capabilities(self)
    }

    fn reserve_owned_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::reserve_owned_path(self, path)
    }

    fn validate_live_mount_replacement(
        &self,
        previous: &[pc::MountRequirement],
        next: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::validate_live_mount_replacement(self, previous, next)
    }

    async fn begin_live_projection_update<'a>(
        &'a self,
    ) -> Result<Option<Box<dyn LiveProjectionUpdate + 'a>>, pc::SandboxError> {
        Ok(SandboxEnvironment::begin_live_projection_update(self)
            .await?
            .map(|update| Box::new(update) as Box<dyn LiveProjectionUpdate + 'a>))
    }

    async fn attach_mount(
        &self,
        requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        SandboxEnvironment::attach_mount(self, requirement).await
    }

    async fn reconcile_adopted_mounts(
        &self,
        requirements: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::reconcile_adopted_mounts(self, requirements).await
    }

    async fn remove_projection_path(&self, logical: &str) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::remove_projection_path(self, logical).await
    }

    async fn write_workspace_file(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::write_workspace_file(self, logical, contents).await
    }
}

#[async_trait]
impl SkillFilesystem for SandboxEnvironment {
    fn skill_directory_path(&self, directory: &str) -> Result<String, pc::SandboxError> {
        SandboxEnvironment::skill_directory_path(self, directory)
    }

    fn scan_skill_dir(&self, subdir: &str) -> Result<Vec<DiscoveredSkillFile>, pc::SandboxError> {
        SandboxEnvironment::scan_skill_dir(self, subdir)
    }

    fn register_skill_dir(&self, subdir: &str) {
        SandboxEnvironment::register_skill_dir(self, subdir);
    }

    async fn refresh_skills(&self) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::refresh_skills(self).await
    }

    async fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>, bool)],
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironment::materialize_read_only_tree(self, subdir, files).await
    }
}

#[async_trait]
impl EnvironmentHarvest for SandboxEnvironment {
    async fn capture_artifacts(&self) -> Result<Vec<CapturedArtifact>, pc::SandboxError> {
        SandboxEnvironment::capture_artifacts(self).await
    }

    async fn list_frozen_mount_files(
        &self,
        mount_path: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        SandboxEnvironment::list_frozen_mount_files(self, mount_path).await
    }
}

#[async_trait]
impl AgentToolEnvironment for SandboxEnvironment {
    fn tool_executor(&self) -> Arc<dyn ToolExecutor> {
        SandboxEnvironment::tool_executor(self)
    }

    async fn tool_execution_domain(
        &self,
    ) -> Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, pc::SandboxError> {
        SandboxEnvironment::tool_execution_domain(self).await
    }

    fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        SandboxEnvironment::rooted_tools(self)
    }

    fn agent_filesystem_view(
        &self,
        working_directory: Option<&str>,
        client: AgentFilesystemClient,
    ) -> AgentFilesystemView {
        SandboxEnvironment::agent_filesystem_view(self, working_directory, client)
    }

    fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir { .. })
    }
}
