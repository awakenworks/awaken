use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::model::Phase;

use crate::hooks::{PhaseContext, PhaseHook};
use crate::state::StateCommand;

use crate::agent::state::PendingWorkKey;

use super::manager::BackgroundTaskManager;
use super::state::BackgroundTaskStateKey;

/// Phase hook that syncs background task metadata with the persisted state.
///
/// - `RunStart`: restores persisted metadata and performs orphan detection.
/// - `RunEnd`: no-op (metadata is committed directly by the manager).
/// - `StepEnd`: updates `PendingWorkKey` based on running task status.
pub(crate) struct BackgroundTaskSyncHook {
    pub(crate) manager: Arc<BackgroundTaskManager>,
}

#[async_trait::async_trait]
impl PhaseHook for BackgroundTaskSyncHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        match ctx.phase {
            Phase::RunStart => {
                let thread_id = &ctx.run_identity.thread_id;
                let snapshot = ctx
                    .state::<BackgroundTaskStateKey>()
                    .cloned()
                    .unwrap_or_default();
                self.manager.restore_for_thread(thread_id, &snapshot).await;
                Ok(StateCommand::new())
            }
            Phase::RunEnd => {
                // Metadata is committed directly by the manager.
                Ok(StateCommand::new())
            }
            Phase::StepEnd => {
                let thread_id = &ctx.run_identity.thread_id;
                let has_running = self.manager.has_running(thread_id).await;
                let mut cmd = StateCommand::new();
                cmd.update::<PendingWorkKey>(has_running);
                Ok(cmd)
            }
            _ => Ok(StateCommand::new()),
        }
    }
}
