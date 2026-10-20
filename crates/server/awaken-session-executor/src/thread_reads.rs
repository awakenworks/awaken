// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: committed Thread readback for one process. Every query reads the
// @formal reason: Session's commit partition through the selected authority (or the resident context's
// @formal reason: exact adapter); it has no cache and no write path.
//! Committed Thread readback: transcripts, recovery snapshots, pending tools,
//! delegated Runs, coordinated Thread links, usage, disposition and terminal
//! Outcome projections.
//!
//! `ThreadReads` owns no state of its own. It reads through the selected
//! Session authority, or through the exact commit adapter a resident Session
//! context already holds, and never builds a context, adopts an Environment or
//! installs a Session.

mod committed_links;
mod committed_runs;
mod role;
mod session_threads;

use committed_links::apply_lifecycle_latest_runs;
pub(crate) use committed_links::{CoordinationReadPurpose, project_committed_coordination};
use committed_runs::delegation_registries_from_commands;
pub(crate) use committed_runs::{
    delegation_registry_from_snapshot, project_delegated_runs, recovery_ticket,
};

use std::sync::{Arc, Weak};

use awaken_agent_contract::agent::awaiting::{AwaitReason, AwaitTarget, ResumeTicket};
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunRecoverySource as _};
use awaken_ext_goal::controller::{CommittedOutcome, Controller};
use awaken_ext_goal::outcome::Id;
use awaken_session_application_contract::{DelegatedRun, Pending};

use crate::SessionExecutionError;
use crate::store::HostCommit;

/// Committed Thread readback over the selected Session authority. Consumers:
/// the Session application's `ThreadReadRuntime` role and every executor
/// component that must observe committed Thread truth.
pub struct ThreadReads {
    authority: Arc<dyn crate::SessionAuthority>,
    installed_sessions: Weak<crate::InstalledSessions>,
    realization_executor: Weak<crate::realization_executor::RealizationExecutor>,
}

impl ThreadReads {
    pub(crate) fn new(
        authority: Arc<dyn crate::SessionAuthority>,
        installed_sessions: Arc<crate::InstalledSessions>,
        realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    ) -> Self {
        Self {
            authority,
            installed_sessions: Arc::downgrade(&installed_sessions),
            realization_executor: Arc::downgrade(&realization_executor),
        }
    }

    /// Open only the authoritative commit/read boundary for a query. A resident
    /// context already owns the exact adapter (including a Worker's recovery
    /// projection); otherwise reconstruct the configured durable adapter without
    /// selecting, creating, or adopting a Session environment.
    pub(crate) async fn commit_for_read(
        &self,
        thread: &str,
    ) -> Result<Arc<HostCommit>, SessionExecutionError> {
        let sessions = self.installed_sessions.upgrade();
        let _lifecycle = match sessions.as_ref() {
            Some(sessions) => {
                sessions
                    .realizations()
                    .lock_lifecycle_if_installed(thread)
                    .await?
            }
            None => None,
        };
        self.open_commit_for_read(thread).await
    }

