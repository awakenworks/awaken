// @formal surface requirement=runtime.pending_input_delegation.delegation_delivery
// @formal reason: This source implements the repository-controlled delegation product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Protocol-neutral Session command/query surface.
//!
//! The application owns access to Runtime and the aggregate repository. Public
//! adapters invoke these use-case methods and never reach through to either
//! collaborator, preventing a second orchestration boundary in protocol state.

use crate::SessionProjectionReads as _;
use awaken_time::WallClockExt as _;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_session_application_contract::DelegatedRun;
use awaken_session_budget_contract::SessionUsage;
use awaken_session_configuration_contract::resource::ResolvedSkillBinding;
use awaken_session_types::RunError;

/// One transitive count law; callers lend their already selected direct values.
/// The resolver owns exact/current selection, never an implicit pin fallback.
pub(super) fn validate_session_skill_total(
    application: &SessionApplication,
    workspace_id: &str,
    root_agent_id: &str,
    root_view: Option<&ExecutableAgentSessionProfile>,
    root_skills: &[awaken_agent_contract::AgentSkillBinding],
    captured: &[(String, ExecutableAgentSessionProfile)],
) -> Result<(), RunError> {
    const MAX_SESSION_SKILLS: usize = 500;
    let mut total: usize = 0;
    let mut add = |count| {
        total = total
            .checked_add(count)
            .filter(|sum| *sum <= MAX_SESSION_SKILLS)
            .ok_or_else(|| {
                RunError::bad_request("a session supports at most 500 skills across all agents")
            })?;
        Ok::<_, RunError>(())
    };
    add(root_skills.len())?;
    let mut seen = std::collections::BTreeSet::from([root_agent_id.to_string()]);
    let mut pending = root_view
        .into_iter()
        .flat_map(|view| view.delegates.iter().cloned())
        .collect::<std::collections::VecDeque<_>>();
    while let Some(delegate) = pending.pop_front() {
        if !seen.insert(delegate.agent_id.clone()) {
            continue;
        }
        let Some(view) = captured
            .iter()
            .find(|(id, _)| id == &delegate.agent_id)
            .map(|(_, selected)| selected.clone())
            .or_else(|| {
                application.queries.selected_session_profile(
                    workspace_id,
                    &delegate.agent_id,
                    delegate.source_revision,
                )
            })
        else {
            continue;
        };
        add(view.skills.len())?;
        pending.extend(view.delegates);
    }
    Ok(())
}

pub(super) fn session_input_error(error: crate::session_resource::SessionInputError) -> RunError {
    use awaken_resource_contract::ResourceRegistryError;
    use awaken_session_configuration_contract::resource::SessionInputError;
    match &error {
        SessionInputError::Registry(ResourceRegistryError::AuthorityUnavailable { .. })
        | SessionInputError::Registry(ResourceRegistryError::Indeterminate { .. }) => {
            RunError::unavailable(error.to_string())
        }
        SessionInputError::Registry(ResourceRegistryError::CorruptData(_)) => {
            RunError::internal(error.to_string())
        }
        _ => RunError::bad_request(error.to_string()),
    }
}

pub use awaken_session_execution_contract::{
    SessionExecutionObservation, SessionModelRequestObservation, SessionRunExecutionObservation,
    SessionToolUseObservation,
};

use crate::SessionApplication;

