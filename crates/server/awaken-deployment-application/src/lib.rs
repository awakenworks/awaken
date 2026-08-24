//! Coordinator-owned Deployment aggregate, scheduling, and Session launch orchestration.
//!
//! Public protocol adapters translate their wire DTOs into the commands in this
//! crate. Repository adapters persist opaque encodings of these application
//! records, so neither storage nor this owner depends on Axum or Managed DTOs.

mod scheduler;

pub use awaken_deployment_contract::{
    AgentSelector, CreateDeploymentCommand, DeploymentAgent, DeploymentLaunch,
    DeploymentLaunchOutcome, DeploymentOutcomeRubric, DeploymentPauseError, DeploymentPauseReason,
    DeploymentRecord, DeploymentRepositoryCheckout, DeploymentResource, DeploymentRunFailure,
    DeploymentRunRecord, DeploymentRunView, DeploymentSchedule, DeploymentSeedEvent,
    DeploymentStatus, DeploymentTrigger, DeploymentView, FieldUpdate, MetadataUpdate,
    UpdateDeploymentCommand,
};

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_deployment_contract::{
    AgentArchiveCascade, DeploymentLifecycleFact, DeploymentRepository, DeploymentRepositoryError,
    DeploymentWriteOutcome, MAX_DEPLOYMENT_REVISION,
};
use awaken_executable_agent_contract::{
    ExecutableAgentRegistrationError, ExecutableAgentRegistrationSource,
};
use scheduler::{next_occurrence, validate_schedule};

#[cfg(test)]
use awaken_deployment_contract::ScheduledRunClaimOutcome;
#[cfg(test)]
use scheduler::{MAX_JITTER_BOUND_MS, execution_jitter_ms};

const DEFAULT_SCHEDULED_LIMIT: usize = 1_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn timestamp(milliseconds: u64) -> String {
    awaken_session_contract::epoch_millis_to_rfc3339(milliseconds)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeploymentApplicationError {
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("archived deployment is terminal and cannot be modified")]
    Terminal,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
}

impl From<DeploymentRepositoryError> for DeploymentApplicationError {
    fn from(value: DeploymentRepositoryError) -> Self {
        Self::Unavailable(value.to_string())
    }
}

#[async_trait]
pub trait DeploymentSessionLauncher: Send + Sync {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome;
}

/// One authoritative Deployment application instance. Its maps are synchronized
/// working projections; the injected repository is restart and replica truth.
pub struct DeploymentApplication {
    deployments: Mutex<BTreeMap<String, DeploymentRecord>>,
    runs: Mutex<BTreeMap<String, DeploymentRunRecord>>,
    launcher: Mutex<Option<Arc<dyn DeploymentSessionLauncher>>>,
    lifecycle_notifier: Mutex<Option<Arc<dyn awaken_session_contract::LifecycleFactNotifier>>>,
    repository: Option<Arc<dyn DeploymentRepository>>,
    executable_agents: Mutex<Option<Arc<dyn ExecutableAgentRegistrationSource>>>,
    executable_projection_refresh:
        Mutex<Option<Arc<dyn awaken_session_contract::ExecutableProjectionRefresh>>>,
    scheduled_limit: usize,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for DeploymentApplication {
    fn default() -> Self {
        Self::new()
    }
}

impl DeploymentApplication {
    /// Disposable fixture constructor. Product composition must inject a durable
    /// repository through [`Self::from_repository`].
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn new() -> Self {
        Self {
            deployments: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(BTreeMap::new()),
            launcher: Mutex::new(None),
            lifecycle_notifier: Mutex::new(None),
            repository: None,
            executable_agents: Mutex::new(None),
            executable_projection_refresh: Mutex::new(None),
            scheduled_limit: DEFAULT_SCHEDULED_LIMIT,
        }
    }

    /// Restore the scheduling aggregate and append-only run history.
    pub async fn from_repository(
        repository: Arc<dyn DeploymentRepository>,
    ) -> Result<Self, DeploymentApplicationError> {
        let (deployments, runs) = load_projection(repository.as_ref()).await?;
        Ok(Self {
            deployments: Mutex::new(deployments),
            runs: Mutex::new(runs),
            launcher: Mutex::new(None),
            lifecycle_notifier: Mutex::new(None),
            repository: Some(repository),
            executable_agents: Mutex::new(None),
            executable_projection_refresh: Mutex::new(None),
            scheduled_limit: DEFAULT_SCHEDULED_LIMIT,
        })
    }

    pub fn bind_launcher(&self, launcher: Arc<dyn DeploymentSessionLauncher>) {
        *self.launcher.lock().expect("Deployment launcher lock") = Some(launcher);
    }

    /// Bind the same payload-free wake used by the Session lifecycle outbox.
    /// Deployment facts remain repository truth; this only avoids waiting for
    /// the periodic reconciliation interval after a successful transaction.
    pub fn bind_lifecycle_notifier(
        &self,
        notifier: Arc<dyn awaken_session_contract::LifecycleFactNotifier>,
    ) {
        *self
            .lifecycle_notifier
            .lock()
            .expect("Deployment lifecycle notifier lock") = Some(notifier);
    }

    pub(crate) fn notify_lifecycle(&self) {
        if let Some(notifier) = self
            .lifecycle_notifier
            .lock()
            .expect("Deployment lifecycle notifier lock")
            .as_ref()
        {
            notifier.notify();
        }
    }

    pub fn bind_executable_agents(&self, source: Arc<dyn ExecutableAgentRegistrationSource>) {
        *self
            .executable_agents
            .lock()
            .expect("executable Agent source lock") = Some(source);
    }

    pub fn bind_executable_projection_refresh(
        &self,
        refresh: Arc<dyn awaken_session_contract::ExecutableProjectionRefresh>,
    ) {
        *self
            .executable_projection_refresh
            .lock()
            .expect("executable projection refresh lock") = Some(refresh);
    }

    async fn refresh_executable_projections(&self) -> Result<(), DeploymentApplicationError> {
        let refresh = self
            .executable_projection_refresh
            .lock()
            .expect("executable projection refresh lock")
            .clone();
        match refresh {
            Some(refresh) => refresh
                .refresh()
                .await
                .map_err(DeploymentApplicationError::Unavailable),
            None => Ok(()),
        }
    }

    async fn refresh(&self) -> Result<(), DeploymentApplicationError> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        let (deployments, runs) = load_projection(repository.as_ref()).await?;
        self.deployments
            .lock()
            .expect("Deployment projection lock")
            .extend(deployments);
        self.runs
            .lock()
            .expect("DeploymentRun projection lock")
            .extend(runs);
        Ok(())
    }

