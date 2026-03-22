use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::model::Phase;

use crate::runtime::{PhaseContext, PhaseHook};
use crate::state::StateCommand;

use super::manager::BackgroundTaskManager;
use super::state::{BackgroundTaskStateAction, BackgroundTaskStateKey};

/// Phase hook that syncs background task metadata into the persisted state.
///
/// Registered for both `RunStart` (restore from persisted state) and
/// `RunEnd` (persist current task state).
pub(crate) struct BackgroundTaskSyncHook {
    pub(crate) manager: Arc<BackgroundTaskManager>,
}

#[async_trait::async_trait]
impl PhaseHook for BackgroundTaskSyncHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        match ctx.phase {
            Phase::RunStart => {
                let thread_id = &ctx.run_input.identity.thread_id;
                let snapshot = ctx
                    .state::<BackgroundTaskStateKey>()
                    .cloned()
                    .unwrap_or_default();
                self.manager.restore_for_thread(thread_id, &snapshot).await;
                Ok(StateCommand::new())
            }
            Phase::RunEnd => {
                let persisted = self.manager.persisted_snapshot().await;
                let mut cmd = StateCommand::new();
                cmd.update::<BackgroundTaskStateKey>(BackgroundTaskStateAction::ReplaceAll {
                    tasks: persisted,
                });
                Ok(cmd)
            }
            _ => Ok(StateCommand::new()),
        }
    }
}
