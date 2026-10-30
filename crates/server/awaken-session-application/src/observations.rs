// @formal surface requirement=runtime.pending_input_delegation.delegation_delivery
// @formal reason: Read-only projections of committed Session, Thread and coordination facts; it
// @formal reason: issues no command and owns no state, cache or durable authority.
//! Session observations: the frozen execution selection with its committed
//! Runs, cumulative usage across coordinated Threads, delegated Runs, Thread
//! recovery prefixes and the live Thread subscription.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_application_contract::DelegatedRun;
use awaken_session_contract::SessionUsage;
use awaken_session_execution_contract::{
    BudgetAccounting, ChildRunCoordination, SessionCoordinationReads, ThreadReads,
};
use awaken_session_types::RunError;
use awaken_time::WallClockExt as _;

use crate::SessionProjectionReads as _;
use crate::{
    SessionExecutionObservation, SessionModelRequestObservation, SessionRunExecutionObservation,
    SessionToolUseObservation,
};

/// Read-only Session observations over the Session root, the committed
/// projection and the
/// Thread, coordination and budget runtime roles, plus the File catalog that
/// reports artifact capture.
pub struct SessionObservations {
    mutations: Arc<crate::SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    queries: Arc<crate::SessionQueries>,
    resource_files: Option<Arc<dyn awaken_resource_contract::FileCatalog>>,
    thread_reads: Arc<dyn ThreadReads>,
    coordination: Arc<dyn ChildRunCoordination>,
    coordination_reads: Arc<dyn SessionCoordinationReads>,
    budget: Arc<dyn BudgetAccounting>,
}

impl SessionObservations {
    pub(crate) fn new(
        mutations: Arc<crate::SessionMutations>,
        clock: Arc<dyn awaken_time::WallClock>,
        queries: Arc<crate::SessionQueries>,
        resource_files: Option<Arc<dyn awaken_resource_contract::FileCatalog>>,
        runtime: &crate::SessionExecutor,
    ) -> Self {
        Self {
            mutations,
            clock,
            queries,
            resource_files,
            thread_reads: runtime.thread_reads.clone(),
            coordination: runtime.coordination.clone(),
            coordination_reads: runtime.coordination_reads.clone(),
            budget: runtime.budget.clone(),
        }
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
        let thread_reads = self.thread_reads.as_ref();

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
                        let model_requests = SessionModelRequestObservation::for_run(&snapshot.events, &run.id)
                            .map_err(|error| crate::SessionProjectionRecoveryError::Rejected(RunError::internal(format!("decode committed model request observation: {error}"))))?;
                        let model_operations = awaken_runtime_contract::model_inspection::recorded_model_operations(&snapshot.state, &run.id)
                            .map_err(crate::SessionProjectionRecoveryError::Unavailable)?;
                        Ok(SessionRunExecutionObservation {
                            model_operations,
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
            configuration: Some(self.queries.inspect_configuration(workspace_id, baseline)),
            artifact_capture,
            recovery: self
                .coordination_reads
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

    /// Read cumulative accounting for one logical Thread through its parent
    /// Session partition. This is the same neutral Runtime authority used by
    /// Session totals; the application does not cache or re-aggregate it.
    pub async fn session_thread_usage(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<SessionUsage, RunError> {
        let budget_runtime = self.budget.as_ref();

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
        let thread_reads = self.thread_reads.as_ref();

        thread_reads
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await
    }

    pub async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        let coordination_reads = self.coordination_reads.as_ref();

        coordination_reads.delegated_runs(thread).await
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
        let coordination_reads = self.coordination_reads.as_ref();

        let budget_runtime = self.budget.as_ref();

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
        session: &awaken_session_contract::PersistedSession,
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
}

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

#[async_trait::async_trait]
impl crate::SessionObservation for SessionObservations {
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        RunError,
    > {
        self.coordination
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    async fn session_execution_observation(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<SessionExecutionObservation, crate::SessionProjectionRecoveryError> {
        SessionObservations::session_execution_observation(self, workspace_id, session_id).await
    }

    async fn session_usage(
        &self,
        thread: &str,
        observed_links: Option<&[awaken_session_execution_contract::CoordinatedThreadLink]>,
    ) -> Result<SessionUsage, RunError> {
        SessionObservations::session_usage(self, thread, observed_links).await
    }
}