    async fn resolve_agent(
        &self,
        workspace_id: &str,
        selector: &AgentSelector,
    ) -> Result<DeploymentAgent, DeploymentApplicationError> {
        if selector.id.trim().is_empty() || selector.version == Some(0) {
            return Err(DeploymentApplicationError::Invalid(
                "deployment Agent id must be non-empty and version must be at least 1".into(),
            ));
        }
        let source = self
            .executable_agents
            .lock()
            .expect("executable Agent source lock")
            .clone();
        let Some(source) = source else {
            return Ok(DeploymentAgent::new(
                selector.id.clone(),
                selector.version.unwrap_or(1),
            ));
        };
        let selected = match selector.version {
            Some(version) => {
                source
                    .registration_at_revision(workspace_id, &selector.id, version)
                    .await
            }
            None => {
                source
                    .current_registration(workspace_id, &selector.id)
                    .await
            }
        }
        .map_err(agent_error)?
        .ok_or(DeploymentApplicationError::NotFound("agent"))?;
        Ok(DeploymentAgent::new(
            selected.agent_id,
            selected.source_revision,
        ))
    }

    pub async fn create(
        &self,
        command: CreateDeploymentCommand,
    ) -> Result<DeploymentView, DeploymentApplicationError> {
        self.refresh().await?;
        validate_schedule(command.schedule.as_ref())?;
        self.refresh_executable_projections().await?;
        let agent = self
            .resolve_agent(&command.workspace_id, &command.agent)
            .await?;
        let now = now_ms();
        let record = DeploymentRecord {
            revision: 0,
            created_at: timestamp(now),
            updated_at: timestamp(now),
            workspace_id: command.workspace_id,
            agent,
            environment_id: command.environment_id,
            name: command.name,
            description: command.description,
            metadata: command.metadata,
            initial_events: command.initial_events,
            resources: command.resources,
            next_fire_ms: command
                .schedule
                .as_ref()
                .and_then(|schedule| next_occurrence(schedule, now)),
            schedule: command.schedule,
            vault_ids: command.vault_ids,
            budget_max_list_cost_minor: command.budget_max_list_cost_minor,
            status: DeploymentStatus::Active,
            paused_reason: None,
            archived_at: None,
            last_run_at: None,
        };
        validate_record(&record)?;
        {
            let deployments = self.deployments.lock().expect("Deployment projection lock");
            if record.schedule.is_some() {
                ensure_scheduled_capacity(&deployments, self.scheduled_limit)?;
            }
        }
        let id = format!("depl_{}", uuid::Uuid::new_v4().simple());
        match self
            .persist_deployment(&id, &record, None, "deployment.created")
            .await?
        {
            DeploymentWriteOutcome::Applied => {}
            DeploymentWriteOutcome::Conflict => {
                return Err(DeploymentApplicationError::Conflict(
                    "Deployment identity collision; retry create".into(),
                ));
            }
            DeploymentWriteOutcome::ScheduledCapacityReached => {
                return Err(scheduled_capacity_error(self.scheduled_limit));
            }
        }
        self.deployments
            .lock()
            .expect("Deployment projection lock")
            .insert(id.clone(), record.clone());
        Ok(DeploymentView { id, record })
    }