    /// Read the authoritative commit while the caller holds this Session's
    /// lifecycle guard, proven by the token only that guard lends. This is the
    /// only non-reentrant seam used by continuation quiescence; ordinary
    /// callers use [`Self::commit_for_read`].
    pub(crate) fn commit_for_read_under_lifecycle<'s>(
        &'s self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
    ) -> impl std::future::Future<Output = Result<Arc<HostCommit>, SessionExecutionError>> + Send + use<'s>
    {
        let thread = lifecycle.session().to_owned();
        async move { self.open_commit_for_read(&thread).await }
    }

    /// The resident context's commit, or the configured durable adapter. The
    /// caller decides which lifecycle lock, if any, the read happens under.
    async fn open_commit_for_read(
        &self,
        thread: &str,
    ) -> Result<Arc<HostCommit>, SessionExecutionError> {
        if let Some(sessions) = self.installed_sessions.upgrade()
            && let Some(commit) = sessions
                .read_run(thread, |run| {
                    run.context.as_ref().map(|context| context.commit.clone())
                })?
                .flatten()
        {
            return Ok(commit);
        }
        self.authority.open_commit(thread).await.map(Arc::new)
    }

    /// All messages committed on `thread` so far (the source of history). Empty
    /// when the thread has not run yet. Opens only the configured commit adapter,
    /// so a fresh process can read durable history without provisioning the
    /// Session execution environment (ADR-0039).
    /// True when the durable store already holds `thread` — WITHOUT building a
    /// session context (the layout probe lives with the commit boundary in
    /// [`crate::store`]).
    pub async fn has_durable_thread(&self, thread: &str) -> Result<bool, SessionExecutionError> {
        self.authority.durable_thread_exists(thread).await
    }

    pub async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<Message>, SessionExecutionError> {
        let commit = self.commit_for_read(thread).await?;
        commit
            .authoritative_committed_messages(&ThreadId(thread.to_string()))
            .await
            .map_err(SessionExecutionError::internal)
    }

    /// Rebuild the neutral child-Run projection from the one durable owner:
    /// `RunDelegations` entries committed in each parent Run's ordinary state log.
    pub async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<Vec<DelegatedRun>, SessionExecutionError> {
        Ok(self.delegated_run_snapshot(thread).await?.delegated_runs)
    }

    /// Continuation-only read that reuses the lifecycle guard the caller holds,
    /// proven by its token, instead of recursively acquiring it through
    /// `commit_for_read`.
    pub(crate) fn delegated_runs_under_lifecycle<'s>(
        &'s self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
    ) -> impl std::future::Future<Output = Result<Vec<DelegatedRun>, SessionExecutionError>>
    + Send
    + use<'s> {
        let thread = lifecycle.session().to_owned();
        let commit = self.commit_for_read_under_lifecycle(lifecycle);
        async move {
            let commit = commit.await?;
            self.delegated_run_snapshot_from_commit(&thread, commit)
                .await
                .map(|(snapshot, _)| snapshot.delegated_runs)
        }
    }

    /// Terminal cleanup's delegated-Run read: under the target's held lifecycle
    /// guard, or, for a cold child that has no installed lifecycle (cleanup never
    /// installs one), straight from the configured durable adapter.
    pub(crate) fn delegated_runs_for_cleanup<'s>(
        &'s self,
        thread: String,
        lifecycle: Option<crate::session_realizations::LifecycleHeld<'_>>,
    ) -> impl std::future::Future<Output = Result<Vec<DelegatedRun>, SessionExecutionError>>
    + Send
    + use<'s> {
        let held = lifecycle.map(|lifecycle| self.delegated_runs_under_lifecycle(lifecycle));
        async move {
            match held {
                Some(held) => held.await,
                None => {
                    let commit = self.open_commit_for_read(&thread).await?;
                    self.delegated_run_snapshot_from_commit(&thread, commit)
                        .await
                        .map(|(snapshot, _)| snapshot.delegated_runs)
                }
            }
        }
    }

    async fn delegated_run_snapshot(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, SessionExecutionError>
    {
        Ok(self.delegated_runs_and_root(thread).await?.0)
    }

    /// The delegated-Run projection together with the root recovery prefix it
    /// was read from, so a caller can derive further facts from that same prefix.
    pub(crate) async fn delegated_runs_and_root(
        &self,
        thread: &str,
    ) -> Result<
        (
            awaken_session_application_contract::DelegatedRunSnapshot,
            Option<RunRecoverySnapshot>,
        ),
        SessionExecutionError,
    > {
        let commit = self.commit_for_read(thread).await?;
        self.delegated_run_snapshot_from_commit(thread, commit)
            .await
    }

    async fn delegated_run_snapshot_from_commit(
        &self,
        thread: &str,
        commit: Arc<crate::store::HostCommit>,
    ) -> Result<
        (
            awaken_session_application_contract::DelegatedRunSnapshot,
            Option<RunRecoverySnapshot>,
        ),
        SessionExecutionError,
    > {
        let root_thread = ThreadId(thread.to_string());
        let recovery = match commit
            .authoritative_latest_run(&root_thread)
            .await
            .map_err(SessionExecutionError::internal)?
        {
            Some(run) => Some(
                commit
                    .recovery_snapshot(&root_thread, &run.id)
                    .await
                    .map_err(|error| SessionExecutionError::internal(error.to_string()))?,
            ),
            None => None,
        };
        let commands = recovery
            .as_ref()
            .map(|snapshot| snapshot.state.as_slice())
            .unwrap_or_default();
        let watermark = u64::try_from(commands.len())
            .map_err(|_| SessionExecutionError::internal("delegation watermark exceeds u64"))?;
        let runtime_commit_cursor = recovery
            .as_ref()
            .map_or(0, |snapshot| snapshot.store_cursor);
        let mut projected = Vec::new();
        for registry in delegation_registries_from_commands(commands, None)? {
            projected.extend(project_delegated_runs(Some(&registry)));
        }
        projected.sort_by(|left, right| left.run_id.0.cmp(&right.run_id.0));
        projected.dedup_by(|left, right| left.run_id == right.run_id);
        Ok((
            awaken_session_application_contract::DelegatedRunSnapshot {
                delegated_runs: projected,
                coordinated_thread_ids: Vec::new(),
                watermark,
                runtime_commit_cursor,
            },
            recovery,
        ))
    }

    /// True when `thread` has a run awaiting a decision.
    pub async fn is_awaiting(&self, thread: &str) -> bool {
        let commit = match self.commit_for_read(thread).await {
            Ok(commit) => commit,
            Err(_) => return false,
        };
        match commit
            .open_wait_for_thread(&ThreadId(thread.to_string()))
            .await
        {
            Ok(waiting) => waiting.is_some(),
            Err(error) => {
                tracing::warn!(thread, %error, "failed to read authoritative awaiting position");
                true
            }
        }
    }

    /// The tool an awaiting run on `thread` is awaiting on, if any.
    pub async fn pending_tool(
        &self,
        thread: &str,
    ) -> Result<Option<Pending>, SessionExecutionError> {
        self.pending_tool_in_partition(thread, &ThreadId(thread.to_string()))
            .await
    }

    /// Canonical pending-tool projection for one logical Thread in a Session
    /// partition. Both root and child reads use this implementation so local
    /// durable stores never infer a physical database from a child id.
    pub(crate) async fn pending_tool_in_partition(
        &self,
        partition: &str,
        logical_thread: &ThreadId,
    ) -> Result<Option<Pending>, SessionExecutionError> {
        // The committed ticket is the lifecycle authority. In particular, an
        // observer on another protocol can see the committed tool-call message
        // before the foreground caller reaches `finish_step` and publishes its
        // process-local step projection; consulting that projection here would briefly
        // misclassify a client-executed call as an ordinary executed tool. Open
        // committed truth before rebuilding derived context: a malformed A2A
        // continuation must not hide its still-authoritative input/auth wait and
        // change a legitimate resume into a client-id error.
        let commit = self.commit_for_read(partition).await?;
        let (run_id, ticket) = match commit.open_wait_for_thread(logical_thread).await {
            Ok(Some(waiting)) => waiting,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(SessionExecutionError::internal(format!(
                    "failed to read authoritative pending tool for {}: {error}",
                    logical_thread.0
                )));
            }
        };
        if matches!(ticket.target(), AwaitTarget::RemoteInput { .. }) {
            return Ok(Pending::from_resume_ticket(&ticket));
        }
        if ticket.reason() == AwaitReason::Delegation {
            let snapshot = match commit.recovery_snapshot(logical_thread, &run_id).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(thread = %logical_thread.0, %error, "failed to read pending delegation snapshot");
                    return Err(SessionExecutionError::internal(format!(
                        "failed to read pending delegation snapshot: {error}"
                    )));
                }
            };
            let registry = match delegation_registry_from_snapshot(&snapshot, &run_id) {
                Ok(registry) => registry,
                Err(error) => {
                    tracing::warn!(thread = %logical_thread.0, %error, "failed to rebuild pending delegation registry");
                    return Err(SessionExecutionError::internal(format!(
                        "failed to rebuild pending delegation registry: {error}"
                    )));
                }
            };
            return visible_pending_tool(commit.as_ref(), &ticket, registry.as_ref()).await;
        }
        visible_pending_tool(commit.as_ref(), &ticket, None).await
    }

    /// Read one exact terminal projection through the ordinary commit/read
    /// adapter and the Outcome controller's sole projection implementation.
    /// No Agent or physical Environment context is required for this query.
    pub(crate) async fn committed_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcome>, SessionExecutionError> {
        let commit = self.commit_for_read(thread).await?;
        let thread_id = awaken_agent_contract::agent::thread::Id(thread.to_owned());
        let state = awaken_ext_goal::state::ThreadOutcomeState::new(
            &thread_id,
            commit.as_ref(),
            commit.as_ref(),
        );
        Controller::committed_projection_from_state(&state, &Id(outcome_id.to_owned()))
            .await
            .map_err(crate::outcome_report::controller_error)
    }
}

