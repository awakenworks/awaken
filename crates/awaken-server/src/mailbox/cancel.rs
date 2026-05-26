//! Cancellation, interruption, and terminal-dispatch reconciliation for
//! `Mailbox`.
//!
//! All methods stay on `Mailbox` via an additional `impl` block.

use std::time::Instant;

use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::mailbox::{
    LiveDeliveryOutcome, LiveRunCommand, LiveRunTarget, MailboxInterrupt, MailboxInterruptDetails,
    RunDispatch, RunDispatchStatus,
};
use awaken_contract::contract::storage::RunRecord;
use awaken_contract::now_ms;

use super::{
    Mailbox, MailboxError, REMOTE_CANCEL_WAIT_MS, TERMINAL_RECONCILE_BATCH,
    live_target_for_dispatch, live_target_for_run, record_mailbox_dispatch_terminal_metrics,
    record_mailbox_operation_result, result_label,
};

impl Mailbox {
    // ── Control ──────────────────────────────────────────────────────

    /// Cancel a run by dispatch_id or thread_id.
    ///
    /// If Queued: transitions to Cancelled via store.
    /// If Claimed/Running: cancels runtime run via dual-index lookup.
    pub async fn cancel(&self, id: &str) -> Result<bool, MailboxError> {
        // Try store cancel first (works for Queued dispatches).
        let now = now_ms();
        let cancel_start = Instant::now();
        let cancel_result = self.store.cancel(id, now).await;
        record_mailbox_operation_result("cancel", result_label(&cancel_result), cancel_start);
        let cancelled = cancel_result?;
        if let Some(cancelled_dispatch) = cancelled {
            self.mark_cancelled_dispatch_run_cancelled(
                &cancelled_dispatch,
                "queued dispatch cancelled",
            )
            .await;
            self.refresh_dispatch_depth_metrics().await;
            return Ok(true);
        }

        // Try runtime cancel (for Claimed/Running dispatches).
        if self.executor.cancel(id) {
            return Ok(true);
        }

        if let Some(dispatch) = self.store.load_dispatch(id).await?
            && dispatch.status == RunDispatchStatus::Claimed
        {
            return self
                .deliver_live_cancel(&live_target_for_dispatch(&dispatch))
                .await;
        }

        let run = if let Some(run) = self.run_store.load_run(id).await? {
            Some(run)
        } else {
            self.run_store.latest_run(id).await?
        };
        if let Some(run) = run
            && matches!(run.status, RunStatus::Running | RunStatus::Waiting)
        {
            return self.deliver_live_cancel(&live_target_for_run(&run)).await;
        }

        Ok(false)
    }

    /// Interrupt a thread: bump dispatch epoch, supersede all pending,
    /// cancel active run. Clean slate for the thread.
    pub async fn interrupt(&self, thread_id: &str) -> Result<MailboxInterrupt, MailboxError> {
        self.interrupt_detailed(thread_id).await.map(Into::into)
    }

    /// Interrupt a thread and return the exact queued dispatches superseded by
    /// the operation.
    pub async fn interrupt_detailed(
        &self,
        thread_id: &str,
    ) -> Result<MailboxInterruptDetails, MailboxError> {
        let now = now_ms();
        let interrupt_start = Instant::now();
        let interrupt_result = self.store.interrupt_detailed(thread_id, now).await;
        record_mailbox_operation_result(
            "interrupt",
            result_label(&interrupt_result),
            interrupt_start,
        );
        let result = interrupt_result?;
        crate::metrics::inc_mailbox_operation_by("supersede", "ok", result.superseded_count as u64);
        self.refresh_dispatch_depth_metrics().await;
        for superseded_dispatch in &result.superseded_dispatches {
            self.mark_superseded_dispatch_run_cancelled(
                superseded_dispatch,
                "queued dispatch superseded by interrupt",
            )
            .await;
        }

        let Some(active_dispatch) = result.active_dispatch.as_ref() else {
            return Ok(result);
        };

        // Cancel active runtime run if any.
        if self
            .cancel_active_dispatch(thread_id, active_dispatch, false)
            .await?
        {
            self.record_mailbox_dispatch_event("RunInterrupted", active_dispatch)
                .await;
        }

        Ok(result)
    }

