// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the commit source this executor's committed reads open through. It reuses a
// @formal reason: resident context's exact commit adapter and serializes with the Session lifecycle guard;
// @formal reason: it has no cache and no write path.
//! The commit source of this executor's committed Thread reads
//! (`awaken_session_run_delivery::CommittedReads`).
//!
//! A resident Session context already owns the exact commit adapter
//! (including a Worker's recovery projection); otherwise the configured
//! durable adapter is opened without selecting, creating or adopting a
//! Session environment. Ordinary reads serialize with the Session's lifecycle
//! guard; continuation quiescence and terminal cleanup read under the guard
//! they already hold.

use std::sync::{Arc, Weak};

use awaken_runtime_contract::runs::{DelegatedRun, RunError};

use awaken_session_realization::peers::EnvironmentPeer as _;
use awaken_session_run_delivery::{CommittedReads, SessionCommit};

use crate::SessionExecutionError;
use crate::session_execution_error::to_run_error;

/// The commit boundary this executor's reads open. Consumers: its
/// [`CommittedReads`] and the lifecycle-held reads below.
pub(crate) struct InstalledCommitSource {
    authority: Arc<dyn crate::SessionAuthority>,
    installed_sessions: Weak<crate::InstalledSessions>,
}

impl InstalledCommitSource {
    pub(crate) fn new(
        authority: Arc<dyn crate::SessionAuthority>,
        installed_sessions: &Arc<crate::InstalledSessions>,
    ) -> Self {
        Self {
            authority,
            installed_sessions: Arc::downgrade(installed_sessions),
        }
    }

    /// The resident context's commit, or the configured durable adapter. The
    /// caller decides which lifecycle lock, if any, the read happens under.
    async fn open_unlocked(
        &self,
        thread: &str,
    ) -> Result<Arc<SessionCommit>, SessionExecutionError> {
        if let Some(sessions) = self.installed_sessions.upgrade()
            && let Some(commit) = sessions
                .read_run(thread, |run| {
                    run.context.as_ref().map(|context| context.commit.clone())
                })?
                .flatten()
        {
            return Ok(commit);
        }
        self.authority
            .open_for_read(thread)
            .await
            .map_err(SessionExecutionError::from)
    }

    /// Continuation-only read that reuses the lifecycle guard the caller holds,
    /// proven by its token, instead of recursively acquiring it.
    pub(crate) fn delegated_runs_under_lifecycle<'s>(
        &'s self,
        reads: &'s CommittedReads,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
    ) -> impl std::future::Future<Output = Result<Vec<DelegatedRun>, SessionExecutionError>>
    + Send
    + use<'s> {
        let thread = lifecycle.session().to_owned();
        async move {
            let commit = self.open_unlocked(&thread).await?;
            Ok(reads
                .delegated_run_snapshot_from_commit(&thread, commit)
                .await?
                .0
                .delegated_runs)
        }
    }

    /// Terminal cleanup's delegated-Run read: under the target's held lifecycle
    /// guard, or, for a cold child that has no installed lifecycle (cleanup never
    /// installs one), straight from the configured durable adapter.
    pub(crate) fn delegated_runs_for_cleanup<'s>(
        &'s self,
        reads: &'s CommittedReads,
        thread: String,
        lifecycle: Option<crate::session_realizations::LifecycleHeld<'_>>,
    ) -> impl std::future::Future<Output = Result<Vec<DelegatedRun>, SessionExecutionError>>
    + Send
    + use<'s> {
        let held = lifecycle.map(|lifecycle| self.delegated_runs_under_lifecycle(reads, lifecycle));
        async move {
            match held {
                Some(held) => held.await,
                None => {
                    let commit = self.open_unlocked(&thread).await?;
                    Ok(reads
                        .delegated_run_snapshot_from_commit(&thread, commit)
                        .await?
                        .0
                        .delegated_runs)
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_run_delivery::SessionCommitSource for InstalledCommitSource {
    async fn read_transcript_page(
        &self,
        session: &str,
        request: &awaken_agent_contract::thread::read::transcript::TranscriptPageRequest,
    ) -> Result<
        awaken_agent_contract::thread::read::transcript::TranscriptPage,
        awaken_runtime_contract::runs::RunError,
    > {
        self.authority.read_transcript_page(session, request).await
    }

    async fn open_for_read(&self, session: &str) -> Result<Arc<SessionCommit>, RunError> {
        let sessions = self.installed_sessions.upgrade();
        let _lifecycle = match sessions.as_ref() {
            Some(sessions) => sessions
                .realizations()
                .lock_lifecycle_if_installed(session)
                .await
                .map_err(|error| to_run_error(error.into()))?,
            None => None,
        };
        self.open_unlocked(session).await.map_err(to_run_error)
    }

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RunError> {
        self.authority.durable_thread_exists(thread).await
    }
}

/// Advisor model labels from the publication this executor resolves for a
/// Session: its delivered snapshot, else the catalog's current one.
pub(crate) struct ResolvedAdvisorModels {
    installed_sessions: Weak<crate::InstalledSessions>,
    realization_executor: Weak<crate::realization_executor::RealizationExecutor>,
}

impl ResolvedAdvisorModels {
    pub(crate) fn new(
        installed_sessions: &Arc<crate::InstalledSessions>,
        realization_executor: &Arc<crate::realization_executor::RealizationExecutor>,
    ) -> Self {
        Self {
            installed_sessions: Arc::downgrade(installed_sessions),
            realization_executor: Arc::downgrade(realization_executor),
        }
    }
}

impl awaken_session_run_delivery::AdvisorModelSource for ResolvedAdvisorModels {
    fn advisor_model(&self, session: &str) -> Result<Option<String>, RunError> {
        let delivered = self
            .installed_sessions
            .upgrade()
            .map(|sessions| sessions.realizations().published_snapshot(session))
            .transpose()
            .map_err(|error| to_run_error(error.into()))?
            .flatten();
        Ok(self.realization_executor.upgrade().and_then(|executor| {
            executor
                .environments()
                .publication()
                .resolve_session_publication(session, None, delivered)
                .ok()
                .and_then(|(_, _, publication, _)| publication)
                .and_then(|snapshot| {
                    snapshot
                        .resolved_spec
                        .plugin_config
                        .agent
                        .advisor
                        .map(|advisor| advisor.model)
                })
        }))
    }
}

#[cfg(test)]
mod committed_read_tests;
#[cfg(test)]
mod recovery_lifetime_tests;