/// Join the parent-thread delegation relationship with the child thread's
/// authoritative recovery snapshot. Child Runs intentionally own a separate
/// thread, so the parent's consistent snapshot cannot contain their ticket.
pub(crate) async fn authoritative_child_ticket(
    commit: &HostCommit,
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
    parent_call_id: Option<&str>,
) -> Result<Option<ResumeTicket>, SessionExecutionError> {
    let Some(child_run_id) = parent_call_id.and_then(|parent_call_id| {
        registry?
            .delegations()
            .find(|relationship| relationship.parent_call_id == parent_call_id)
            .map(|relationship| relationship.child_run_id.clone())
    }) else {
        return Ok(None);
    };
    let child_thread_id = ThreadId(child_run_id.0.clone());
    let committed = commit
        .recovery_snapshot(&child_thread_id, &child_run_id)
        .await
        .map_err(|error| SessionExecutionError::internal(error.to_string()))?;
    recovery_ticket(&committed, &child_run_id)
}

/// Project the one externally answerable tool from a committed ticket. A
/// delegation owns a child ticket when the child is local; remote A2A may
/// instead expose opaque input through the parent ticket. Both foreground
/// completion and later protocol reads must use this same classification.
pub(crate) async fn visible_pending_tool(
    commit: &HostCommit,
    ticket: &ResumeTicket,
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
) -> Result<Option<Pending>, SessionExecutionError> {
    if ticket.reason() != AwaitReason::Delegation {
        return Ok(Pending::from_resume_ticket(ticket));
    }
    let visible_child = authoritative_child_ticket(commit, registry, ticket.call_id()).await?;
    match visible_child {
        Some(child) => Ok(Pending::from_resume_ticket(&child)),
        None => Ok(Pending::from_resume_ticket(ticket).map(|mut pending| {
            pending.client_executed = true;
            pending
        })),
    }
}

#[cfg(test)]
mod recovery_lifetime_tests;