    pub async fn get(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<DeploymentView, DeploymentApplicationError> {
        self.refresh().await?;
        self.deployments
            .lock()
            .expect("Deployment projection lock")
            .get(id)
            .filter(|record| record.workspace_id == workspace_id)
            .cloned()
            .map(|record| DeploymentView {
                id: id.to_string(),
                record,
            })
            .ok_or(DeploymentApplicationError::NotFound("deployment"))
    }

    pub async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<DeploymentView>, DeploymentApplicationError> {
        self.refresh().await?;
        Ok(self
            .deployments
            .lock()
            .expect("Deployment projection lock")
            .iter()
            .filter(|(_, record)| record.workspace_id == workspace_id)
            .map(|(id, record)| DeploymentView {
                id: id.clone(),
                record: record.clone(),
            })
            .collect())
    }

    pub async fn update(
        &self,
        workspace_id: &str,
        id: &str,
        command: UpdateDeploymentCommand,
    ) -> Result<DeploymentView, DeploymentApplicationError> {
        self.refresh().await?;
        if let Some(FieldUpdate::Replace(schedule)) = &command.schedule {
            validate_schedule(Some(schedule))?;
        }
        let current = self.get_cached(workspace_id, id)?;
        let mut candidate = current.clone();
        if candidate.archived_at.is_some() {
            return Err(DeploymentApplicationError::Terminal);
        }
        if command.agent.is_some() {
            self.refresh_executable_projections().await?;
        }
        if let Some(agent) = command.agent {
            candidate.agent = self.resolve_agent(workspace_id, &agent).await?;
        }
        if let Some(value) = command.environment_id {
            candidate.environment_id = value;
        }
        if let Some(value) = command.name {
            candidate.name = value;
        }
        if let Some(change) = command.description {
            candidate.description = match change {
                FieldUpdate::Clear => None,
                FieldUpdate::Replace(value) => Some(value),
            };
        }
        if let Some(change) = command.metadata {
            match change {
                MetadataUpdate::Clear => candidate.metadata.clear(),
                MetadataUpdate::Patch(patch) => {
                    for (key, value) in patch {
                        if let Some(value) = value {
                            candidate.metadata.insert(key, value);
                        } else {
                            candidate.metadata.remove(&key);
                        }
                    }
                }
            }
        }
        if let Some(value) = command.initial_events {
            candidate.initial_events = value;
        }
        if let Some(change) = command.resources {
            candidate.resources = match change {
                FieldUpdate::Clear => Vec::new(),
                FieldUpdate::Replace(value) => value,
            };
        }
        if let Some(change) = command.schedule {
            candidate.schedule = match change {
                FieldUpdate::Clear => None,
                FieldUpdate::Replace(value) => Some(value),
            };
            candidate.next_fire_ms = candidate
                .schedule
                .as_ref()
                .and_then(|schedule| next_occurrence(schedule, now_ms()));
        }
        if let Some(change) = command.vault_ids {
            candidate.vault_ids = match change {
                FieldUpdate::Clear => Vec::new(),
                FieldUpdate::Replace(value) => value,
            };
        }
        if let Some(change) = command.budget_max_list_cost_minor {
            candidate.budget_max_list_cost_minor = match change {
                FieldUpdate::Clear => None,
                FieldUpdate::Replace(value) => Some(value),
            };
        }
        validate_record(&candidate)?;
        let current_had_schedule = current.schedule.is_some();
        if !current_had_schedule && candidate.schedule.is_some() {
            ensure_scheduled_capacity(
                &self.deployments.lock().expect("Deployment projection lock"),
                self.scheduled_limit,
            )?;
        }
        candidate.updated_at = timestamp(now_ms());
        candidate.revision = next_revision(current.revision)?;
        apply_write_outcome(
            self.persist_deployment(id, &candidate, Some(current.revision), "deployment.updated")
                .await?,
            self.scheduled_limit,
        )?;
        self.deployments
            .lock()
            .expect("Deployment projection lock")
            .insert(id.to_string(), candidate.clone());
        Ok(DeploymentView {
            id: id.to_string(),
            record: candidate,
        })
    }

    pub async fn archive(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<DeploymentView, DeploymentApplicationError> {
        self.refresh().await?;
        let mut record = self.get_cached(workspace_id, id)?;
        if record.archived_at.is_none() {
            let expected_revision = record.revision;
            let now = now_ms();
            record.archived_at = Some(timestamp(now));
            record.updated_at = timestamp(now);
            record.revision = next_revision(expected_revision)?;
            apply_write_outcome(
                self.persist_deployment(
                    id,
                    &record,
                    Some(expected_revision),
                    "deployment.archived",
                )
                .await?,
                self.scheduled_limit,
            )?;
            self.deployments
                .lock()
                .expect("Deployment projection lock")
                .insert(id.to_string(), record.clone());
        }
        Ok(DeploymentView {
            id: id.to_string(),
            record,
        })
    }

    pub async fn pause(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<DeploymentView, DeploymentApplicationError> {
        self.transition_pause(workspace_id, id, true).await
    }

    pub async fn unpause(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<DeploymentView, DeploymentApplicationError> {
        self.transition_pause(workspace_id, id, false).await
    }

    async fn transition_pause(
        &self,
        workspace_id: &str,
        id: &str,
        pause: bool,
    ) -> Result<DeploymentView, DeploymentApplicationError> {
        self.refresh().await?;
        let mut record = self.get_cached(workspace_id, id)?;
        if record.archived_at.is_some() {
            return Err(DeploymentApplicationError::Terminal);
        }
        let now = now_ms();
        let expected_revision = record.revision;
        let event = if pause {
            record.status = DeploymentStatus::Paused;
            record.paused_reason = Some(DeploymentPauseReason::Manual);
            "deployment.paused"
        } else {
            record.status = DeploymentStatus::Active;
            record.paused_reason = None;
            record.next_fire_ms = record
                .schedule
                .as_ref()
                .and_then(|schedule| next_occurrence(schedule, now));
            "deployment.unpaused"
        };
        record.updated_at = timestamp(now);
        record.revision = next_revision(expected_revision)?;
        apply_write_outcome(
            self.persist_deployment(id, &record, Some(expected_revision), event)
                .await?,
            self.scheduled_limit,
        )?;
        self.deployments
            .lock()
            .expect("Deployment projection lock")
            .insert(id.to_string(), record.clone());
        Ok(DeploymentView {
            id: id.to_string(),
            record,
        })
    }

    pub async fn run(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<DeploymentRunView, DeploymentApplicationError> {
        self.refresh().await?;
        let deployment = self.get_cached(workspace_id, id)?;
        if deployment.archived_at.is_some() {
            return Err(DeploymentApplicationError::Terminal);
        }
        self.refresh_executable_projections().await?;
        let run_id = format!("drun_{}", uuid::Uuid::new_v4().simple());
        let launch = launch_for(&deployment, id, &run_id);
        let run = DeploymentRunRecord {
            created_at: timestamp(now_ms()),
            deployment_id: id.to_string(),
            workspace_id: workspace_id.to_string(),
            agent: deployment.agent.clone(),
            trigger: DeploymentTrigger::Manual,
            session_id: None,
            error: None,
        };
        self.persist_run(&run_id, &run, None).await?;
        self.runs
            .lock()
            .expect("DeploymentRun projection lock")
            .insert(run_id.clone(), run);
        self.launch_run(&run_id, launch).await
    }

    pub async fn tick_and_launch(
        &self,
        now: u64,
    ) -> Result<Vec<DeploymentRunView>, DeploymentApplicationError> {
        self.tick_and_launch_scheduled(now).await
    }

    pub async fn get_run(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<DeploymentRunView, DeploymentApplicationError> {
        self.refresh().await?;
        self.runs
            .lock()
            .expect("DeploymentRun projection lock")
            .get(id)
            .filter(|record| record.workspace_id == workspace_id)
            .cloned()
            .map(|record| DeploymentRunView {
                id: id.to_string(),
                record,
            })
            .ok_or(DeploymentApplicationError::NotFound("deployment_run"))
    }

    pub async fn list_runs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<DeploymentRunView>, DeploymentApplicationError> {
        self.refresh().await?;
        Ok(self
            .runs
            .lock()
            .expect("DeploymentRun projection lock")
            .iter()
            .filter(|(_, record)| record.workspace_id == workspace_id)
            .map(|(id, record)| DeploymentRunView {
                id: id.clone(),
                record: record.clone(),
            })
            .collect())
    }

    pub async fn archive_for_agent(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<usize, DeploymentApplicationError> {
        self.refresh().await?;
        let now = timestamp(now_ms());
        let candidates: Vec<(String, DeploymentRecord)> = self
            .deployments
            .lock()
            .expect("Deployment projection lock")
            .iter()
            .filter(|(_, record)| {
                record.archived_at.is_none()
                    && record.workspace_id == workspace_id
                    && record.agent.id == agent_id
            })
            .map(|(id, record)| {
                let mut candidate = record.clone();
                candidate.archived_at = Some(now.clone());
                candidate.updated_at = now.clone();
                candidate.revision = next_revision(record.revision)?;
                Ok((id.clone(), candidate))
            })
            .collect::<Result<_, DeploymentApplicationError>>()?;
        for (id, candidate) in &candidates {
            let expected = candidate.revision.checked_sub(1).ok_or_else(|| {
                DeploymentApplicationError::Conflict("Deployment revision did not advance".into())
            })?;
            apply_write_outcome(
                self.persist_deployment(id, candidate, Some(expected), "deployment.archived")
                    .await?,
                self.scheduled_limit,
            )?;
        }
        let mut deployments = self.deployments.lock().expect("Deployment projection lock");
        for (id, candidate) in &candidates {
            deployments.insert(id.clone(), candidate.clone());
        }
        Ok(candidates.len())
    }

    async fn launch_run(
        &self,
        run_id: &str,
        launch: DeploymentLaunch,
    ) -> Result<DeploymentRunView, DeploymentApplicationError> {
        let launcher = self
            .launcher
            .lock()
            .expect("Deployment launcher lock")
            .clone()
            .ok_or_else(|| {
                DeploymentApplicationError::Unavailable(
                    "Deployment Session launcher is not configured".into(),
                )
            })?;
        let outcome = launcher.launch(launch).await;
        let DeploymentLaunchOutcome::Unavailable { message } = &outcome else {
            return self.finish_run(run_id, outcome).await;
        };
        Err(DeploymentApplicationError::Unavailable(format!(
            "Deployment Session launch remains pending: {message}"
        )))
    }

    async fn finish_run(
        &self,
        run_id: &str,
        outcome: DeploymentLaunchOutcome,
    ) -> Result<DeploymentRunView, DeploymentApplicationError> {
        let mut run = self
            .runs
            .lock()
            .expect("DeploymentRun projection lock")
            .get(run_id)
            .cloned()
            .ok_or(DeploymentApplicationError::NotFound("deployment_run"))?;
        match outcome {
            DeploymentLaunchOutcome::Created { session_id } => {
                run.session_id = Some(session_id);
                run.error = None;
            }
            DeploymentLaunchOutcome::Failed { error } => {
                run.session_id = None;
                run.error = Some(error);
            }
            DeploymentLaunchOutcome::Unavailable { .. } => {
                return Err(DeploymentApplicationError::Invalid(
                    "indeterminate launch cannot be committed as terminal".into(),
                ));
            }
        }
        let event = if run.error.is_some() {
            "deployment_run.failed"
        } else {
            "deployment_run.succeeded"
        };
        let lifecycle = matches!(run.trigger, DeploymentTrigger::Schedule { .. }).then(|| {
            lifecycle_fact(
                format!("deployment_run:{run_id}:{event}"),
                run_id,
                &run.workspace_id,
                event,
            )
        });
        self.persist_run(run_id, &run, lifecycle).await?;
        self.runs
            .lock()
            .expect("DeploymentRun projection lock")
            .insert(run_id.to_string(), run.clone());

        if matches!(run.trigger, DeploymentTrigger::Schedule { .. })
            && let Some(error) = run
                .error
                .as_ref()
                .and_then(DeploymentRunFailure::pause_error)
        {
            let mut deployment = self
                .deployments
                .lock()
                .expect("Deployment projection lock")
                .get(&run.deployment_id)
                .cloned()
                .ok_or(DeploymentApplicationError::NotFound("deployment"))?;
            deployment.status = DeploymentStatus::Paused;
            deployment.paused_reason = Some(DeploymentPauseReason::Error { error });
            deployment.updated_at = timestamp(now_ms());
            let expected_revision = deployment.revision;
            deployment.revision = next_revision(expected_revision)?;
            apply_write_outcome(
                self.persist_deployment(
                    &run.deployment_id,
                    &deployment,
                    Some(expected_revision),
                    "deployment.paused",
                )
                .await?,
                self.scheduled_limit,
            )?;
            self.deployments
                .lock()
                .expect("Deployment projection lock")
                .insert(run.deployment_id.clone(), deployment);
        }
        Ok(DeploymentRunView {
            id: run_id.to_string(),
            record: run,
        })
    }

    fn get_cached(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<DeploymentRecord, DeploymentApplicationError> {
        self.deployments
            .lock()
            .expect("Deployment projection lock")
            .get(id)
            .filter(|record| record.workspace_id == workspace_id)
            .cloned()
            .ok_or(DeploymentApplicationError::NotFound("deployment"))
    }

    async fn primary_agent_missing(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<bool, DeploymentApplicationError> {
        let source = self
            .executable_agents
            .lock()
            .expect("executable Agent source lock")
            .clone();
        let Some(source) = source else {
            return Ok(false);
        };
        source
            .current_registration(workspace_id, agent_id)
            .await
            .map(|registration| registration.is_none())
            .map_err(agent_error)
    }

    async fn persist_deployment(
        &self,
        id: &str,
        record: &DeploymentRecord,
        expected_revision: Option<u64>,
        event: &str,
    ) -> Result<DeploymentWriteOutcome, DeploymentApplicationError> {
        let Some(repository) = &self.repository else {
            return Ok(DeploymentWriteOutcome::Applied);
        };
        let outcome = repository
            .write_deployment(
                stored_deployment(id, record)?,
                expected_revision,
                self.scheduled_limit,
                Some(lifecycle_fact(
                    format!("deployment:{id}:{event}:{}", record.revision),
                    id,
                    &record.workspace_id,
                    event,
                )),
            )
            .await?;
        if outcome == DeploymentWriteOutcome::Applied {
            self.notify_lifecycle();
        }
        Ok(outcome)
    }

    async fn persist_run(
        &self,
        id: &str,
        record: &DeploymentRunRecord,
        lifecycle: Option<DeploymentLifecycleFact>,
    ) -> Result<(), DeploymentApplicationError> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        let notify = lifecycle.is_some();
        repository
            .upsert_deployment_run(stored_run(id, record)?, lifecycle)
            .await?;
        if notify {
            self.notify_lifecycle();
        }
        Ok(())
    }

    #[cfg(test)]
    fn with_scheduled_limit(limit: usize) -> Self {
        Self {
            scheduled_limit: limit,
            ..Self::new()
        }
    }
}

#[async_trait]
impl AgentArchiveCascade for DeploymentApplication {
    async fn archive_agent_dependents(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<(), String> {
        self.archive_for_agent(workspace_id, agent_id)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn agent_error(error: ExecutableAgentRegistrationError) -> DeploymentApplicationError {
    match error {
        ExecutableAgentRegistrationError::Invalid(message) => {
            DeploymentApplicationError::Invalid(message)
        }
        ExecutableAgentRegistrationError::Conflict(message) => {
            DeploymentApplicationError::Conflict(message)
        }
        ExecutableAgentRegistrationError::Unavailable(message)
        | ExecutableAgentRegistrationError::Storage(message) => {
            DeploymentApplicationError::Unavailable(message)
        }
    }
}

fn next_revision(revision: u64) -> Result<u64, DeploymentApplicationError> {
    (revision < MAX_DEPLOYMENT_REVISION)
        .then_some(revision + 1)
        .ok_or_else(|| {
            DeploymentApplicationError::Conflict(
                "Deployment durable revision exhausted; mutation rejected fail-closed".into(),
            )
        })
}

fn scheduled_capacity_error(limit: usize) -> DeploymentApplicationError {
    DeploymentApplicationError::Invalid(format!(
        "an organization supports at most {limit} scheduled deployments"
    ))
}

fn apply_write_outcome(
    outcome: DeploymentWriteOutcome,
    scheduled_limit: usize,
) -> Result<(), DeploymentApplicationError> {
    match outcome {
        DeploymentWriteOutcome::Applied => Ok(()),
        DeploymentWriteOutcome::Conflict => Err(DeploymentApplicationError::Conflict(
            "Deployment changed concurrently; retry the command".into(),
        )),
        DeploymentWriteOutcome::ScheduledCapacityReached => {
            Err(scheduled_capacity_error(scheduled_limit))
        }
    }
}

async fn load_projection(
    repository: &dyn DeploymentRepository,
) -> Result<
    (
        BTreeMap<String, DeploymentRecord>,
        BTreeMap<String, DeploymentRunRecord>,
    ),
    DeploymentApplicationError,
> {
    let deployments = repository
        .deployments()
        .await?
        .into_iter()
        .map(|stored| (stored.id, stored.record))
        .collect();
    let runs = repository
        .deployment_runs()
        .await?
        .into_iter()
        .map(|stored| (stored.id, stored.record))
        .collect();
    Ok((deployments, runs))
}

fn stored_deployment(
    id: &str,
    record: &DeploymentRecord,
) -> Result<DeploymentView, DeploymentApplicationError> {
    Ok(DeploymentView {
        id: id.to_string(),
        record: record.clone(),
    })
}

fn stored_run(
    id: &str,
    record: &DeploymentRunRecord,
) -> Result<DeploymentRunView, DeploymentApplicationError> {
    Ok(DeploymentRunView {
        id: id.to_string(),
        record: record.clone(),
    })
}

fn lifecycle_fact(
    key: String,
    object_id: &str,
    workspace_id: &str,
    event_type: &str,
) -> DeploymentLifecycleFact {
    DeploymentLifecycleFact {
        id: format!("{key}:{}", uuid::Uuid::new_v4().simple()),
        object_id: object_id.to_string(),
        workspace_id: Some(workspace_id.to_string()),
        event_type: event_type.to_string(),
        timestamp: (now_ms() / 1_000) as i64,
        runtime_interval: None,
    }
}

fn validate_record(record: &DeploymentRecord) -> Result<(), DeploymentApplicationError> {
    if record.name.trim().is_empty() {
        return Err(DeploymentApplicationError::Invalid(
            "deployment name must be non-empty".into(),
        ));
    }
    if !(1..=50).contains(&record.initial_events.len()) {
        return Err(DeploymentApplicationError::Invalid(
            "initial_events must contain between 1 and 50 entries".into(),
        ));
    }
    if record.metadata.len() > 16
        || record
            .metadata
            .iter()
            .any(|(key, value)| key.chars().count() > 64 || value.chars().count() > 512)
    {
        return Err(DeploymentApplicationError::Invalid(
            "metadata allows at most 16 pairs, 64-character keys, and 512-character values".into(),
        ));
    }
    if record.resources.len() > 500 {
        return Err(DeploymentApplicationError::Invalid(
            "resources allows at most 500 entries".into(),
        ));
    }
    if record.vault_ids.len() > 50 {
        return Err(DeploymentApplicationError::Invalid(
            "vault_ids allows at most 50 entries".into(),
        ));
    }
    Ok(())
}

fn ensure_scheduled_capacity(
    deployments: &BTreeMap<String, DeploymentRecord>,
    limit: usize,
) -> Result<(), DeploymentApplicationError> {
    let count = deployments
        .values()
        .filter(|record| record.archived_at.is_none() && record.schedule.is_some())
        .count();
    if count >= limit {
        return Err(scheduled_capacity_error(limit));
    }
    Ok(())
}

fn launch_for(record: &DeploymentRecord, deployment_id: &str, run_id: &str) -> DeploymentLaunch {
    DeploymentLaunch {
        deployment_id: deployment_id.to_string(),
        deployment_run_id: run_id.to_string(),
        workspace_id: record.workspace_id.clone(),
        agent: record.agent.clone(),
        environment_id: record.environment_id.clone(),
        metadata: record.metadata.clone(),
        initial_events: record.initial_events.clone(),
        resources: record.resources.clone(),
        vault_ids: record.vault_ids.clone(),
        budget_max_list_cost_minor: record.budget_max_list_cost_minor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(crate) fn command(schedule: bool) -> CreateDeploymentCommand {
        CreateDeploymentCommand {
            workspace_id: "workspace-a".into(),
            agent: AgentSelector {
                id: "agent-a".into(),
                version: Some(1),
            },
            environment_id: "env-a".into(),
            name: "nightly".into(),
            description: None,
            metadata: BTreeMap::new(),
            initial_events: vec![DeploymentSeedEvent::UserMessage {
                content: Vec::new(),
            }],
            resources: Vec::new(),
            schedule: schedule.then(|| DeploymentSchedule::Cron {
                expression: "*/15 * * * *".into(),
                timezone: "UTC".into(),
            }),
            vault_ids: Vec::new(),
            budget_max_list_cost_minor: None,
        }
    }

    #[tokio::test]
    async fn lifecycle_is_owned_by_the_application_and_archive_is_terminal() {
        // Cause-effect graph: C1=live aggregate, C2=archived aggregate,
        // C3=same/different Workspace. Effects: E1=mutation commits,
        // E2=archive replay is idempotent, E3=later mutation/run is rejected,
        // E4=foreign owner is indistinguishable from absence.
        // Decision table: R1 C1+owner->E1; R2 C2+owner+archive->E2;
        // R3 C2+owner+update/run->E3; R4 any+foreign->E4.
        let application = DeploymentApplication::new();
        let created = application.create(command(false)).await.expect("R1");
        application
            .archive("workspace-a", &created.id)
            .await
            .expect("R1");
        application
            .archive("workspace-a", &created.id)
            .await
            .expect("R2");
        assert_eq!(
            application
                .update(
                    "workspace-a",
                    &created.id,
                    UpdateDeploymentCommand {
                        name: Some("changed".into()),
                        ..UpdateDeploymentCommand::default()
                    },
                )
                .await,
            Err(DeploymentApplicationError::Terminal),
            "R3"
        );
        assert_eq!(
            application.get("workspace-b", &created.id).await,
            Err(DeploymentApplicationError::NotFound("deployment")),
            "R4"
        );
    }

    #[tokio::test]
    async fn scheduled_capacity_rejects_without_partial_mutation() {
        // Causes: C1=scheduled slot free/full, C2=create vs unscheduled->scheduled
        // update, C3=archive frees slot. Effects: E1=commit, E2=reject with the
        // prior aggregate unchanged. Rules: R1 free+create->E1; R2 full+create
        // or update->E2; R3 archived slot+update->E1.
        let application = DeploymentApplication::with_scheduled_limit(1);
        let first = application.create(command(true)).await.expect("R1");
        let unscheduled = application.create(command(false)).await.unwrap();
        assert!(application.create(command(true)).await.is_err(), "R2");
        let schedule = command(true).schedule.expect("scheduled fixture");
        assert!(
            application
                .update(
                    "workspace-a",
                    &unscheduled.id,
                    UpdateDeploymentCommand {
                        schedule: Some(FieldUpdate::Replace(schedule.clone())),
                        ..UpdateDeploymentCommand::default()
                    },
                )
                .await
                .is_err(),
            "R2"
        );
        assert!(
            application
                .get("workspace-a", &unscheduled.id)
                .await
                .unwrap()
                .record
                .schedule
                .is_none(),
            "R2 no partial effect"
        );
        application.archive("workspace-a", &first.id).await.unwrap();
        assert!(
            application
                .update(
                    "workspace-a",
                    &unscheduled.id,
                    UpdateDeploymentCommand {
                        schedule: Some(FieldUpdate::Replace(schedule)),
                        ..UpdateDeploymentCommand::default()
                    },
                )
                .await
                .is_ok(),
            "R3"
        );
    }

    pub(crate) struct OutcomeLauncher {
        pub(crate) outcome: DeploymentLaunchOutcome,
        pub(crate) calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DeploymentSessionLauncher for OutcomeLauncher {
        async fn launch(&self, _request: DeploymentLaunch) -> DeploymentLaunchOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
        }
    }

    struct MissingAgentSource;

    #[async_trait]
    impl ExecutableAgentRegistrationSource for MissingAgentSource {
        async fn current_registration(
            &self,
            _workspace_id: &str,
            _agent_id: &str,
        ) -> Result<
            Option<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            ExecutableAgentRegistrationError,
        > {
            Ok(None)
        }

        async fn registration_at_revision(
            &self,
            _workspace_id: &str,
            _agent_id: &str,
            _source_revision: u64,
        ) -> Result<
            Option<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            ExecutableAgentRegistrationError,
        > {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn launch_outcome_decision_table_preserves_pending_and_controls_pause() {
        // Cause graph: C1=manual/scheduled trigger; C2=created, terminal
        // persistent failure, transient failure, or indeterminate outcome;
        // C3=launcher configured. Effects: E1=success XOR error, E2=scheduled
        // persistent failure auto-pauses, E3=manual/transient remains active,
        // E4=indeterminate or missing adapter retains a non-terminal run.
        // Rules exercised: L1 manual+created->E1/E3; L2 scheduled+persistent
        // ->E1/E2; L3 scheduled+transient->E1/E3; L4 any+indeterminate->E4;
        // L5 missing adapter->E4 without inventing UnknownError business truth.
        let success = DeploymentApplication::new();
        success.bind_launcher(Arc::new(OutcomeLauncher {
            outcome: DeploymentLaunchOutcome::Created {
                session_id: "session-a".into(),
            },
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let created = success.create(command(false)).await.unwrap();
        let run = success.run("workspace-a", &created.id).await.unwrap();
        assert_eq!(run.record.session_id.as_deref(), Some("session-a"), "L1");
        assert!(run.record.error.is_none(), "L1 terminal XOR");

        let missing = DeploymentApplication::new();
        let created = missing.create(command(false)).await.unwrap();
        assert!(
            matches!(
                missing.run("workspace-a", &created.id).await,
                Err(DeploymentApplicationError::Unavailable(_))
            ),
            "L5"
        );
        let pending = missing.list_runs("workspace-a").await.unwrap();
        assert_eq!(pending.len(), 1, "L5 durable command identity");
        assert!(
            pending[0].record.session_id.is_none() && pending[0].record.error.is_none(),
            "L5"
        );

        let unavailable = DeploymentApplication::new();
        unavailable.bind_launcher(Arc::new(OutcomeLauncher {
            outcome: DeploymentLaunchOutcome::Unavailable {
                message: "response lost".into(),
            },
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let created = unavailable.create(command(false)).await.unwrap();
        assert!(
            unavailable.run("workspace-a", &created.id).await.is_err(),
            "L4"
        );
        let pending = unavailable.list_runs("workspace-a").await.unwrap();
        assert!(pending[0].record.error.is_none(), "L4");
    }

    #[tokio::test]
    async fn scheduled_failure_pause_matrix_is_exact() {
        // Scheduled failure decision table:
        // P1 persistent Environment failure -> failed run + exact auto-pause;
        // P2 archived subagent -> failed run + exact Agent auto-pause;
        // P3 rate limit -> failed run, schedule stays active;
        // P4 manual persistent failure -> failed run, schedule stays active.
        async fn exercise(
            trigger: DeploymentTrigger,
            error: DeploymentRunFailure,
        ) -> (DeploymentRunRecord, DeploymentRecord) {
            let application = DeploymentApplication::new();
            application.bind_launcher(Arc::new(OutcomeLauncher {
                outcome: DeploymentLaunchOutcome::Failed { error },
                calls: Arc::new(AtomicUsize::new(0)),
            }));
            let deployment = application.create(command(false)).await.unwrap();
            let run_id = "drun-failure";
            let run = DeploymentRunRecord {
                created_at: timestamp(now_ms()),
                deployment_id: deployment.id.clone(),
                workspace_id: "workspace-a".into(),
                agent: deployment.record.agent.clone(),
                trigger,
                session_id: None,
                error: None,
            };
            application.runs.lock().unwrap().insert(run_id.into(), run);
            let launch = launch_for(&deployment.record, &deployment.id, run_id);
            let completed = application.launch_run(run_id, launch).await.unwrap();
            let current = application
                .get("workspace-a", &deployment.id)
                .await
                .unwrap();
            (completed.record, current.record)
        }

        let persistent = DeploymentRunFailure::EnvironmentArchivedError {
            message: "archived".into(),
        };
        let (run, deployment) = exercise(
            DeploymentTrigger::Schedule {
                scheduled_at: timestamp(now_ms()),
            },
            persistent.clone(),
        )
        .await;
        assert_eq!(run.error, Some(persistent.clone()), "P1");
        assert_eq!(deployment.status, DeploymentStatus::Paused, "P1");
        assert_eq!(
            deployment.paused_reason,
            Some(DeploymentPauseReason::Error {
                error: DeploymentPauseError::EnvironmentArchivedError
            }),
            "P1"
        );

        let archived_agent = DeploymentRunFailure::AgentArchivedError {
            message: "subagent archived".into(),
        };
        let (run, deployment) = exercise(
            DeploymentTrigger::Schedule {
                scheduled_at: timestamp(now_ms()),
            },
            archived_agent.clone(),
        )
        .await;
        assert_eq!(run.error, Some(archived_agent), "P2");
        assert_eq!(deployment.status, DeploymentStatus::Paused, "P2");
        assert_eq!(
            deployment.paused_reason,
            Some(DeploymentPauseReason::Error {
                error: DeploymentPauseError::AgentArchivedError
            }),
            "P2"
        );

        let (_, deployment) = exercise(
            DeploymentTrigger::Schedule {
                scheduled_at: timestamp(now_ms()),
            },
            DeploymentRunFailure::SessionRateLimitedError {
                message: "retry".into(),
            },
        )
        .await;
        assert_eq!(deployment.status, DeploymentStatus::Active, "P3");

        let (_, deployment) = exercise(DeploymentTrigger::Manual, persistent).await;
        assert_eq!(deployment.status, DeploymentStatus::Active, "P4");
    }

    #[tokio::test]
    async fn archive_cascade_is_workspace_and_agent_fenced() {
        // Cascade table: A1 same Workspace+Agent -> archive; A2 other Agent ->
        // unchanged; A3 other Workspace -> unchanged; A4 replay -> zero new
        // transitions. No rule creates a DeploymentRun.
        let application = DeploymentApplication::new();
        let same = application.create(command(false)).await.unwrap();
        let mut other_agent = command(false);
        other_agent.agent.id = "agent-b".into();
        let other_agent = application.create(other_agent).await.unwrap();
        let mut other_workspace = command(false);
        other_workspace.workspace_id = "workspace-b".into();
        let other_workspace = application.create(other_workspace).await.unwrap();
        assert_eq!(
            application
                .archive_for_agent("workspace-a", "agent-a")
                .await
                .unwrap(),
            1,
            "A1"
        );
        assert!(
            application
                .get("workspace-a", &same.id)
                .await
                .unwrap()
                .record
                .archived_at
                .is_some(),
            "A1"
        );
        assert!(
            application
                .get("workspace-a", &other_agent.id)
                .await
                .unwrap()
                .record
                .archived_at
                .is_none(),
            "A2"
        );
        assert!(
            application
                .get("workspace-b", &other_workspace.id)
                .await
                .unwrap()
                .record
                .archived_at
                .is_none(),
            "A3"
        );
        assert_eq!(
            application
                .archive_for_agent("workspace-a", "agent-a")
                .await
                .unwrap(),
            0,
            "A4"
        );
        assert!(application.runs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn durable_replicas_claim_one_exact_occurrence() {
        // Distributed cause/effect table: D1 committed Deployment+restart ->
        // same owner projection; D2 two replicas calculate one occurrence ->
        // repository unique claim selects one; D3 loser -> no launch or durable
        // duplicate; D4 lifecycle row and started/succeeded facts commit;
        // D5 a tick speculatively advances its cache but the durable Agent is
        // missing -> refresh the durable revision, archive by CAS, and create
        // no run/launch. D5 guards the cache-vs-repository causal edge.
        let unique = uuid::Uuid::new_v4();
        let path = std::env::temp_dir().join(format!("deployment-{unique}.db"));
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                .unwrap(),
        );
        let first = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        let deployment = first.create(command(true)).await.unwrap();
        let scheduled = 1_767_603_600_000;
        let mut record = first
            .get("workspace-a", &deployment.id)
            .await
            .unwrap()
            .record;
        let expected_revision = record.revision;
        record.next_fire_ms = Some(scheduled);
        record.revision = next_revision(expected_revision).unwrap();
        repository
            .write_deployment(
                stored_deployment(&deployment.id, &record).unwrap(),
                Some(expected_revision),
                DEFAULT_SCHEDULED_LIMIT,
                None,
            )
            .await
            .unwrap();
        let left = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        let right = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        for application in [&left, &right] {
            application.bind_launcher(Arc::new(OutcomeLauncher {
                outcome: DeploymentLaunchOutcome::Created {
                    session_id: "session-one".into(),
                },
                calls: calls.clone(),
            }));
        }
        let due =
            scheduled.saturating_add(execution_jitter_ms(&deployment.id, scheduled, 15 * 60_000));
        let (left_runs, right_runs) =
            tokio::join!(left.tick_and_launch(due), right.tick_and_launch(due));
        assert_eq!(
            left_runs.unwrap().len() + right_runs.unwrap().len(),
            1,
            "D2/D3"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "D2/D3");
        let restored = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        assert_eq!(
            restored.list_runs("workspace-a").await.unwrap().len(),
            1,
            "D1/D3"
        );
        let facts = awaken_session_contract::ManagedSessionRepository::pending_lifecycle(
            repository.as_ref(),
        )
        .await
        .expect("Session lifecycle outbox");
        assert!(
            facts
                .iter()
                .any(|fact| fact.event_type == "deployment_run.started"),
            "D4"
        );
        assert!(
            facts
                .iter()
                .any(|fact| fact.event_type == "deployment_run.succeeded"),
            "D4"
        );

        let missing_deployment = first.create(command(true)).await.unwrap();
        let missing_scheduled = scheduled + 60 * 60_000;
        let mut missing_record = first
            .get("workspace-a", &missing_deployment.id)
            .await
            .unwrap()
            .record;
        let missing_expected_revision = missing_record.revision;
        missing_record.next_fire_ms = Some(missing_scheduled);
        missing_record.revision = next_revision(missing_expected_revision).unwrap();
        assert_eq!(
            repository
                .write_deployment(
                    stored_deployment(&missing_deployment.id, &missing_record).unwrap(),
                    Some(missing_expected_revision),
                    DEFAULT_SCHEDULED_LIMIT,
                    None,
                )
                .await
                .unwrap(),
            DeploymentWriteOutcome::Applied,
            "D5 setup"
        );
        let missing = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        missing.bind_executable_agents(Arc::new(MissingAgentSource));
        missing.bind_launcher(Arc::new(OutcomeLauncher {
            outcome: DeploymentLaunchOutcome::Created {
                session_id: "must-not-launch".into(),
            },
            calls: calls.clone(),
        }));
        let missing_due = missing_scheduled.saturating_add(execution_jitter_ms(
            &missing_deployment.id,
            missing_scheduled,
            15 * 60_000,
        ));
        assert!(
            missing
                .tick_and_launch(missing_due)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "D5 no launch");
        let missing_restored = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        assert!(
            missing_restored
                .get("workspace-a", &missing_deployment.id)
                .await
                .unwrap()
                .record
                .archived_at
                .is_some(),
            "D5 archived"
        );
        assert_eq!(
            missing_restored
                .list_runs("workspace-a")
                .await
                .unwrap()
                .len(),
            1,
            "D5 no durable run"
        );
        drop(missing_restored);
        drop(missing);
        drop(restored);
        drop(left);
        drop(right);
        drop(first);
        drop(repository);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn repository_cas_fences_lost_updates_and_stale_scheduler_commits() {
        // CAS cause-effect graph: C1=two commands share revision r; C2=one wins
        // and advances to r+1; C3=the loser is an ordinary mutation or a
        // scheduled occurrence. Effects: E1=one business row/fact commits,
        // E2=second write returns Conflict, E3=stale scheduler returns
        // StaleDeployment and creates no claim/run, E4=winner state survives.
        // Decision rules: F1 concurrent update/update->E1/E2/E4;
        // F2 archive wins before stale tick claim->E3/E4.
        let path = std::env::temp_dir().join(format!(
            "deployment-cas-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                .unwrap(),
        );
        let application = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        let deployment = application.create(command(true)).await.unwrap();
        let original = deployment.record;
        let mut paused = original.clone();
        paused.status = DeploymentStatus::Paused;
        paused.revision = next_revision(original.revision).unwrap();
        let mut archived = original.clone();
        archived.archived_at = Some(timestamp(now_ms()));
        archived.revision = next_revision(original.revision).unwrap();
        let (left, right) = tokio::join!(
            repository.write_deployment(
                stored_deployment(&deployment.id, &paused).unwrap(),
                Some(original.revision),
                DEFAULT_SCHEDULED_LIMIT,
                None,
            ),
            repository.write_deployment(
                stored_deployment(&deployment.id, &archived).unwrap(),
                Some(original.revision),
                DEFAULT_SCHEDULED_LIMIT,
                None,
            )
        );
        let outcomes = [left.unwrap(), right.unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DeploymentWriteOutcome::Applied)
                .count(),
            1,
            "F1"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DeploymentWriteOutcome::Conflict)
                .count(),
            1,
            "F1/F2"
        );

        let committed = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap()
            .get("workspace-a", &deployment.id)
            .await
            .unwrap()
            .record;
        let stale = original;
        let scheduled_at = timestamp(now_ms());
        let run_id = "drun-stale";
        let run = DeploymentRunRecord {
            created_at: scheduled_at.clone(),
            deployment_id: deployment.id.clone(),
            workspace_id: "workspace-a".into(),
            agent: stale.agent.clone(),
            trigger: DeploymentTrigger::Schedule {
                scheduled_at: scheduled_at.clone(),
            },
            session_id: None,
            error: None,
        };
        let mut stale_advanced = stale.clone();
        stale_advanced.revision = next_revision(stale.revision).unwrap();
        assert_eq!(
            repository
                .claim_scheduled_run(
                    &format!("{}:{scheduled_at}", deployment.id),
                    stale.revision,
                    stored_deployment(&deployment.id, &stale_advanced).unwrap(),
                    stored_run(run_id, &run).unwrap(),
                    lifecycle_fact(
                        "stale-scheduler".into(),
                        run_id,
                        "workspace-a",
                        "deployment_run.started",
                    ),
                )
                .await
                .unwrap(),
            ScheduledRunClaimOutcome::StaleDeployment,
            "F2/E3"
        );
        let restored = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        assert!(
            restored.list_runs("workspace-a").await.unwrap().is_empty(),
            "E3"
        );
        assert_eq!(
            restored
                .get("workspace-a", &deployment.id)
                .await
                .unwrap()
                .record,
            committed,
            "E4"
        );
        drop(restored);
        drop(application);
        drop(repository);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn scheduled_capacity_is_linearizable_across_replicas() {
        // Capacity graph: C1=two replicas observe zero scheduled rows at limit 1;
        // C2=both concurrently create a scheduled Deployment. Effects:
        // E1=database transaction admits exactly one; E2=other receives capacity
        // rejection; E3=restart lists one row. This is the cross-replica rule
        // that a process-local count cannot provide.
        let path = std::env::temp_dir().join(format!(
            "deployment-capacity-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                .unwrap(),
        );
        let mut left = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        left.scheduled_limit = 1;
        let mut right = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        right.scheduled_limit = 1;
        let (left, right) = tokio::join!(left.create(command(true)), right.create(command(true)));
        assert_eq!(
            usize::from(left.is_ok()) + usize::from(right.is_ok()),
            1,
            "E1/E2"
        );
        let restored = DeploymentApplication::from_repository(repository.clone())
            .await
            .unwrap();
        assert_eq!(restored.list("workspace-a").await.unwrap().len(), 1, "E3");
        drop(restored);
        drop(repository);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn revision_exhaustion_rejects_without_mutation() {
        // Revision boundary table: R1 r<i64::MAX -> advance once; R2 r reaches
        // the SQL BIGINT ceiling -> fail closed with Conflict and preserve the
        // aggregate. R3=scheduled tick at the ceiling -> no speculative run or
        // cursor mutation. No saturating/wrapping revision or later store
        // conversion failure may make two distinct writes share one fence value.
        let application = DeploymentApplication::new();
        let deployment = application.create(command(false)).await.unwrap();
        application
            .deployments
            .lock()
            .unwrap()
            .get_mut(&deployment.id)
            .unwrap()
            .revision = MAX_DEPLOYMENT_REVISION;
        assert!(
            matches!(
                application.pause("workspace-a", &deployment.id).await,
                Err(DeploymentApplicationError::Conflict(_))
            ),
            "R2"
        );
        let current = application
            .get("workspace-a", &deployment.id)
            .await
            .unwrap();
        assert_eq!(current.record.revision, MAX_DEPLOYMENT_REVISION, "R2");
        assert_eq!(
            current.record.status,
            DeploymentStatus::Active,
            "R2 no mutation"
        );

        let scheduled = DeploymentApplication::new();
        let deployment = scheduled.create(command(true)).await.unwrap();
        let scheduled_at = now_ms().saturating_sub(MAX_JITTER_BOUND_MS);
        {
            let mut deployments = scheduled.deployments.lock().unwrap();
            let record = deployments.get_mut(&deployment.id).unwrap();
            record.revision = MAX_DEPLOYMENT_REVISION;
            record.next_fire_ms = Some(scheduled_at);
        }
        assert!(
            matches!(
                scheduled.tick_and_launch(now_ms()).await,
                Err(DeploymentApplicationError::Conflict(_))
            ),
            "R3"
        );
        assert!(scheduled.runs.lock().unwrap().is_empty(), "R3 no run");
        let current = scheduled.get("workspace-a", &deployment.id).await.unwrap();
        assert_eq!(current.record.last_run_at, None, "R3 no cursor mutation");
        assert_eq!(current.record.next_fire_ms, Some(scheduled_at), "R3");
    }
}
