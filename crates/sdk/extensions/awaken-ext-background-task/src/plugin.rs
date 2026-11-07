// @formal entrypoint background_task.never_replay_and_attempt_exhaustion_fail_closed
// @formal entrypoint background_task.process_completion_is_retryable_and_stale_completion_is_inert
// @formal entrypoint background_task.stale_fence_never_finishes_or_heartbeats
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, HookReaction, IdBound, PhaseContext, PhaseHook, PhaseHookPoint,
    Plugin, PluginConfigError, PluginManifest,
};
use awaken_time::WallClockExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::{BackgroundTaskLifecycle, BackgroundTaskSupervisor, task_state_cell, tasks_from_state};
use awaken_agent_contract::thread::background_task::BACKGROUND_TASK_STATE_PREFIX;
use awaken_tool_contract::tool::background_task::BackgroundTaskError;

pub const BACKGROUND_TASK_PLUGIN_ID: &str = "background_task";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundTaskConfig {
    /// Canonical ids of existing ordinary tools that may be invoked in the
    /// background. This does not grant the tools or change their permission,
    /// concurrency, recovery, or resource policies.
    #[serde(default)]
    pub tools: BTreeSet<String>,
    #[serde(default)]
    pub notifications: crate::TaskNotifications,
}

impl BackgroundTaskConfig {
    /// The one semantic admission used by publication and Runtime resolution.
    pub fn validate(&self) -> Result<(), String> {
        if self
            .tools
            .iter()
            .any(|tool| tool.trim().is_empty() || crate::tools::is_management_tool(tool))
        {
            return Err("background tools must be non-empty ordinary tool ids".into());
        }
        self.notifications.validate()
    }
}

/// Authoring schema derived from the one typed configuration shape.
#[must_use]
pub fn config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(BackgroundTaskConfig))
        .expect("derived BackgroundTaskConfig schema serializes")
}

/// One static manifest for capability discovery and Runtime registration.
/// Inspecting a plugin's bound does not construct a process supervisor.
#[must_use]
pub fn background_task_manifest() -> PluginManifest {
    PluginManifest {
        id: BACKGROUND_TASK_PLUGIN_ID.into(),
        requires: Vec::new(),
        config_sections: vec![BACKGROUND_TASK_PLUGIN_ID.into()],
        bound: CapabilityBound {
            tools: IdBound::Exact(
                crate::management_tool_ids()
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            ),
            state_keys: IdBound::Namespace("background_task".into()),
            phase_hooks: vec![PhaseHookPoint::StepStart],
            ..Default::default()
        },
    }
}

pub struct BackgroundTaskPlugin {
    defaults: BackgroundTaskConfig,
    supervisor: Arc<BackgroundTaskSupervisor>,
}

impl BackgroundTaskPlugin {
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
            contributions.declare_state_key(crate::notification::RECEIPT_PREFIX);
            for tool in crate::tools::tools(config.tools, self.supervisor.clone()) {
                contributions.register_dynamic_tool(tool);
            }
            contributions.register_hook(Arc::new(ReconcileBackgroundTasks {
                supervisor: self.supervisor.clone(),
                notifications: config.notifications,
            }));
        }
        contributions
    }
}

impl Plugin for BackgroundTaskPlugin {
    fn manifest(&self) -> PluginManifest {
        background_task_manifest()
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
        config
            .validate()
            .map_err(|error| PluginConfigError::new(BACKGROUND_TASK_PLUGIN_ID, error))?;
        Ok(self.contributions(config))
    }
}

struct ReconcileBackgroundTasks {
    supervisor: Arc<BackgroundTaskSupervisor>,
    notifications: crate::TaskNotifications,
}

#[async_trait::async_trait]
impl PhaseHook for ReconcileBackgroundTasks {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::StepStart
    }

    async fn on_phase(
        &self,
        _ctx: &PhaseContext,
        _conversation: &[awaken_agent_contract::agent::message::Message],
        state: &awaken_agent_contract::agent::state::Store,
    ) -> awaken_runtime_contract::execution::Result<HookReaction> {
        // Decode the complete namespace before consulting the supervisor.
        // Corrupt durable state is an attempt error, never an empty task list.
        let tasks = tasks_from_state(state).map_err(|source| {
            awaken_runtime_contract::diagnostic::RuntimeDiagnostic::new(
                awaken_runtime_contract::diagnostic::RuntimeErrorKind::InvalidRequest,
                "background_task_state_invalid",
                source,
            )
        })?;
        let mut commands = Vec::new();
        let mut messages = Vec::new();
        for mut task in tasks {
            if task.lifecycle().is_terminal() {
                self.supervisor.retire_terminal(task.id());
                if let Some((message, receipt)) = self.notifications.message(&task, state) {
                    messages.push(message);
                    commands.push(receipt);
                }
                continue;
            }
            let committed_fence = task.tenure();
            if let Some(completion) = self
                .supervisor
                .completion_for_fold(task.id(), committed_fence.as_ref())
                && task.finish(&completion.fence, completion.end).is_ok()
            {
                if let Ok(command) = task_state_cell(task.id()).write(&task) {
                    commands.push(command);
                    if let Some((message, receipt)) = self.notifications.message(&task, state) {
                        messages.push(message);
                        commands.push(receipt);
                    }
                }
                continue;
            }
            if let Some(candidate) = self.supervisor.wait_candidate(task.id()) {
                let folded = (|| {
                    if candidate.renew_lease {
                        task.heartbeat(
                            &candidate.fence,
                            self.supervisor.clock().now_ms(),
                            BackgroundTaskSupervisor::lease_ms(),
                        )?;
                    }
                    task.wait(&candidate.fence, candidate.wait)
                })();
                match folded {
                    Ok(()) => {
                        if let Ok(command) = task_state_cell(task.id()).write(&task) {
                            commands.push(command);
                        }
                        continue;
                    }
                    Err(BackgroundTaskError::StaleFence) => {
                        if let Some(attempt) = task.attempt() {
                            self.supervisor.retire(
                                task.id(),
                                &candidate.fence,
                                &attempt.tenure(task.id()),
                            );
                        }
                    }
                    Err(_) => continue,
                }
            }
            let Some(attempt) = task.attempt() else {
                continue;
            };
            if matches!(task.lifecycle(), BackgroundTaskLifecycle::Cancelling { .. }) {
                self.supervisor
                    .cancel(task.id(), &attempt.tenure(task.id()));
            }
            if attempt.worker_id == self.supervisor.worker_id()
                && self
                    .supervisor
                    .is_active(task.id(), &attempt.tenure(task.id()))
            {
                continue;
            }
            let now = self.supervisor.clock().now_ms();
            if attempt.lease_expires_at_ms > now {
                continue;
            }
            let Ok(_) = task.reclaim(
                self.supervisor.worker_id(),
                now,
                BackgroundTaskSupervisor::lease_ms(),
            ) else {
                continue;
            };
            // The aggregate owns recovery and cancellation outcomes. In
            // particular, remote Cancelling retains its exact handle for the
            // post-commit Host observer; no local shortcut certifies termination.
            if let Ok(command) = task_state_cell(task.id()).write(&task) {
                commands.push(command);
                if let Some((message, receipt)) = self.notifications.message(&task, state) {
                    messages.push(message);
                    commands.push(receipt);
                }
            }
        }
        let mut reaction = HookReaction::state(commands);
        reaction.messages = messages;
        Ok(reaction)
    }
}