    pub(super) async fn cancel_active_dispatch(
        &self,
        thread_id: &str,
        active_dispatch: &RunDispatch,
        wait_for_release: bool,
    ) -> Result<bool, MailboxError> {
        if wait_for_release {
            if self.executor.cancel_and_wait_by_thread(thread_id).await {
                if self
                    .wait_for_dispatch_not_claimed(&active_dispatch.dispatch_id)
                    .await?
                {
                    return Ok(true);
                }
                tracing::warn!(
                    thread_id,
                    dispatch_id = %active_dispatch.dispatch_id,
                    "local cancel completed but active dispatch did not release before foreground submit"
                );
                self.record_mailbox_timeout(
                    active_dispatch,
                    "local_cancel_release_wait",
                    REMOTE_CANCEL_WAIT_MS,
                )
                .await;
                return Ok(false);
            }
        } else if self.executor.cancel(thread_id) {
            return Ok(true);
        }

        if !self
            .deliver_live_cancel(&live_target_for_dispatch(active_dispatch))
            .await?
        {
            return Ok(false);
        }

        if wait_for_release
            && !self
                .wait_for_dispatch_not_claimed(&active_dispatch.dispatch_id)
                .await?
        {
            tracing::warn!(
                thread_id,
                dispatch_id = %active_dispatch.dispatch_id,
                "remote cancel delivered but active dispatch did not release before foreground submit"
            );
            self.record_mailbox_timeout(
                active_dispatch,
                "remote_cancel_release_wait",
                REMOTE_CANCEL_WAIT_MS,
            )
            .await;
            return Ok(false);
        }
        Ok(true)
    }

    async fn deliver_live_cancel(&self, target: &LiveRunTarget) -> Result<bool, MailboxError> {
        match self
            .store
            .deliver_live_to(target, LiveRunCommand::Cancel)
            .await?
        {
            LiveDeliveryOutcome::Delivered => Ok(true),
            LiveDeliveryOutcome::NoSubscriber => Ok(false),
        }
    }

    pub(super) async fn mark_superseded_dispatch_run_cancelled(
        &self,
        dispatch: &RunDispatch,
        reason: &str,
    ) {
        self.mark_dispatch_run_cancelled("mark_run_superseded", "superseded", dispatch, reason)
            .await;
    }

    pub(super) async fn mark_cancelled_dispatch_run_cancelled(
        &self,
        dispatch: &RunDispatch,
        reason: &str,
    ) {
        self.mark_dispatch_run_cancelled("mark_run_cancelled", "cancelled", dispatch, reason)
            .await;
    }

    async fn mark_dispatch_run_cancelled(
        &self,
        operation: &str,
        outcome: &str,
        dispatch: &RunDispatch,
        reason: &str,
    ) {
        let start = Instant::now();
        let result = self
            .mark_dispatch_run_cancelled_inner(dispatch, reason)
            .await;
        record_mailbox_operation_result(operation, result_label(&result), start);
        if matches!(result, Ok(true)) {
            record_mailbox_dispatch_terminal_metrics(dispatch, outcome);
            if outcome == "cancelled" {
                self.record_mailbox_dispatch_event("RunCancelled", dispatch)
                    .await;
            }
        }
        if let Err(error) = result {
            tracing::warn!(
                dispatch_id = %dispatch.dispatch_id,
                run_id = %dispatch.run_id,
                thread_id = %dispatch.thread_id,
                reason,
                error = %error,
                "failed to mark terminal mailbox run as cancelled"
            );
        }
    }

    async fn mark_dispatch_run_cancelled_inner(
        &self,
        dispatch: &RunDispatch,
        _reason: &str,
    ) -> Result<bool, MailboxError> {
        let Some(mut run) = self.run_store.load_run(&dispatch.run_id).await? else {
            return Ok(false);
        };
        if run.thread_id != dispatch.thread_id || run.status == RunStatus::Done {
            return Ok(false);
        }

        let now = now_ms() / 1000;
        run.status = RunStatus::Done;
        run.termination_reason = Some(TerminationReason::Cancelled);
        run.error_payload = None;
        run.dispatch_id = Some(dispatch.dispatch_id.clone());
        run.session_id = dispatch.dispatch_instance_id.clone();
        run.waiting = None;
        run.finished_at = Some(now);
        run.updated_at = now;

        self.checkpoint_terminal_dispatch_run(dispatch, &run)
            .await?;
        Ok(true)
    }

