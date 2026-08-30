use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, HookReaction, IdBound, PhaseContext, PhaseHook, PhaseHookPoint,
    Plugin, PluginConfigError, PluginManifest,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::BACKGROUND_TASK_STATE_PREFIX;
use crate::tools::{CancelBackgroundTask, GetBackgroundTask, ListBackgroundTasks, RunInBackground};
use crate::{
    BackgroundTaskError, BackgroundTaskLifecycle, BackgroundTaskSupervisor, TaskClaim,
    task_state_cell, tasks_from_state,
};

pub const BACKGROUND_TASK_PLUGIN_ID: &str = "background_task";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundTaskConfig {
    /// Canonical ids of existing ordinary tools that may be invoked in the
    /// background. This does not grant the tools or change their permission,
    /// concurrency, recovery, or resource policies.
    #[serde(default)]
    pub tools: BTreeSet<String>,
}

/// Authoring schema derived from the one typed configuration shape.
#[must_use]
pub fn config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(BackgroundTaskConfig))
        .expect("derived BackgroundTaskConfig schema serializes")
}

pub struct BackgroundTaskPlugin {
    defaults: BackgroundTaskConfig,
    supervisor: Arc<BackgroundTaskSupervisor>,
}

impl BackgroundTaskPlugin {
    #[must_use]
    pub fn new(defaults: BackgroundTaskConfig) -> Self {
        Self::with_supervisor(defaults, crate::process_supervisor())
    }

    #[must_use]
    pub fn with_supervisor(
        defaults: BackgroundTaskConfig,
        supervisor: Arc<BackgroundTaskSupervisor>,
    ) -> Self {
        Self {
            defaults,
            supervisor,
        }
    }

    #[must_use]
    pub fn supervisor(&self) -> Arc<BackgroundTaskSupervisor> {
        self.supervisor.clone()
    }

    fn contributions(&self, config: BackgroundTaskConfig) -> Contributions {
        let mut contributions = Contributions::new(BACKGROUND_TASK_PLUGIN_ID);
        if !config.tools.is_empty() {
            contributions.declare_state_key(BACKGROUND_TASK_STATE_PREFIX);
            for tool in crate::tools::tools(config.tools, self.supervisor.clone()) {
                contributions.register_dynamic_tool(tool);
            }
            contributions.register_hook(Arc::new(ReconcileBackgroundTasks {
                supervisor: self.supervisor.clone(),
            }));
        }
        contributions
    }
}

impl Plugin for BackgroundTaskPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: BACKGROUND_TASK_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: vec![BACKGROUND_TASK_PLUGIN_ID.into()],
            bound: CapabilityBound {
                tools: IdBound::Exact(vec![
                    RunInBackground::ID.into(),
                    ListBackgroundTasks::ID.into(),
                    GetBackgroundTask::ID.into(),
                    CancelBackgroundTask::ID.into(),
                ]),
                state_keys: IdBound::Namespace(BACKGROUND_TASK_STATE_PREFIX.into()),
                phase_hooks: vec![PhaseHookPoint::StepStart],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        self.contributions(self.defaults.clone())
    }

    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let config = config
            .map(BackgroundTaskConfig::deserialize)
            .transpose()
            .map_err(|error| PluginConfigError::new(BACKGROUND_TASK_PLUGIN_ID, error.to_string()))?
            .unwrap_or_else(|| self.defaults.clone());
        if config
            .tools
            .iter()
            .any(|tool| tool.trim().is_empty() || crate::tools::is_management_tool(tool))
        {
            return Err(PluginConfigError::new(
                BACKGROUND_TASK_PLUGIN_ID,
                "background tools must be non-empty ordinary tool ids",
            ));
        }
        Ok(self.contributions(config))
    }
}

struct ReconcileBackgroundTasks {
    supervisor: Arc<BackgroundTaskSupervisor>,
}

#[async_trait::async_trait]
impl PhaseHook for ReconcileBackgroundTasks {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::StepStart
    }

    async fn on_phase(
        &self,
        _ctx: &PhaseContext,
        _conversation: &[awaken_runtime_contract::Message],
        state: &awaken_runtime_contract::Store,
    ) -> HookReaction {
        let Ok(tasks) = tasks_from_state(state) else {
            return HookReaction::default();
        };
        let mut commands = Vec::new();
        for mut task in tasks {
            if let Some(completion) = self.supervisor.completion(&task.id) {
                match task.finish(&completion.fence, completion.end) {
                    Ok(()) => {
                        if let Ok(command) = task_state_cell(&task.id).write(&task) {
                            commands.push(command);
                        }
                        continue;
                    }
                    Err(BackgroundTaskError::StaleFence) => {
                        // Another worker epoch is now durable. Retire only the
                        // stale process projection and continue reconciling the
                        // authoritative attempt; otherwise stale completion
                        // would permanently suppress its post-commit launch.
                        self.supervisor.retire(&task.id);
                    }
                    Err(_) => continue,
                }
            }
            if matches!(task.lifecycle, BackgroundTaskLifecycle::Cancelling { .. }) {
                self.supervisor.cancel(&task.id);
            }
            let Some(attempt) = task.attempt() else {
                continue;
            };
            if attempt.worker_id == self.supervisor.worker_id()
                && self.supervisor.is_active(&task.id)
            {
                continue;
            }
            let now = BackgroundTaskSupervisor::now_ms();
            if attempt.lease_expires_at_ms > now {
                continue;
            }
            let was_cancelling =
                matches!(task.lifecycle, BackgroundTaskLifecycle::Cancelling { .. });
            let Ok(claim) = task.reclaim(
                self.supervisor.worker_id(),
                now,
                BackgroundTaskSupervisor::lease_ms(),
            ) else {
                continue;
            };
            if let TaskClaim::Acquired(fence) = claim
                && was_cancelling
                && task.request_cancel().is_ok()
            {
                let _ = task.finish(&fence, crate::BackgroundTaskEnd::Cancelled);
            }
            if let Ok(command) = task_state_cell(&task.id).write(&task) {
                commands.push(command);
            }
        }
        HookReaction::state(commands)
    }
}
