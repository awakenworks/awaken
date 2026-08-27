//! Lifecycle operations shared by every concrete Session environment.

use awaken_provisioning_contract as pc;

use super::SessionEnvironment;

impl SessionEnvironment {
    pub(super) fn sandbox(&self) -> &dyn pc::Sandbox {
        match self {
            Self::Workdir(sandbox) => sandbox.as_ref(),
            Self::Namespace { sandbox, .. } => sandbox.as_ref(),
            Self::Container { sandbox, .. } => sandbox.as_ref(),
        }
    }

    pub(crate) fn handle(&self) -> pc::SandboxHandle {
        self.sandbox().handle()
    }

    pub(crate) async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        self.sandbox().status().await
    }

    #[cfg(test)]
    pub(crate) async fn run_test_command(
        &self,
        command: pc::Command,
    ) -> Result<pc::ExitStatus, pc::SandboxError> {
        self.sandbox().spawn(command).await?.wait().await
    }

    pub(crate) async fn dispose(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::dispose(sandbox.as_ref()).await,
            Self::Namespace { sandbox, hand } => {
                hand.stop().await;
                pc::Sandbox::dispose(sandbox.as_ref()).await
            }
            Self::Container { sandbox, hand, .. } => {
                hand.stop().await;
                sandbox.dispose().await
            }
        }
    }

    pub(crate) async fn quiesce(&self) {
        self.stop_bound_processes().await;
    }

    pub(crate) async fn checkpoint(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        self.sandbox().checkpoint(request, store).await
    }
}
