// @formal entrypoint ingress.commit_epoch_atomic_remote_custom
// @formal entrypoint remote_worker.committed_truth_is_coordinator_owned
// @formal entrypoint remote_worker.operation_identity_maps_to_one_hash_and_append
// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
// @formal entrypoint run.lifecycle_absorption
//! [`CommittedReads`]: committed Thread readback — transcripts, recovery
//! snapshots, pending tools, delegated Runs, coordinated Thread links, usage,
//! disposition and terminal Outcome projections — and the commit owner a
//! Worker's claimed commit writes through.
//!
//! `CommittedReads` owns no state. It reads through the [`SessionCommitSource`]
//! its composition selects: a Coordinator opens the commit straight from its
//! runtime authority; a co-located executor adds its resident-context reuse
//! and lifecycle serialization. It never builds a context, adopts an
//! Environment or installs a Session.

mod committed_links;
mod committed_runs;
mod role;
mod session_threads;
mod task_callback;

pub use committed_links::{
    CommittedCoordinationProjection, CoordinationReadPurpose, project_committed_coordination,
};
use committed_runs::delegation_registries_from_commands;
pub use committed_runs::{
    delegation_registry_from_snapshot, project_delegated_runs, recovery_ticket,
};

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::{AwaitReason, AwaitTarget, ResumeTicket};
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_agent_contract::thread::read::transcript::{TranscriptPage, TranscriptPageRequest};
use awaken_ext_goal::controller::{CommittedOutcome, Controller};
use awaken_ext_goal::outcome::Id;
use awaken_runtime_contract::runs::{DelegatedRun, Pending, RunError};

use awaken_session_execution_contract::ApplicationError;

use crate::SessionCommit;

/// A Run failure as the neutral application error the Coordinator's durable
/// and claimed-commit ports return: the fault kind survives, the code is the
/// message's.
#[must_use]
pub fn application_error(error: RunError) -> ApplicationError {
    use awaken_runtime_contract::runs::RunErrorKind;
    match error.kind {
        RunErrorKind::BadRequest => ApplicationError::invalid(error.message),
        RunErrorKind::Conflict => ApplicationError::conflict(error.message),
        RunErrorKind::Internal => ApplicationError::internal(error.message),
        RunErrorKind::Unavailable => ApplicationError::unavailable(error.message),
    }
}

/// Where committed reads open a Session's commit boundary. Consumers:
/// [`CommittedReads`] and Environment-free Worker construction. Implementations:
/// the Coordinator's runtime authority, the Worker's fresh recovery projection,
/// and the co-located executor's resident-context-aware source.
#[async_trait::async_trait]
pub trait SessionCommitSource: Send + Sync {
    /// The commit boundary of `session`'s physical partition.
    async fn open_for_read(&self, session: &str) -> Result<Arc<SessionCommit>, RunError>;

    /// Whether the durable store already holds `thread`.
    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RunError>;

    async fn read_transcript_page(
        &self,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, RunError> {
        let commit = self.open_for_read(&request.thread_id.0).await?;
        let authority = commit
            .committed_authority()
            .ok_or_else(|| RunError::unavailable("authoritative transcript is unavailable"))?;
        authority
            .transcript_page(request)
            .await
            .map_err(RunError::internal)
    }
}

/// The model label of a Session's advisor, from its current publication.
/// Consumer: coordinated-Thread links, which carry the label only as
/// enrichment: identity always comes from the committed advisor call.
pub trait AdvisorModelSource: Send + Sync {
    /// The advisor model of `session`'s publication, when it is known. A
    /// publication that cannot be resolved has no label; an unreadable
    /// Session projection is refused.
    fn advisor_model(&self, session: &str) -> Result<Option<String>, RunError>;
}

/// Committed Thread readback over one commit source. Consumers: the Session
/// application's `ThreadReads` role, the Worker recovery source, the
/// claimed-commit owner, and every component that observes committed truth.
pub struct CommittedReads {
    source: Arc<dyn SessionCommitSource>,
    advisor_models: Arc<dyn AdvisorModelSource>,
}

impl CommittedReads {
    /// Reads over `source`, labelling advisor links from `advisor_models`.
    #[must_use]
    pub fn new(
        source: Arc<dyn SessionCommitSource>,
        advisor_models: Arc<dyn AdvisorModelSource>,
    ) -> Self {
        Self {
            source,
            advisor_models,
        }
    }

    /// The commit boundary of `session` from the selected source.
    pub async fn commit_for_read(&self, session: &str) -> Result<Arc<SessionCommit>, RunError> {
        self.source.open_for_read(session).await
    }

    /// The Session's existing commit owner for a Worker's claim-guarded
    /// commit. Physical commit authority is Session-scoped: a child keeps its
    /// logical Thread identity in the operation, while the guarded dispatch
    /// selects the parent Session partition. This only resolves the existing
    /// owner; it never opens a second one.
    pub async fn commit_owner(
        &self,
        session_thread: &ThreadId,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError> {
        self.commit_for_read(&session_thread.0)
            .await
            .map(|commit| commit as Arc<dyn OperationCoordinator>)
            .map_err(application_error)
    }

    /// True when the durable store already holds `thread`, without opening a
    /// Session context.
    pub async fn has_durable_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.source.durable_thread_exists(thread).await
    }