    pub(super) async fn mark_dead_letter_dispatch_run_error(&self, dispatch: &RunDispatch) {
        let start = Instant::now();
        let result = self
            .mark_dead_letter_dispatch_run_error_inner(dispatch)
            .await;
        record_mailbox_operation_result("mark_run_dead_letter", result_label(&result), start);
        if matches!(result, Ok(true)) {
            record_mailbox_dispatch_terminal_metrics(dispatch, "dead_letter");
        }
        if let Err(error) = result {
            tracing::warn!(
                dispatch_id = %dispatch.dispatch_id,
                run_id = %dispatch.run_id,
                thread_id = %dispatch.thread_id,
                error = %error,
                "failed to mark dead-lettered mailbox run as errored"
            );
        }
    }

    pub(super) async fn reconcile_terminal_dispatch(&self, dispatch: &RunDispatch) {
        match dispatch.status {
            RunDispatchStatus::DeadLetter => {
                self.mark_dead_letter_dispatch_run_error(dispatch).await;
            }
            RunDispatchStatus::Cancelled => {
                self.mark_cancelled_dispatch_run_cancelled(
                    dispatch,
                    "cancelled dispatch reclaimed during mailbox maintenance",
                )
                .await;
            }
            RunDispatchStatus::Superseded => {
                self.mark_superseded_dispatch_run_cancelled(
                    dispatch,
                    "superseded dispatch reclaimed during mailbox maintenance",
                )
                .await;
            }
            RunDispatchStatus::Queued | RunDispatchStatus::Claimed | RunDispatchStatus::Acked => {}
        }
    }

    pub(super) async fn reconcile_terminal_dispatches(&self) {
        let mut offset = 0;
        loop {
            let list_start = Instant::now();
            let result = self
                .store
                .list_terminal_dispatches(TERMINAL_RECONCILE_BATCH, offset)
                .await;
            record_mailbox_operation_result(
                "list_terminal_dispatches",
                result_label(&result),
                list_start,
            );
            let dispatches = match result {
                Ok(dispatches) => dispatches,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to list terminal mailbox dispatches for run reconciliation"
                    );
                    return;
                }
            };
            if dispatches.is_empty() {
                return;
            }
            crate::metrics::inc_mailbox_operation_by(
                "reconcile_terminal_dispatch",
                "ok",
                dispatches.len() as u64,
            );
            let page_len = dispatches.len();
            for dispatch in &dispatches {
                self.reconcile_terminal_dispatch(dispatch).await;
            }
            if page_len < TERMINAL_RECONCILE_BATCH {
                return;
            }
            offset += page_len;
        }
    }

    async fn mark_dead_letter_dispatch_run_error_inner(
        &self,
        dispatch: &RunDispatch,
    ) -> Result<bool, MailboxError> {
        let Some(mut run) = self.run_store.load_run(&dispatch.run_id).await? else {
            return Ok(false);
        };
        if run.thread_id != dispatch.thread_id || run.status == RunStatus::Done {
            return Ok(false);
        }

        let reason = dispatch
            .run_error
            .clone()
            .or_else(|| dispatch.last_error.clone())
            .unwrap_or_else(|| "mailbox dispatch dead-lettered".to_string());
        let now = now_ms() / 1000;
        run.status = RunStatus::Done;
        run.termination_reason = Some(TerminationReason::Error(reason.clone()));
        run.error_payload = Some(serde_json::json!({ "message": reason }));
        run.dispatch_id = Some(dispatch.dispatch_id.clone());
        run.session_id = dispatch.dispatch_instance_id.clone();
        run.waiting = None;
        run.finished_at = Some(now);
        run.updated_at = now;

        self.checkpoint_terminal_dispatch_run(dispatch, &run)
            .await?;
        Ok(true)
    }

    async fn checkpoint_terminal_dispatch_run(
        &self,
        dispatch: &RunDispatch,
        run: &RunRecord,
    ) -> Result<(), MailboxError> {
        const MAX_APPEND_ATTEMPTS: usize = 8;
        for _ in 0..MAX_APPEND_ATTEMPTS {
            let messages = self
                .run_store
                .load_committed_messages(&dispatch.thread_id)
                .await?
                .unwrap_or_default();
            let expected_version = messages.len() as u64;
            if self
                .commit_run_append(&dispatch.thread_id, &[], Some(expected_version), run)
                .await?
            {
                self.refresh_worker_checkpoint_cache(&dispatch.thread_id, &messages, run)
                    .await;
                return Ok(());
            }
        }
        Err(MailboxError::Internal(format!(
            "terminal dispatch run checkpoint exhausted {MAX_APPEND_ATTEMPTS} retries under version conflict for thread '{}'",
            dispatch.thread_id
        )))
    }
}
