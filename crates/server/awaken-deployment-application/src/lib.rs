//! Coordinator-owned Deployment aggregate, scheduling, and Session launch orchestration.
//!
//! Public protocol adapters translate their wire DTOs into the commands in this
//! crate. Repository adapters persist opaque encodings of these application
//! records, so neither storage nor this owner depends on Axum or Managed DTOs.

mod model;

pub use model::{
    AgentSelector, CreateDeploymentCommand, DeploymentAgent, DeploymentLaunch,
    DeploymentLaunchOutcome, DeploymentPauseError, DeploymentPauseReason, DeploymentRecord,
    DeploymentRunFailure, DeploymentRunRecord, DeploymentRunView, DeploymentSchedule,
    DeploymentStatus, DeploymentTrigger, DeploymentView, UpdateDeploymentCommand,
};

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_deployment_contract::{
    AgentArchiveCascade, Cron, DeploymentLifecycleFact, DeploymentRecord as StoredDeployment,
    DeploymentRepository, DeploymentRepositoryError, DeploymentRunRecord as StoredRun,
};
use awaken_executable_agent_contract::{
    ExecutableAgentRegistrationError, ExecutableAgentRegistrationSource,
};
use chrono_tz::Tz;

const DEFAULT_SCHEDULED_LIMIT: usize = 1_000;
const MAX_JITTER_MS: u64 = 10_000;

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
    repository: Option<Arc<dyn DeploymentRepository>>,
    executable_agents: Mutex<Option<Arc<dyn ExecutableAgentRegistrationSource>>>,
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
            repository: None,
            executable_agents: Mutex::new(None),
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
            repository: Some(repository),
            executable_agents: Mutex::new(None),
            scheduled_limit: DEFAULT_SCHEDULED_LIMIT,
        })
    }

    pub fn bind_launcher(&self, launcher: Arc<dyn DeploymentSessionLauncher>) {
        *self.launcher.lock().expect("Deployment launcher lock") = Some(launcher);
    }

    pub fn bind_executable_agents(&self, source: Arc<dyn ExecutableAgentRegistrationSource>) {
        *self
            .executable_agents
            .lock()
            .expect("executable Agent source lock") = Some(source);
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
        let agent = self
            .resolve_agent(&command.workspace_id, &command.agent)
            .await?;
        let now = now_ms();
        let record = DeploymentRecord {
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
        self.persist_deployment(&id, &record, "deployment.created")
            .await?;
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
        if let Some(Some(schedule)) = &command.schedule {
            validate_schedule(Some(schedule))?;
        }
        let mut candidate = self.get_cached(workspace_id, id)?;
        if candidate.archived_at.is_some() {
            return Err(DeploymentApplicationError::Terminal);
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
        if let Some(value) = command.description {
            candidate.description = value;
        }
        if let Some(metadata) = command.metadata {
            match metadata {
                None => candidate.metadata.clear(),
                Some(patch) => {
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
        if let Some(value) = command.resources {
            candidate.resources = value.unwrap_or_default();
        }
        if let Some(value) = command.schedule {
            candidate.schedule = value;
            candidate.next_fire_ms = candidate
                .schedule
                .as_ref()
                .and_then(|schedule| next_occurrence(schedule, now_ms()));
        }
        if let Some(value) = command.vault_ids {
            candidate.vault_ids = value.unwrap_or_default();
        }
        validate_record(&candidate)?;
        let current_had_schedule = self.get_cached(workspace_id, id)?.schedule.is_some();
        if !current_had_schedule && candidate.schedule.is_some() {
            ensure_scheduled_capacity(
                &self.deployments.lock().expect("Deployment projection lock"),
                self.scheduled_limit,
            )?;
        }
        candidate.updated_at = timestamp(now_ms());
        self.persist_deployment(id, &candidate, "deployment.updated")
            .await?;
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
            let now = now_ms();
            record.archived_at = Some(timestamp(now));
            record.updated_at = timestamp(now);
            self.persist_deployment(id, &record, "deployment.archived")
                .await?;
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
        self.persist_deployment(id, &record, event).await?;
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
                (id.clone(), candidate)
            })
            .collect();
        for (id, candidate) in &candidates {
            self.persist_deployment(id, candidate, "deployment.archived")
                .await?;
        }
        let mut deployments = self.deployments.lock().expect("Deployment projection lock");
        for (id, candidate) in &candidates {
            deployments.insert(id.clone(), candidate.clone());
        }
        Ok(candidates.len())
    }

    pub async fn tick_and_launch(
        &self,
        now: u64,
    ) -> Result<Vec<DeploymentRunView>, DeploymentApplicationError> {
        self.refresh().await?;
        let candidates = self.tick(now);
        let mut completed = Vec::with_capacity(candidates.len());
        for (run_id, launch, run, deployment) in candidates {
            if self
                .primary_agent_missing(&launch.workspace_id, &launch.agent.id)
                .await?
            {
                self.runs
                    .lock()
                    .expect("DeploymentRun projection lock")
                    .remove(&run_id);
                self.archive_for_agent(&launch.workspace_id, &launch.agent.id)
                    .await?;
                continue;
            }
            if let Some(repository) = &self.repository {
                let DeploymentTrigger::Schedule { scheduled_at } = &run.trigger else {
                    return Err(DeploymentApplicationError::Invalid(
                        "scheduler produced a non-scheduled run".into(),
                    ));
                };
                let claim_id = format!("{}:{scheduled_at}", run.deployment_id);
                if !repository
                    .claim_scheduled_run(
                        &claim_id,
                        stored_deployment(&run.deployment_id, &deployment)?,
                        stored_run(&run_id, &run)?,
                        lifecycle_fact(
                            format!("deployment_run:{run_id}:deployment_run.started"),
                            &run_id,
                            &run.workspace_id,
                            "deployment_run.started",
                        ),
                    )
                    .await?
                {
                    self.runs
                        .lock()
                        .expect("DeploymentRun projection lock")
                        .remove(&run_id);
                    continue;
                }
            }
            completed.push(self.launch_run(&run_id, launch).await?);
        }
        Ok(completed)
    }

    fn tick(
        &self,
        now: u64,
    ) -> Vec<(
        String,
        DeploymentLaunch,
        DeploymentRunRecord,
        DeploymentRecord,
    )> {
        let mut result = Vec::new();
        let mut deployments = self.deployments.lock().expect("Deployment projection lock");
        let mut runs = self.runs.lock().expect("DeploymentRun projection lock");
        for (deployment_id, deployment) in deployments.iter_mut() {
            let Some((cron, timezone)) = active_cron(deployment) else {
                continue;
            };
            let mut cursor = match deployment.next_fire_ms {
                Some(cursor) => cursor,
                None => match cron.next_after_in(now, timezone) {
                    Some(cursor) => cursor,
                    None => continue,
                },
            };
            loop {
                let next = cron.next_after_in(cursor, timezone);
                let due = cursor.saturating_add(execution_jitter_ms(deployment_id, cursor));
                if due > now {
                    break;
                }
                let run_id = format!("drun_{}", uuid::Uuid::new_v4().simple());
                let scheduled_at = timestamp(cursor);
                let run = DeploymentRunRecord {
                    created_at: timestamp(now),
                    deployment_id: deployment_id.clone(),
                    workspace_id: deployment.workspace_id.clone(),
                    agent: deployment.agent.clone(),
                    trigger: DeploymentTrigger::Schedule {
                        scheduled_at: scheduled_at.clone(),
                    },
                    session_id: None,
                    error: None,
                };
                runs.insert(run_id.clone(), run.clone());
                deployment.last_run_at = Some(scheduled_at);
                result.push((
                    run_id.clone(),
                    launch_for(deployment, deployment_id, &run_id),
                    run,
                    deployment.clone(),
                ));
                cursor = match next {
                    Some(cursor) => cursor,
                    None => break,
                };
            }
            deployment.next_fire_ms = Some(cursor);
        }
        result
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
            self.persist_deployment(&run.deployment_id, &deployment, "deployment.paused")
                .await?;
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
        event: &str,
    ) -> Result<(), DeploymentApplicationError> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        repository
            .upsert_deployment(
                stored_deployment(id, record)?,
                Some(lifecycle_fact(
                    format!("deployment:{id}:{event}:{}", record.updated_at),
                    id,
                    &record.workspace_id,
                    event,
                )),
            )
            .await?;
        Ok(())
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
        repository
            .upsert_deployment_run(stored_run(id, record)?, lifecycle)
            .await?;
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
        .map(|stored| {
            let record: DeploymentRecord = serde_json::from_str(&stored.data)
                .map_err(|error| DeploymentApplicationError::Unavailable(error.to_string()))?;
            if record.workspace_id != stored.workspace_id {
                return Err(DeploymentApplicationError::Unavailable(
                    "Deployment owner mismatch in durable row".into(),
                ));
            }
            Ok((stored.deployment_id, record))
        })
        .collect::<Result<_, _>>()?;
    let runs = repository
        .deployment_runs()
        .await?
        .into_iter()
        .map(|stored| {
            let record: DeploymentRunRecord = serde_json::from_str(&stored.data)
                .map_err(|error| DeploymentApplicationError::Unavailable(error.to_string()))?;
            if record.deployment_id != stored.deployment_id
                || record.workspace_id != stored.workspace_id
            {
                return Err(DeploymentApplicationError::Unavailable(
                    "DeploymentRun identity mismatch in durable row".into(),
                ));
            }
            Ok((stored.run_id, record))
        })
        .collect::<Result<_, _>>()?;
    Ok((deployments, runs))
}

fn stored_deployment(
    id: &str,
    record: &DeploymentRecord,
) -> Result<StoredDeployment, DeploymentApplicationError> {
    Ok(StoredDeployment {
        deployment_id: id.to_string(),
        workspace_id: record.workspace_id.clone(),
        data: serde_json::to_string(record)
            .map_err(|error| DeploymentApplicationError::Unavailable(error.to_string()))?,
    })
}

fn stored_run(
    id: &str,
    record: &DeploymentRunRecord,
) -> Result<StoredRun, DeploymentApplicationError> {
    Ok(StoredRun {
        run_id: id.to_string(),
        deployment_id: record.deployment_id.clone(),
        workspace_id: record.workspace_id.clone(),
        data: serde_json::to_string(record)
            .map_err(|error| DeploymentApplicationError::Unavailable(error.to_string()))?,
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
    }
}

fn validate_schedule(
    schedule: Option<&DeploymentSchedule>,
) -> Result<(), DeploymentApplicationError> {
    let Some(schedule) = schedule else {
        return Ok(());
    };
    Cron::parse(schedule.expression()).map_err(|error| {
        DeploymentApplicationError::Invalid(format!("invalid cron schedule: {error}"))
    })?;
    schedule.timezone().parse::<Tz>().map_err(|error| {
        DeploymentApplicationError::Invalid(format!("invalid IANA timezone: {error}"))
    })?;
    Ok(())
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
        return Err(DeploymentApplicationError::Invalid(format!(
            "an organization supports at most {limit} scheduled deployments"
        )));
    }
    Ok(())
}

fn parsed_schedule(schedule: &DeploymentSchedule) -> Option<(Cron, Tz)> {
    Some((
        Cron::parse(schedule.expression()).ok()?,
        schedule.timezone().parse().ok()?,
    ))
}

fn next_occurrence(schedule: &DeploymentSchedule, after_ms: u64) -> Option<u64> {
    let (cron, timezone) = parsed_schedule(schedule)?;
    cron.next_after_in(after_ms, timezone)
}

fn active_cron(record: &DeploymentRecord) -> Option<(Cron, Tz)> {
    if record.status != DeploymentStatus::Active || record.archived_at.is_some() {
        return None;
    }
    parsed_schedule(record.schedule.as_ref()?)
}

fn execution_jitter_ms(deployment_id: &str, scheduled_ms: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in deployment_id.bytes().chain(scheduled_ms.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash % (MAX_JITTER_MS + 1)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn command(schedule: bool) -> CreateDeploymentCommand {
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
            initial_events: vec![serde_json::json!({"type":"user.message"})],
            resources: Vec::new(),
            schedule: schedule.then(|| DeploymentSchedule::Cron {
                expression: "*/15 * * * *".into(),
                timezone: "UTC".into(),
                last_run_at: None,
                upcoming_runs_at: Vec::new(),
            }),
            vault_ids: Vec::new(),
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
        let schedule = command(true).schedule;
        assert!(
            application
                .update(
                    "workspace-a",
                    &unscheduled.id,
                    UpdateDeploymentCommand {
                        schedule: Some(schedule.clone()),
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
                        schedule: Some(schedule),
                        ..UpdateDeploymentCommand::default()
                    },
                )
                .await
                .is_ok(),
            "R3"
        );
    }

    #[test]
    fn jitter_is_stable_and_bounded_at_extreme_time() {
        // Cause/effect decision table: J1 same identity -> identical delay;
        // J2 different schedule instant including u64::MAX -> no panic/wrap in
        // jitter calculation and delay <=10s. The caller's saturating due-time
        // addition then fails late rather than firing an overflowed occurrence.
        let delay = execution_jitter_ms("depl-a", u64::MAX);
        assert_eq!(delay, execution_jitter_ms("depl-a", u64::MAX), "J1");
        assert!(delay <= MAX_JITTER_MS, "J2");
        assert_eq!(u64::MAX.saturating_add(delay), u64::MAX, "J2");
    }

    struct OutcomeLauncher {
        outcome: DeploymentLaunchOutcome,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DeploymentSessionLauncher for OutcomeLauncher {
        async fn launch(&self, _request: DeploymentLaunch) -> DeploymentLaunchOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
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
        // P2 rate limit -> failed run, schedule stays active;
        // P3 manual persistent failure -> failed run, schedule stays active.
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

        let (_, deployment) = exercise(
            DeploymentTrigger::Schedule {
                scheduled_at: timestamp(now_ms()),
            },
            DeploymentRunFailure::SessionRateLimitedError {
                message: "retry".into(),
            },
        )
        .await;
        assert_eq!(deployment.status, DeploymentStatus::Active, "P2");

        let (_, deployment) = exercise(DeploymentTrigger::Manual, persistent).await;
        assert_eq!(deployment.status, DeploymentStatus::Active, "P3");
    }

    #[tokio::test]
    async fn schedule_cursor_is_future_only_and_pause_suppresses_execution() {
        // Cursor graph: S1 first tick with no cursor -> seed strictly after now;
        // S2 before stable jitter due -> no run; S3 at due -> one run carrying
        // the unjittered cron instant; S4 paused -> no later runs; S5 unpause ->
        // reseed after unpause time and never backfill missed occurrences.
        const MONDAY_0900: u64 = 1_767_603_600_000;
        let application = DeploymentApplication::new();
        let mut create = command(true);
        create.schedule = Some(DeploymentSchedule::Cron {
            expression: "*/15 * * * *".into(),
            timezone: "UTC".into(),
            last_run_at: None,
            upcoming_runs_at: Vec::new(),
        });
        let deployment = application.create(create).await.unwrap();
        {
            let mut records = application.deployments.lock().unwrap();
            records.get_mut(&deployment.id).unwrap().next_fire_ms = None;
        }
        assert!(application.tick(MONDAY_0900).is_empty(), "S1");
        let scheduled = MONDAY_0900 + 15 * 60_000;
        let due = scheduled.saturating_add(execution_jitter_ms(&deployment.id, scheduled));
        assert!(application.tick(due - 1).is_empty(), "S2");
        let fired = application.tick(due);
        assert_eq!(fired.len(), 1, "S3");
        assert!(
            matches!(
                &fired[0].2.trigger,
                DeploymentTrigger::Schedule { scheduled_at }
                    if scheduled_at == "2026-01-05T09:15:00Z"
            ),
            "S3"
        );
        application
            .pause("workspace-a", &deployment.id)
            .await
            .unwrap();
        assert!(
            application.tick(MONDAY_0900 + 2 * 60 * 60_000).is_empty(),
            "S4"
        );
        application
            .unpause("workspace-a", &deployment.id)
            .await
            .unwrap();
        assert!(
            application
                .get("workspace-a", &deployment.id)
                .await
                .unwrap()
                .record
                .next_fire_ms
                .is_some_and(|cursor| cursor > now_ms()),
            "S5"
        );
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
        // duplicate; D4 lifecycle row and started/succeeded facts commit.
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
        record.next_fire_ms = Some(scheduled);
        repository
            .upsert_deployment(stored_deployment(&deployment.id, &record).unwrap(), None)
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
        let due = scheduled.saturating_add(execution_jitter_ms(&deployment.id, scheduled));
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
        .await;
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
        drop(restored);
        drop(left);
        drop(right);
        drop(first);
        drop(repository);
        let _ = std::fs::remove_file(path);
    }
}
