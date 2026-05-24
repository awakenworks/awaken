use std::sync::Arc;

use crate::extensions::background::BackgroundTaskManager;
use crate::registry::ResolvedExecution;

use super::compaction::CompactionRuntime;

pub(super) fn managers_for_run(
    resolved_execution: &ResolvedExecution,
    compaction_runtime: Option<&CompactionRuntime>,
) -> Vec<Arc<BackgroundTaskManager>> {
    let mut managers = Vec::new();
    if let ResolvedExecution::Local(agent) = resolved_execution {
        managers.extend(crate::extensions::background::managers_for_resolved_agent(
            agent,
        ));
    }
    if let Some(runtime) = compaction_runtime {
        managers.push(runtime.manager.clone());
    }
    crate::extensions::background::dedup_managers(managers)
}
