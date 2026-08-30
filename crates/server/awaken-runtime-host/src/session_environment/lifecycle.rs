//! Lifecycle operations shared by every concrete Session environment.

use awaken_provisioning_contract as pc;

use super::SessionEnvironment;
use super::environment::{EnvironmentQuiescenceProof, EnvironmentStopProof};
use super::session_hand::{HandStopProof, SessionHandExecutor};

/// Close the one Hand owner, then always reach the owning Sandbox's physical
/// disposal boundary. An indeterminate Hand reap forbids replacement, but the
/// complete Environment disposal is itself the authoritative process-substrate
/// teardown and must not be short-circuited by that narrower proof failure.
async fn dispose_environment_after_closing_hand<S: pc::Sandbox + ?Sized>(
    hand: &SessionHandExecutor,
    sandbox: &S,
) -> Result<(), pc::SandboxError> {
    if let Err(error) = hand.stop().await {
        tracing::warn!(
            error = %error,
            "Hand reap was indeterminate; continuing with owning Environment disposal"
        );
    }
    pc::Sandbox::dispose(sandbox).await
}

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

    /// Add a conservative path reservation to the provider's V2 durable
    /// handle. The caller persists that handle before starting a live physical
    /// projection, making the existing Environment binding CAS the write-ahead
    /// boundary for crash recovery.
    pub(crate) fn reserve_owned_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.reserve_owned_path(path),
            Self::Namespace { sandbox, .. } => sandbox.reserve_owned_path(path),
            Self::Container { sandbox, .. } => sandbox.record_owned_path(path)?,
        }
        Ok(())
    }

    /// Direct provider status is test observability only. Production recovery
    /// uses the provider's effect-free typed observation under the Session
    /// realization fence.
    #[cfg(test)]
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
                dispose_environment_after_closing_hand(hand, sandbox.as_ref()).await
            }
            Self::Container { sandbox, hand, .. } => {
                // A resident Hand is terminated by disposal of its owning
                // sandbox below; the retained proof is therefore admissible
                // here but never at checkpoint-and-release quiescence.
                dispose_environment_after_closing_hand(hand, sandbox.as_ref()).await
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

    /// Forward the aggregate-owned exact evidence acknowledgement to the one
    /// concrete Sandbox lifecycle owner. The Host remains responsible for
    /// deciding which reconciliation work precedes this process-local edge.
    pub(crate) async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
    ) -> Result<(), pc::SandboxError> {
        self.sandbox()
            .acknowledge_memory_reconciliation(effect_fence, complete_materializations)
            .await
    }

    /// Complete every provider source-dependent disposal participant without
    /// deleting the physical realization. The caller must first obtain the
    /// appropriate Hand proof: checkpoint release uses strict [`Self::quiesce`],
    /// while terminal preparation may accept a resident Hand whose owning
    /// Sandbox is removed only by the later physical disposal phase.
    pub(crate) async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        self.sandbox()
            .prepare_disposal_for_effect(effect_fence)
            .await
    }

    /// Physically dispose only under the aggregate-owned effect fence after
    /// the aggregate durably accepted preparation. No Hand or other live-data
    /// participant is touched here; the provider revalidates immutable
    /// incarnation and lease evidence at its destructive boundary.
    pub(crate) async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), pc::SandboxError> {
        self.sandbox().dispose_for_effect(authorization).await
    }

    pub(crate) async fn checkpoint_for_effect(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        self.sandbox()
            .checkpoint_for_effect(request, store, effect_fence)
            .await
    }

    pub(crate) async fn cleanup_checkpoint_for_terminal(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        self.sandbox()
            .cleanup_checkpoint_for_terminal(
                request,
                store,
                expected_effect_fence,
                terminal_effect_fence,
            )
            .await
    }
}
