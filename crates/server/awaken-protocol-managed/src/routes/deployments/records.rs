//! Persisted Deployment and DeploymentRun projections owned by DeploymentState.

use super::*;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct DeploymentRecord {
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) workspace_id: String,
    pub(super) agent: AgentReference,
    pub(super) environment_id: String,
    pub(super) name: String,
    pub(super) description: Option<String>,
    pub(super) metadata: BTreeMap<String, String>,
    pub(super) initial_events: Vec<DeploymentInitialEvent>,
    pub(super) resources: Vec<ResourceInput>,
    pub(super) schedule: Option<Schedule>,
    pub(super) vault_ids: Vec<String>,
    /// `"active"` | `"paused"`.
    pub(super) status: String,
    pub(super) paused_reason: Option<PausedReason>,
    pub(super) archived_at: Option<String>,
    /// RFC 3339 of the schedule's last fire (echoed into the schedule object).
    pub(super) last_run_at: Option<String>,
    /// The next scheduled fire instant (epoch ms); lazily seeded on the first tick
    /// so a just-created deployment doesn't fire retroactively.
    pub(super) next_fire_ms: Option<u64>,
}

impl DeploymentRecord {
    pub(super) fn project(&self, id: &str) -> Deployment {
        Deployment {
            id: id.to_string(),
            object_type: "deployment",
            agent: self.agent.clone(),
            archived_at: self.archived_at.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            description: self.description.clone(),
            environment_id: self.environment_id.clone(),
            initial_events: self.initial_events.clone(),
            metadata: self.metadata.clone(),
            name: self.name.clone(),
            paused_reason: self.paused_reason.clone(),
            resources: self.resources.clone(),
            schedule: self.projected_schedule(),
            status: if self.status == "active" {
                "active"
            } else {
                "paused"
            },
            vault_ids: self.vault_ids.clone(),
        }
    }

    /// The schedule object echoed back, with `last_run_at` reflecting the most
    /// recent fire (the stored expression/timezone pass through unchanged).
    fn projected_schedule(&self) -> Option<Schedule> {
        self.schedule.as_ref().map(|schedule| {
            let upcoming = if self.archived_at.is_some() {
                Vec::new()
            } else {
                upcoming_occurrences(schedule, now_ms())
            };
            schedule.with_runtime(self.last_run_at.clone(), upcoming)
        })
    }

    /// The parsed cron for an active, non-archived deployment.
    pub(super) fn active_cron(&self) -> Option<(crate::cron::Cron, Tz)> {
        if self.status != "active" || self.archived_at.is_some() {
            return None;
        }
        parsed_schedule(self.schedule.as_ref()?)
    }

    pub(super) fn launch(&self, deployment_id: &str, deployment_run_id: &str) -> DeploymentLaunch {
        DeploymentLaunch {
            deployment_id: deployment_id.to_string(),
            deployment_run_id: deployment_run_id.to_string(),
            workspace_id: self.workspace_id.clone(),
            agent: self.agent.clone(),
            environment_id: self.environment_id.clone(),
            metadata: self.metadata.clone(),
            initial_events: self.initial_events.clone(),
            resources: self.resources.clone(),
            vault_ids: self.vault_ids.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct RunRecord {
    pub(super) created_at: String,
    pub(super) deployment_id: String,
    pub(super) workspace_id: String,
    pub(super) agent: AgentReference,
    pub(super) trigger: TriggerContext,
    pub(super) session_id: Option<String>,
    pub(super) error: Option<RunError>,
}

impl RunRecord {
    pub(super) fn project(&self, id: &str) -> DeploymentRun {
        DeploymentRun {
            id: id.to_string(),
            object_type: "deployment_run",
            agent: self.agent.clone(),
            created_at: self.created_at.clone(),
            deployment_id: self.deployment_id.clone(),
            error: self.error.clone(),
            session_id: self.session_id.clone(),
            trigger_context: self.trigger.clone(),
        }
    }
}
