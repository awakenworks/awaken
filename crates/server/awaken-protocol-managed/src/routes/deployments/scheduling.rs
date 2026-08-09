//! Deployment aggregate recovery, durable lifecycle transitions, and cron execution.
//!
//! The parent module owns HTTP admission and DTO projection. This module owns the
//! application behavior that coordinates the durable repository and Session port.

use super::*;

fn next_sequence<'a>(ids: impl Iterator<Item = &'a String>, prefix: &str) -> u64 {
    ids.filter_map(|id| id.strip_prefix(prefix)?.parse::<u64>().ok())
        .max()
        .map_or(0, |value| value.saturating_add(1))
}

pub(super) fn stored_deployment(
    id: &str,
    record: &DeploymentRecord,
) -> Result<StoredDeployment, DeploymentRepositoryError> {
    Ok(StoredDeployment {
        deployment_id: id.to_string(),
        workspace_id: record.workspace_id.clone(),
        data: serde_json::to_string(record)
            .map_err(|error| DeploymentRepositoryError::Storage(error.to_string()))?,
    })
}

fn stored_run(
    id: &str,
    record: &RunRecord,
) -> Result<StoredDeploymentRun, DeploymentRepositoryError> {
    Ok(StoredDeploymentRun {
        run_id: id.to_string(),
        deployment_id: record.deployment_id.clone(),
        workspace_id: record.workspace_id.clone(),
        data: serde_json::to_string(record)
            .map_err(|error| DeploymentRepositoryError::Storage(error.to_string()))?,
    })
}

fn lifecycle_fact(
    fact_key: String,
    object_id: &str,
    workspace_id: &str,
    event_type: &str,
) -> ManagedLifecycleFact {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    ManagedLifecycleFact {
        id: format!("{fact_key}:{}:{nonce}", std::process::id()),
        object_id: object_id.to_string(),
        workspace_id: Some(workspace_id.to_string()),
        event_type: event_type.to_string(),
        timestamp: (now_ms() / 1_000) as i64,
    }
}

