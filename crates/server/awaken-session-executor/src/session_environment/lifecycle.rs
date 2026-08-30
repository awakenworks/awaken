//! Lifecycle operations shared by every concrete Session environment.

use awaken_provisioning_contract as pc;

use super::SessionEnvironment;
use super::environment::{EnvironmentQuiescenceProof, EnvironmentStopProof};
use super::session_hand::HandStopProof;

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
                hand.stop().await?;
                pc::Sandbox::dispose(sandbox.as_ref()).await
            }
            Self::Container { sandbox, hand, .. } => {
                // A resident Hand is terminated by disposal of its owning
                // sandbox below; the retained proof is therefore admissible
                // here but never at checkpoint-and-release quiescence.
                hand.stop().await?;
                sandbox.dispose().await
            }
        }
    }

    pub(crate) async fn quiesce(&self) -> Result<EnvironmentQuiescenceProof, pc::SandboxError> {
        let proof = match self {
            Self::Workdir(_) => EnvironmentStopProof::NoBoundProcesses,
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => {
                EnvironmentStopProof::Hand(hand.quiesce().await?)
            }
        };
        match proof {
            EnvironmentStopProof::NoBoundProcesses
            | EnvironmentStopProof::Hand(HandStopProof::NoBinding)
            | EnvironmentStopProof::Hand(HandStopProof::AttachedProcessReaped) => {
                Ok(EnvironmentQuiescenceProof)
            }
            EnvironmentStopProof::Hand(HandStopProof::ResidentProcessRetained) => {
                Err(pc::SandboxError::new(
                    "resident Session Hand remains owned by the sandbox and cannot be checkpointed after release",
                ))
            }
        }
    }

    pub(crate) async fn checkpoint(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        self.sandbox().checkpoint(request, store).await
    }
}