    /// All messages committed on `thread` so far (the source of history). Empty
    /// when the thread has not run yet. Opens only the commit boundary, so a
    /// fresh process reads durable history without provisioning the Session
    /// execution environment (ADR-0039).
    pub async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        let commit = self.commit_for_read(thread).await?;
        commit
            .authoritative_committed_messages(&ThreadId(thread.to_string()))
            .await
            .map_err(RunError::internal)
    }

    /// A page of an authorized primary Thread's frozen committed transcript.
    pub async fn transcript_page(
        &self,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, RunError> {
        self.source.read_transcript_page(request).await
    }

    /// Rebuild the neutral child-Run projection from the one durable owner:
    /// `RunDelegations` entries committed in each parent Run's ordinary state log.
    pub async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        Ok(self.delegated_run_snapshot(thread).await?.delegated_runs)
    }

    async fn delegated_run_snapshot(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        Ok(self.delegated_runs_and_root(thread).await?.0)
    }

    /// The delegated-Run projection together with the root recovery prefix it
    /// was read from, so a caller can derive further facts from that same prefix.
    pub async fn delegated_runs_and_root(
        &self,
        thread: &str,
    ) -> Result<
        (
            awaken_session_application_contract::DelegatedRunSnapshot,
            Option<RunRecoverySnapshot>,
        ),
        RunError,
    > {
        let commit = self.commit_for_read(thread).await?;
        self.delegated_run_snapshot_from_commit(thread, commit)
            .await
    }

    /// The delegated-Run projection read from `commit`, with the root
    /// recovery prefix it was read from. A caller that already holds the
    /// Session's commit (for example under its own serialization) reads here.
    pub async fn delegated_run_snapshot_from_commit(
        &self,
        thread: &str,
        commit: Arc<SessionCommit>,
    ) -> Result<
        (
            awaken_session_application_contract::DelegatedRunSnapshot,
            Option<RunRecoverySnapshot>,
        ),
        RunError,
    > {
        let root_thread = ThreadId(thread.to_string());
        let recovery = match commit
            .authoritative_latest_run(&root_thread)
            .await
            .map_err(RunError::internal)?
        {
            Some(run) => Some(
                commit
                    .recovery_snapshot(&root_thread, &run.id)
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?,
            ),
            None => None,
        };
        let commands = recovery
            .as_ref()
            .map(|snapshot| snapshot.state.as_slice())
            .unwrap_or_default();
        let watermark = u64::try_from(commands.len())
            .map_err(|_| RunError::internal("delegation watermark exceeds u64"))?;
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
    pub async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        self.pending_tool_in_partition(thread, &ThreadId(thread.to_string()))
            .await
    }

    /// Canonical pending-tool projection for one logical Thread in a Session
    /// partition. Both root and child reads use this implementation so local
    /// durable stores never infer a physical database from a child id.
    pub async fn pending_tool_in_partition(
        &self,
        partition: &str,
        logical_thread: &ThreadId,
    ) -> Result<Option<Pending>, RunError> {
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
                return Err(RunError::internal(format!(
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
                    return Err(RunError::internal(format!(
                        "failed to read pending delegation snapshot: {error}"
                    )));
                }
            };
            let registry = match delegation_registry_from_snapshot(&snapshot, &run_id) {
                Ok(registry) => registry,
                Err(error) => {
                    tracing::warn!(thread = %logical_thread.0, %error, "failed to rebuild pending delegation registry");
                    return Err(RunError::internal(format!(
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
    pub async fn committed_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcome>, RunError> {
        let commit = self.commit_for_read(thread).await?;
        let thread_id = awaken_agent_contract::agent::thread::Id(thread.to_owned());
        let state = awaken_ext_goal::state::ThreadOutcomeState::new(
            &thread_id,
            commit.as_ref(),
            commit.as_ref(),
        );
        Controller::committed_projection_from_state(&state, &Id(outcome_id.to_owned()))
            .await
            .map_err(crate::outcome_controller_error)
    }
}

/// Join the parent-thread delegation relationship with the child thread's
/// authoritative recovery snapshot. Child Runs intentionally own a separate
/// thread, so the parent's consistent snapshot cannot contain their ticket.
pub async fn authoritative_child_ticket(
    commit: &SessionCommit,
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
    parent_call_id: Option<&str>,
) -> Result<Option<ResumeTicket>, RunError> {
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
        .map_err(|error| RunError::internal(error.to_string()))?;
    recovery_ticket(&committed, &child_run_id)
}

/// Project the one externally answerable tool from a committed ticket. A
/// delegation owns a child ticket when the child is local; remote A2A may
/// instead expose opaque input through the parent ticket. Both foreground
/// completion and later protocol reads must use this same classification.
pub async fn visible_pending_tool(
    commit: &SessionCommit,
    ticket: &ResumeTicket,
    registry: Option<&awaken_agent_contract::agent::delegation::DelegationRegistry>,
) -> Result<Option<Pending>, RunError> {
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

/// Committed-run recovery a registered Worker's claim reads, through the
/// parent Session partition the guarded dispatch selects.
#[async_trait::async_trait]
impl RunRecoverySource for CommittedReads {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.recovery_snapshot_in_session(thread_id, thread_id, claimed_run_id)
            .await
    }

    async fn recovery_snapshot_in_session(
        &self,
        session_thread_id: &ThreadId,
        thread_id: &ThreadId,
        claimed_run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.session_thread_run_recovery_snapshot(&session_thread_id.0, thread_id, claimed_run_id)
            .await
            .map_err(|error| RecoveryError::Rejected(error.to_string()))
    }
}