impl DeploymentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore the Deployment aggregate and DeploymentRun history from one
    /// durable repository. In-flight Session execution remains Session truth;
    /// this restores only the scheduling/control-plane aggregate.
    pub async fn with_repository(
        repository: Arc<dyn DeploymentRepository>,
    ) -> Result<Self, DeploymentRepositoryError> {
        let deployments = repository
            .deployments()
            .await?
            .into_iter()
            .map(|stored| {
                let record: DeploymentRecord = serde_json::from_str(&stored.data)
                    .map_err(|error| DeploymentRepositoryError::Storage(error.to_string()))?;
                if record.workspace_id != stored.workspace_id {
                    return Err(DeploymentRepositoryError::Storage(
                        "Deployment owner mismatch in durable row".into(),
                    ));
                }
                Ok((stored.deployment_id, record))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let runs = repository
            .deployment_runs()
            .await?
            .into_iter()
            .map(|stored| {
                let record: RunRecord = serde_json::from_str(&stored.data)
                    .map_err(|error| DeploymentRepositoryError::Storage(error.to_string()))?;
                if record.deployment_id != stored.deployment_id
                    || record.workspace_id != stored.workspace_id
                {
                    return Err(DeploymentRepositoryError::Storage(
                        "DeploymentRun identity mismatch in durable row".into(),
                    ));
                }
                Ok((stored.run_id, record))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let dep_seq = next_sequence(deployments.keys(), "depl_");
        let run_seq = next_sequence(runs.keys(), "drun_");
        Ok(Self {
            deployments: Mutex::new(deployments),
            runs: Mutex::new(runs),
            dep_seq: AtomicU64::new(dep_seq),
            run_seq: AtomicU64::new(run_seq),
            launcher: Mutex::new(None),
            rate_limiter: Mutex::new(None),
            repository: Some(repository),
            executable_agents: Mutex::new(None),
            scheduled_limit: MAX_SCHEDULED_DEPLOYMENTS,
        })
    }

    /// Merge the repository's latest committed projection into this replica's
    /// working set. The repository is authoritative in multi-replica deployments;
    /// process memory is only a scheduling cache and must never create affinity.
    pub(super) async fn refresh_repository_projection(
        &self,
    ) -> Result<(), DeploymentRepositoryError> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        let deployments = repository
            .deployments()
            .await?
            .into_iter()
            .map(|stored| {
                let record: DeploymentRecord = serde_json::from_str(&stored.data)
                    .map_err(|error| DeploymentRepositoryError::Storage(error.to_string()))?;
                if record.workspace_id != stored.workspace_id {
                    return Err(DeploymentRepositoryError::Storage(
                        "Deployment owner mismatch in durable row".into(),
                    ));
                }
                Ok((stored.deployment_id, record))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let runs = repository
            .deployment_runs()
            .await?
            .into_iter()
            .map(|stored| {
                let record: RunRecord = serde_json::from_str(&stored.data)
                    .map_err(|error| DeploymentRepositoryError::Storage(error.to_string()))?;
                if record.deployment_id != stored.deployment_id
                    || record.workspace_id != stored.workspace_id
                {
                    return Err(DeploymentRepositoryError::Storage(
                        "DeploymentRun identity mismatch in durable row".into(),
                    ));
                }
                Ok((stored.run_id, record))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        self.dep_seq
            .fetch_max(next_sequence(deployments.keys(), "depl_"), Ordering::SeqCst);
        self.run_seq
            .fetch_max(next_sequence(runs.keys(), "drun_"), Ordering::SeqCst);
        self.deployments.lock().unwrap().extend(deployments);
        self.runs.lock().unwrap().extend(runs);
        Ok(())
    }

    pub(super) async fn persist_deployment_event(
        &self,
        id: &str,
        record: &DeploymentRecord,
        event_type: &str,
    ) -> Result<(), DeploymentRepositoryError> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        repository
            .upsert_deployment(
                stored_deployment(id, record)?,
                Some(lifecycle_fact(
                    format!("deployment:{id}:{event_type}:{}", record.updated_at),
                    id,
                    &record.workspace_id,
                    event_type,
                )),
            )
            .await
    }

    pub(super) async fn persist_run_event(
        &self,
        id: &str,
        record: &RunRecord,
        event_type: &str,
    ) -> Result<(), DeploymentRepositoryError> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        repository
            .upsert_deployment_run(
                stored_run(id, record)?,
                Some(lifecycle_fact(
                    format!("deployment_run:{id}:{event_type}"),
                    id,
                    &record.workspace_id,
                    event_type,
                )),
            )
            .await
    }

    /// Persist a DeploymentRun without publishing a lifecycle fact. Anthropic's
    /// webhook contract exposes run events only for scheduled executions; manual
    /// runs remain fully durable and listable through the same repository row.
    pub(super) async fn persist_manual_run(
        &self,
        id: &str,
        record: &RunRecord,
    ) -> Result<(), DeploymentRepositoryError> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        repository
            .upsert_deployment_run(stored_run(id, record)?, None)
            .await
    }

    /// Bind the Session application service after both control and data planes
    /// have been assembled. The state is shared by the already-mounted router.
    pub fn bind_launcher(&self, launcher: Arc<dyn DeploymentSessionLauncher>) {
        *self.launcher.lock().unwrap() = Some(launcher);
    }

    /// Bind the composition root's one organization limiter. Deployment-created
    /// Sessions then share the ordinary Managed Create bucket.
    pub fn bind_rate_limiter(&self, limiter: Arc<ManagedRateLimiter>) {
        *self.rate_limiter.lock().unwrap() = Some(limiter);
    }

    /// Bind Coordinator's immutable executable-Agent projection. Deployment
    /// writes resolve a bare Agent id to the current registered publication once,
    /// so every later run remains pinned without reading Control's authoring DB.
    pub fn bind_executable_agents(&self, source: Arc<dyn ExecutableAgentRegistrationSource>) {
        *self.executable_agents.lock().unwrap() = Some(source);
    }

    pub(super) async fn resolve_agent(
        &self,
        workspace_id: &str,
        input: &crate::types::AgentRef,
    ) -> Result<AgentReference, WireError> {
        if input.id().trim().is_empty() || input.version() == Some(0) {
            return Err(invalid(
                "deployment Agent id must be non-empty and version must be at least 1",
            ));
        }
        if let crate::types::AgentRef::Object(reference) = input
            && (!matches!(
                reference.kind.as_ref(),
                Some(crate::types::AgentRefKind::Agent)
            ) || reference.system.is_some()
                || reference.tools.is_some()
                || reference.mcp_servers.is_some()
                || reference.skills.is_some()
                || reference.model.is_some())
        {
            return Err(invalid(
                "deployment Agent object must be an unmodified `agent` reference",
            ));
        }
        let source = self.executable_agents.lock().unwrap().clone();
        let Some(source) = source else {
            return Ok(AgentReference::from_input(input));
        };
        let selected = match input.version().map(u64::from) {
            Some(version) => {
                source
                    .registration_at_revision(workspace_id, input.id(), version)
                    .await
            }
            None => source.current_registration(workspace_id, input.id()).await,
        }
        .map_err(agent_resolution_error)?
        .ok_or_else(|| not_found("agent"))?;
        Ok(AgentReference::new(
            selected.agent_id,
            selected.source_revision,
        ))
    }

    async fn primary_agent_missing_or_archived(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<bool, DeploymentRepositoryError> {
        let source = self.executable_agents.lock().unwrap().clone();
        let Some(source) = source else {
            return Ok(false);
        };
        source
            .current_registration(workspace_id, agent_id)
            .await
            .map(|registration| registration.is_none())
            .map_err(|error| DeploymentRepositoryError::Storage(error.to_string()))
    }

    /// Archive every live Deployment whose primary Agent was archived. The Agent
    /// archive handler invokes this before returning, so no later schedule can
    /// mint a run for that primary Agent.
    pub async fn archive_for_agent(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<usize, DeploymentRepositoryError> {
        self.refresh_repository_projection().await?;
        let now = crate::cron::to_rfc3339(now_ms());
        let candidates: Vec<(String, DeploymentRecord)> = self
            .deployments
            .lock()
            .unwrap()
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
            self.persist_deployment_event(id, candidate, "deployment.archived")
                .await?;
        }
        let mut deployments = self.deployments.lock().unwrap();
        for (id, candidate) in &candidates {
            deployments.insert(id.clone(), candidate.clone());
        }
        Ok(candidates.len())
    }

    #[cfg(test)]
    pub(super) fn with_scheduled_limit(limit: usize) -> Self {
        Self {
            scheduled_limit: limit,
            ..Self::default()
        }
    }

    pub(super) async fn launch_run(
        &self,
        run_id: &str,
        launch: DeploymentLaunch,
    ) -> Result<DeploymentRun, DeploymentRepositoryError> {
        let admitted = self
            .rate_limiter
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|limiter| limiter.admit_internal_session_create());
        let outcome = if admitted {
            let launcher = self.launcher.lock().unwrap().clone();
            match launcher {
                Some(launcher) => launcher.launch(launch).await,
                None => DeploymentLaunchOutcome::Failed {
                    error: RunError::UnknownError {
                        message: "deployment Session launcher is not bound".to_string(),
                    },
                },
            }
        } else {
            DeploymentLaunchOutcome::Failed {
                error: RunError::SessionRateLimitedError {
                    message: "organization Session creation rate limit exceeded".to_string(),
                },
            }
        };
        if let DeploymentLaunchOutcome::Unavailable { message } = outcome {
            return Err(DeploymentRepositoryError::Storage(format!(
                "Deployment Session launch remains pending: {message}"
            )));
        }
        let (deployment_id, trigger, error) = {
            let mut runs = self.runs.lock().unwrap();
            let record = runs
                .get_mut(run_id)
                .expect("deployment run was inserted before launch");
            match outcome {
                DeploymentLaunchOutcome::Created { session_id } => {
                    record.session_id = Some(session_id);
                    record.error = None;
                }
                DeploymentLaunchOutcome::Failed { error } => {
                    record.session_id = None;
                    record.error = Some(error);
                }
                DeploymentLaunchOutcome::Unavailable { .. } => unreachable!(
                    "indeterminate launch outcome returns before terminal run projection"
                ),
            }
            (
                record.deployment_id.clone(),
                record.trigger.clone(),
                record.error.clone(),
            )
        };
        let paused = if matches!(trigger, TriggerContext::Schedule { .. })
            && let Some(reason) = error.as_ref().and_then(RunError::paused_reason)
            && let Some(deployment) = self.deployments.lock().unwrap().get_mut(&deployment_id)
        {
            deployment.status = "paused".into();
            deployment.paused_reason = Some(PausedReason::Error { error: reason });
            deployment.updated_at = crate::cron::to_rfc3339(now_ms());
            Some(deployment.clone())
        } else {
            None
        };
        let run = self
            .runs
            .lock()
            .unwrap()
            .get(run_id)
            .expect("deployment run remains stored")
            .clone();
        let run_event = if run.error.is_some() {
            "deployment_run.failed"
        } else {
            "deployment_run.succeeded"
        };
        match &run.trigger {
            TriggerContext::Schedule { .. } => {
                self.persist_run_event(run_id, &run, run_event).await?;
            }
            TriggerContext::Manual => self.persist_manual_run(run_id, &run).await?,
        }
        if let Some(deployment) = paused {
            self.persist_deployment_event(&deployment_id, &deployment, "deployment.paused")
                .await?;
        }
        Ok(run.project(run_id))
    }

    /// Fire due schedule occurrences and launch each through the same Session port
    /// as a manual run. Returns the completed run projections for observability.
    pub async fn tick_and_launch(
        &self,
        now_ms: u64,
    ) -> Result<Vec<DeploymentRun>, DeploymentRepositoryError> {
        self.refresh_repository_projection().await?;
        let run_ids = self.tick(now_ms);
        let launches: Vec<(String, DeploymentLaunch, RunRecord, DeploymentRecord)> = {
            let runs = self.runs.lock().unwrap();
            let deployments = self.deployments.lock().unwrap();
            run_ids
                .into_iter()
                .filter_map(|run_id| {
                    let deployment_id = runs.get(&run_id)?.deployment_id.clone();
                    let deployment = deployments.get(&deployment_id)?.clone();
                    let launch = deployment.launch(&deployment_id, &run_id);
                    Some((
                        run_id.clone(),
                        launch,
                        runs.get(&run_id)?.clone(),
                        deployment,
                    ))
                })
                .collect()
        };
        let mut completed = Vec::with_capacity(launches.len());
        for (run_id, launch, run, deployment) in launches {
            if self
                .primary_agent_missing_or_archived(&launch.workspace_id, &launch.agent.id)
                .await?
            {
                self.runs.lock().unwrap().remove(&run_id);
                self.archive_for_agent(&launch.workspace_id, &launch.agent.id)
                    .await?;
                continue;
            }
            if let Some(repository) = &self.repository {
                let TriggerContext::Schedule { scheduled_at } = &run.trigger else {
                    unreachable!("tick only creates scheduled DeploymentRuns")
                };
                let claim_id = format!("{}:{scheduled_at}", run.deployment_id);
                let claimed = repository
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
                    .await?;
                if !claimed {
                    self.runs.lock().unwrap().remove(&run_id);
                    continue;
                }
            }
            completed.push(self.launch_run(&run_id, launch).await?);
        }
        Ok(completed)
    }

    /// Advance every active schedule to `now_ms`. Exact cron instants remain the
    /// trigger context, while execution waits for the stable bounded jitter delay.
    /// The cursor is seeded after the first tick, so new deployments never fire
    /// retroactively. Returns the run ids fired.
    pub fn tick(&self, now_ms: u64) -> Vec<String> {
        let mut fired = Vec::new();
        let mut deployments = self.deployments.lock().unwrap();
        let mut runs = self.runs.lock().unwrap();
        for (dep_id, record) in deployments.iter_mut() {
            let Some((cron, timezone)) = record.active_cron() else {
                continue;
            };
            let mut cursor = match record.next_fire_ms {
                Some(cursor) => cursor,
                None => match cron.next_after_in(now_ms, timezone) {
                    Some(cursor) => cursor,
                    None => continue,
                },
            };
            loop {
                let next = cron.next_after_in(cursor, timezone);
                let interval_ms = next
                    .map(|next| next.saturating_sub(cursor))
                    .unwrap_or(60_000);
                let due_ms =
                    cursor.saturating_add(execution_jitter_ms(dep_id, cursor, interval_ms));
                if due_ms > now_ms {
                    break;
                }
                let n = self.run_seq.fetch_add(1, Ordering::SeqCst);
                let run_id = format!("drun_{n:016}");
                let scheduled_at = crate::cron::to_rfc3339(cursor);
                runs.insert(
                    run_id.clone(),
                    RunRecord {
                        created_at: crate::cron::to_rfc3339(now_ms),
                        deployment_id: dep_id.clone(),
                        workspace_id: record.workspace_id.clone(),
                        agent: record.agent.clone(),
                        trigger: TriggerContext::Schedule {
                            scheduled_at: scheduled_at.clone(),
                        },
                        session_id: None,
                        error: None,
                    },
                );
                record.last_run_at = Some(scheduled_at);
                fired.push(run_id);
                cursor = match next {
                    Some(cursor) => cursor,
                    None => break,
                };
            }
            record.next_fire_ms = Some(cursor);
        }
        fired
    }
}
