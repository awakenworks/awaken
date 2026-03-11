use super::{AgentLoopError, RunIdentity, StateCommitError, StateCommitter};
use crate::contracts::storage::{RunOrigin, VersionPrecondition};
use crate::contracts::thread::CheckpointReason;
use crate::contracts::{RunContext, RunMeta, TerminationReason, ThreadChangeSet};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct ChannelStateCommitter {
    tx: tokio::sync::mpsc::UnboundedSender<ThreadChangeSet>,
    version: Arc<AtomicU64>,
}

impl ChannelStateCommitter {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<ThreadChangeSet>) -> Self {
        Self {
            tx,
            version: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[async_trait]
impl StateCommitter for ChannelStateCommitter {
    async fn commit(
        &self,
        _thread_id: &str,
        changeset: ThreadChangeSet,
        _precondition: VersionPrecondition,
    ) -> Result<u64, StateCommitError> {
        let next_version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        self.tx
            .send(changeset)
            .map_err(|e| StateCommitError::new(format!("channel state commit failed: {e}")))?;
        Ok(next_version)
    }
}

pub(super) async fn commit_pending_delta(
    run_ctx: &mut RunContext,
    reason: CheckpointReason,
    force: bool,
    state_committer: Option<&Arc<dyn StateCommitter>>,
    run_identity: &RunIdentity,
    termination: Option<&TerminationReason>,
) -> Result<(), AgentLoopError> {
    let Some(committer) = state_committer else {
        return Ok(());
    };

    let delta = run_ctx.take_delta();
    if !force && delta.is_empty() {
        return Ok(());
    }

    // On RunFinished, write a full state snapshot to bound the action/patch
    // replay window to a single run.
    let snapshot = if reason == CheckpointReason::RunFinished {
        match run_ctx.snapshot() {
            Ok(state) => Some(state),
            Err(e) => {
                tracing::warn!(error = %e, "failed to compute RunFinished snapshot; continuing without snapshot");
                None
            }
        }
    } else {
        None
    };

    let mut changeset = ThreadChangeSet::from_parts(
        run_identity.run_id.clone(),
        run_identity.parent_run_id.clone(),
        reason,
        delta.messages,
        delta.patches,
        delta.state_actions,
        snapshot,
    );

    // Loop always emits run-finished RunMeta. Whether this metadata is used to
    // materialize/maintain durable run mappings is decided by the outer
    // orchestration layer's StateCommitter policy.
    if let Some(termination) = termination {
        let agent_id = run_identity.agent_id.clone();
        let origin: RunOrigin = run_identity.origin;
        let parent_thread_id = None; // Already set on the initial changeset.
        let (status, termination_code, termination_detail) = map_termination(termination);
        changeset.run_meta = Some(RunMeta {
            agent_id,
            origin,
            status,
            parent_thread_id,
            termination_code,
            termination_detail,
            source_mailbox_entry_id: None,
        });
    }

    let precondition = VersionPrecondition::Exact(run_ctx.version());
    let committed_version = committer
        .commit(run_ctx.thread_id(), changeset, precondition)
        .await
        .map_err(|e| AgentLoopError::StateError(format!("state commit failed: {e}")))?;
    run_ctx.set_version(committed_version, Some(super::current_unix_millis()));
    Ok(())
}

fn map_termination(
    termination: &TerminationReason,
) -> (
    crate::contracts::storage::RunStatus,
    Option<String>,
    Option<String>,
) {
    let (status, _) = termination.to_run_status();
    match termination {
        TerminationReason::NaturalEnd => (status, Some("natural".to_string()), None),
        TerminationReason::BehaviorRequested => {
            (status, Some("behavior_requested".to_string()), None)
        }
        TerminationReason::Suspended => (status, Some("input_required".to_string()), None),
        TerminationReason::Cancelled => (status, Some("cancelled".to_string()), None),
        TerminationReason::Error(message) => {
            (status, Some("error".to_string()), Some(message.clone()))
        }
        TerminationReason::Stopped(stopped) => (
            status,
            Some(stopped.code.trim().to_ascii_lowercase()),
            stopped.detail.clone(),
        ),
    }
}

pub(super) struct PendingDeltaCommitContext<'a> {
    run_identity: &'a RunIdentity,
    state_committer: Option<&'a Arc<dyn StateCommitter>>,
}

impl<'a> PendingDeltaCommitContext<'a> {
    pub(super) fn new(
        run_identity: &'a RunIdentity,
        state_committer: Option<&'a Arc<dyn StateCommitter>>,
    ) -> Self {
        Self {
            run_identity,
            state_committer,
        }
    }

    pub(super) async fn commit(
        &self,
        run_ctx: &mut RunContext,
        reason: CheckpointReason,
        force: bool,
    ) -> Result<(), AgentLoopError> {
        commit_pending_delta(
            run_ctx,
            reason,
            force,
            self.state_committer,
            self.run_identity,
            None,
        )
        .await
    }

    pub(super) async fn commit_run_finished(
        &self,
        run_ctx: &mut RunContext,
        termination: &TerminationReason,
    ) -> Result<(), AgentLoopError> {
        commit_pending_delta(
            run_ctx,
            CheckpointReason::RunFinished,
            true,
            self.state_committer,
            self.run_identity,
            Some(termination),
        )
        .await
    }
}
