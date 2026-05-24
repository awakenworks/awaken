use std::sync::Arc;

use crate::cancellation::CancellationToken;

use super::{BackgroundTaskManager, BackgroundTaskPlugin};

pub(crate) struct RunCancellationGuard {
    token: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for RunCancellationGuard {
    fn drop(&mut self) {
        if !self.token.is_cancelled() {
            self.handle.abort();
        }
    }
}

pub(crate) fn spawn_run_cancellation_guard(
    run_id: String,
    token: Option<CancellationToken>,
    managers: Vec<Arc<BackgroundTaskManager>>,
) -> Option<RunCancellationGuard> {
    let token = token?;
    if managers.is_empty() {
        return None;
    }

    let task_token = token.clone();
    let handle = tokio::spawn(async move {
        task_token.cancelled().await;
        // Descendant cancellation is asynchronous: the owning run can finish
        // before task metadata has reached a terminal state. Managers also
        // remember the cancelled run id so late/recursive spawns cannot escape
        // this sweep after it has started.
        let mut cancelled = 0usize;
        for manager in managers {
            cancelled += manager.cancel_descendants_for_run(&run_id).await;
        }
        if cancelled > 0 {
            tracing::debug!(
                run_id = %run_id,
                cancelled,
                "cancelled background tasks for cancelled agent run"
            );
        }
    });

    Some(RunCancellationGuard { token, handle })
}

pub(crate) fn managers_for_resolved_agent(
    agent: &crate::registry::ResolvedAgent,
) -> Vec<Arc<BackgroundTaskManager>> {
    let mut managers = Vec::new();
    if let Some(manager) = &agent.background_manager {
        managers.push(manager.clone());
    }
    for plugin in &agent.env.plugins {
        if let Some(plugin) = plugin
            .as_ref()
            .as_any()
            .downcast_ref::<BackgroundTaskPlugin>()
        {
            managers.push(plugin.manager().clone());
        }
    }
    dedup_managers(managers)
}

pub(crate) fn dedup_managers(
    managers: Vec<Arc<BackgroundTaskManager>>,
) -> Vec<Arc<BackgroundTaskManager>> {
    let mut deduped = Vec::new();
    for manager in managers {
        if !deduped
            .iter()
            .any(|existing| Arc::ptr_eq(existing, &manager))
        {
            deduped.push(manager);
        }
    }
    deduped
}