fn merge_session_usage(total: &mut SessionUsage, child: SessionUsage) {
    total.input_tokens = total.input_tokens.saturating_add(child.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(child.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(child.cache_read_tokens);
    total.cache_creation_tokens = total
        .cache_creation_tokens
        .saturating_add(child.cache_creation_tokens);
    total.web_fetch_requests = total
        .web_fetch_requests
        .saturating_add(child.web_fetch_requests);
    total.web_search_requests = total
        .web_search_requests
        .saturating_add(child.web_search_requests);
    for (model, child_model) in child.by_model {
        let model_usage = total.by_model.entry(model).or_default();
        model_usage.input_tokens = model_usage
            .input_tokens
            .saturating_add(child_model.input_tokens);
        model_usage.output_tokens = model_usage
            .output_tokens
            .saturating_add(child_model.output_tokens);
        model_usage.cache_read_tokens = model_usage
            .cache_read_tokens
            .saturating_add(child_model.cache_read_tokens);
        model_usage.cache_creation_tokens = model_usage
            .cache_creation_tokens
            .saturating_add(child_model.cache_creation_tokens);
    }
}

impl SessionApplication {
    /// Validate that one published Agent can start new Sessions in this scope.
    pub fn validate_profiled_agent(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<(), RunError> {
        if self
            .queries
            .selected_session_profile(workspace_id, agent_id, None)
            .is_none()
            || self.queries.agent_unavailable(workspace_id, agent_id)
        {
            return Err(RunError::bad_request(format!(
                "agent `{agent_id}` is unavailable"
            )));
        }
        Ok(())
    }

    /// Observe one Session's frozen execution selection and committed primary
    /// Thread Runs without driving realization or execution. A completed model
    /// request is included only when its audit record and Run coexist in the
    /// same Runtime recovery snapshot.
    pub async fn session_execution_observation(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<SessionExecutionObservation, crate::SessionProjectionRecoveryError> {
        let thread_reads = self.runtime().thread_reads();

        use awaken_agent_contract::audit::model_request::ModelRequestObservation as RuntimeModelRequestObservation;

        let recovered = self
            .queries
            .read_session_projection(session_id.as_str(), Some(workspace_id))
            .await?
            .ok_or(crate::SessionProjectionRecoveryError::NotFound)?;
        let baseline = recovered.session.frozen_baseline().ok_or_else(|| {
            crate::SessionProjectionRecoveryError::Unavailable(
                "Session creation finalization is incomplete".into(),
            )
        })?;
        let snapshot = thread_reads
            .session_thread_recovery_snapshot(session_id, session_id.as_str())
            .await
            .map_err(crate::SessionProjectionRecoveryError::Rejected)?;
        let runs = snapshot
            .map(|snapshot| {
                snapshot
                    .runs
                    .into_iter()
                    .map(|run| {
                        let idempotency_key = recovered
                            .session
                            .user_run_idempotency_key(&run.id)
                            .map_err(|error| {
                                crate::SessionProjectionRecoveryError::Rejected(RunError::internal(
                                    format!(
                                        "resolve Session Event provenance for Run `{}`: {error}",
                                        run.id.0
                                    ),
                                ))
                            })?
                            .map(str::to_owned);
                        let mut observed_tool_uses = std::collections::BTreeSet::new();
                        let mut tool_uses = Vec::new();
                        for message in snapshot.messages.iter().filter(|message| {
                            message.role == awaken_agent_contract::agent::message::Role::Assistant
                                && message.id.assistant_step_of(&run.id).is_some()
                        }) {
                            for block in &message.content {
                                if let ContentBlock::ToolUse { id, .. } = block {
                                    if id.trim().is_empty()
                                        || !observed_tool_uses
                                            .insert((message.id.0.as_str(), id.as_str()))
                                    {
                                        return Err(
                                            crate::SessionProjectionRecoveryError::Rejected(
                                                RunError::internal(format!(
                                                    "invalid committed tool-use identity for Run `{}`",
                                                    run.id.0
                                                )),
                                            ),
                                        );
                                    }
                                    tool_uses.push(SessionToolUseObservation {
                                        source_message_id: message.id.0.clone(),
                                        call_id: id.clone(),
                                    });
                                }
                            }
                        }
                        let model_requests = snapshot
                            .events
                            .iter()
                            .filter(|event| event.run_id == run.id)
                            .try_fold(Vec::new(), |mut observations, event| {
                                if let Some(observation) =
                                    RuntimeModelRequestObservation::from_record(event)?
                                {
                                    observations.push(SessionModelRequestObservation {
                                        sequence: event.sequence,
                                        is_error: observation.is_error,
                                        retry_count: observation.retry_count,
                                    });
                                }
                                Ok::<_, serde_json::Error>(observations)
                            })
                            .map_err(|error| {
                                crate::SessionProjectionRecoveryError::Rejected(RunError::internal(
                                    format!("decode committed model request observation: {error}"),
                                ))
                            })?;
                        Ok(SessionRunExecutionObservation {
                            run_id: run.id,
                            idempotency_key,
                            state: run.state,
                            tool_uses,
                            model_requests,
                        })
                    })
                    .collect::<Result<Vec<_>, crate::SessionProjectionRecoveryError>>()
            })
            .transpose()?
            .unwrap_or_default();
        use crate::SessionProjectionRecoveryError as CaptureReadError;
        let artifact_capture = if let Some(files) = &self.resource_files {
            let capture = files
                .artifact_capture(workspace_id, session_id.as_str())
                .await
                .map_err(|_| {
                    CaptureReadError::Unavailable(
                        "Session artifact capture catalog is unavailable".into(),
                    )
                })?;
            capture
                .as_ref()
                .map_or(Ok(()), |capture| {
                    capture.verify_scope(workspace_id, session_id.as_str())
                })
                .map_err(|_| {
                    CaptureReadError::Unavailable(
                        "Session artifact capture identity differs from its original owner".into(),
                    )
                })?;
            capture
        } else {
            None
        };
        Ok(SessionExecutionObservation {
            artifact_capture,
            recovery: self
                .runtime()
                .coordination_reads()
                .execution_recovery_observation(&recovered.session)
                .await,
            session_id: recovered.session.session_id.to_string(),
            session_revision: recovered.session.revision.0,
            agent_id: baseline.agent_id.clone(),
            agent_revision: baseline.agent_revision,
            application_origin: baseline.application_origin.clone(),
            model: baseline.model.clone(),
            execution_model_ref: baseline.execution_model_ref.clone(),
            backend_ref: baseline.runtime.clone(),
            execution: recovered.session.execution,
            runs,
        })
    }

    pub fn validate_session_skill_total(
        &self,
        workspace_id: &str,
        root_agent_id: &str,
        root_view: Option<&ExecutableAgentSessionProfile>,
        root_skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<(), RunError> {
        validate_session_skill_total(
            self,
            workspace_id,
            root_agent_id,
            root_view,
            root_skills,
            &[],
        )
    }

    pub async fn runtime_owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        let thread_reads = self.runtime().thread_reads();

        thread_reads.owns_thread(thread).await
    }

    /// Read cumulative accounting for one logical Thread through its parent
    /// Session partition. This is the same neutral Runtime authority used by
    /// Session totals; the application does not cache or re-aggregate it.
    pub async fn session_thread_usage(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<SessionUsage, RunError> {
        let budget_runtime = self.runtime().budget();

        budget_runtime
            .session_thread_usage(session_id, thread_id)
            .await
    }

    /// Read the Runtime's one consistent recovery prefix for a logical child
    /// through the parent Session partition. This is a direct query wrapper;
    /// the application owns no snapshot cache or reconstructed projection.
    pub async fn session_thread_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let thread_reads = self.runtime().thread_reads();

        thread_reads
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await
    }

    pub async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        let coordination_reads = self.runtime().coordination_reads();

        coordination_reads.delegated_runs(thread).await
    }

    pub async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<ResolvedSkillBinding>, RunError> {
        let environment_runtime = self.runtime().environment();

        environment_runtime
            .resolve_session_skills(workspace_id, skills)
            .await
    }

    pub async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        let streaming_runtime = self.runtime().streaming_runs();

        streaming_runtime.interrupt(thread).await
    }

    pub async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError> {
        let outcome_runtime = self.runtime().outcomes();

        outcome_runtime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    /// Aggregate current counters through an already observed relationship set
    /// inside a fenced read, or discover current relationships when absent.
    /// This borrows query input; it creates no relationship or usage cache.
    pub async fn session_usage(
        &self,
        thread: &str,
        observed_links: Option<&[awaken_session_execution_contract::CoordinatedThreadLink]>,
    ) -> Result<SessionUsage, RunError> {
        self.session_usage_with_additional_thread(thread, None, observed_links)
            .await
    }

    /// Include the currently executing logical Thread exactly once even if its
    /// deterministic coordination receipt is still racing an already-admitted
    /// dispatch. The committed Thread usage and ordinary link projection remain
    /// the only authorities; this method creates no relationship cache.
    pub(crate) async fn session_usage_for_model_request(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<SessionUsage, RunError> {
        self.session_usage_with_additional_thread(session_id, Some(thread_id), None)
            .await
    }

    async fn session_usage_with_additional_thread(
        &self,
        thread: &str,
        additional_thread: Option<&str>,
        observed_links: Option<&[awaken_session_execution_contract::CoordinatedThreadLink]>,
    ) -> Result<SessionUsage, RunError> {
        let coordination_reads = self.runtime().coordination_reads();

        let budget_runtime = self.runtime().budget();

        let usage = budget_runtime.session_usage(thread).await?;
        let mut thread_usages = vec![usage];
        let links = match observed_links {
            Some(links) => std::borrow::Cow::Borrowed(links),
            None => std::borrow::Cow::Owned(
                coordination_reads
                    .coordinated_threads(&awaken_session_types::SessionId::from(thread), None)
                    .await?,
            ),
        };
        if links.iter().any(|link| link.session_id != thread) {
            return Err(RunError::internal(
                "usage query received a coordinated Thread from another Session",
            ));
        }
        let mut included = std::collections::BTreeSet::from([thread.to_string()]);
        for link in links.iter() {
            if !included.insert(link.thread_id.0.clone()) {
                continue;
            }
            let child = budget_runtime
                .session_thread_usage(
                    &awaken_session_types::SessionId::from(thread),
                    &link.thread_id.0,
                )
                .await?;
            thread_usages.push(child);
        }
        if let Some(additional_thread) = additional_thread
            && included.insert(additional_thread.to_string())
        {
            let current = budget_runtime
                .session_thread_usage(
                    &awaken_session_types::SessionId::from(thread),
                    additional_thread,
                )
                .await?;
            thread_usages.push(current);
        }
        let session = self
            .mutations
            .session_aggregate(&awaken_session_types::SessionId::from(thread))
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        Ok(Self::project_session_usage(
            &session,
            thread_usages,
            self.clock.now_ms(),
        ))
    }

    /// Aggregate already observed, distinct logical Threads using the same
    /// accounting rule as live budget reads. Callers own scope and prefix
    /// validation; this projection creates no usage cache or authority.
    /// Active time belongs to the Session root, never the sum of child clocks.
    /// The observing owner supplies its existing Clock sample; a projection
    /// cannot silently select an ambient second time authority.
    pub fn project_session_usage(
        session: &awaken_session_state_contract::PersistedSession,
        threads: impl IntoIterator<Item = SessionUsage>,
        now_unix_ms: u64,
    ) -> SessionUsage {
        let mut usage = SessionUsage::default();
        for thread in threads {
            merge_session_usage(&mut usage, thread);
        }
        usage.active_seconds = session.effective_runtime_active_millis(now_unix_ms) / 1_000;
        usage
    }

    #[must_use]
    pub fn model(&self) -> String {
        let execution = self.runtime().core();

        execution.model()
    }
}

impl crate::SessionCredentials {
    pub async fn missing_vault(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
    ) -> Result<Option<String>, RunError> {
        let Some(source) = self.credential_source() else {
            return Ok(None);
        };
        for vault_id in vault_ids {
            let exists = source
                .has_vault(workspace_id, vault_id)
                .await
                .map_err(|error| {
                    RunError::unavailable(format!("credential authority unavailable: {error}"))
                })?;
            if !exists {
                return Ok(Some(vault_id.clone()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod input_error_tests {
    #[test]
    fn dependency_failure_is_not_reclassified_as_invalid_user_input() {
        // R1 dependency unavailable -> Unavailable; R2 corrupt persisted data
        // -> Internal; R3 invalid caller binding -> BadRequest. Identical
        // diagnostic text in every row proves the type owns the decision.
        use awaken_resource_contract::ResourceRegistryError;
        use awaken_session_configuration_contract::resource::SessionInputError;
        use awaken_session_types::RunErrorKind;
        for (error, expected) in [
            (
                SessionInputError::Registry(ResourceRegistryError::AuthorityUnavailable {
                    detail: "same".into(),
                }),
                RunErrorKind::Unavailable,
            ),
            (
                SessionInputError::Registry(ResourceRegistryError::Indeterminate {
                    detail: "same".into(),
                }),
                RunErrorKind::Unavailable,
            ),
            (
                SessionInputError::Registry(ResourceRegistryError::CorruptData("same".into())),
                RunErrorKind::Internal,
            ),
            (
                SessionInputError::InvalidBindingId("same".into()),
                RunErrorKind::BadRequest,
            ),
        ] {
            assert_eq!(super::session_input_error(error).kind, expected);
        }
    }
}
