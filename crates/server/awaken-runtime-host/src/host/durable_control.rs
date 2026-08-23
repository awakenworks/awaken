//! Neutral durable-control operations exposed to Coordinator interfaces.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::control::{LiveCommand, LiveRunControl};
use awaken_runtime_contract::resume::ResumeResult;

use super::{HostError, SharedHost};

impl SharedHost {
    /// Persist one exact cancellation through the dispatch authority, then nudge
    /// the already-registered local attempt, if this Host owns it.
    ///
    /// The durable bit is the authority and must cross its atomic boundary first.
    /// Runtime delivery is only a process-local accelerator for the old, now-fenced
    /// claim; `NotActive` therefore cannot undo an accepted cancellation. Keeping
    /// this composition in one Host seam prevents the Managed Event, durable-op,
    /// and terminal-quiescence callers from drifting into different orderings.
    pub(crate) async fn persist_dispatch_cancellation(
        &self,
        run_id: &RunId,
        live_runtime: Option<&Runtime>,
    ) -> Result<bool, HostError> {
        use awaken_run_ingress::DispatchQueue as _;

        let accepted = if let Some(pool) = self.dispatch_pool.get() {
            pool.cancel(run_id)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
        } else {
            self.dispatch_store()?
                .cancel(run_id)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?
                .is_some()
        };
        if accepted && let Some(runtime) = live_runtime {
            let _ = runtime.deliver(LiveCommand::Cancel {
                run_id: run_id.clone(),
            });
        }
        Ok(accepted)
    }

    /// Cancel a run by id through the durable live-control seam (ADR-0018, slice E
    /// follow-up): records the durable intent first, then nudges an in-flight local
    /// attempt while the pool commits the terminal `Cancelled` fact. Fail-closed:
    /// an unknown run id errors rather than silently succeeding.
    pub async fn cancel_durable(&self, thread: &str, run_id: &str) -> Result<(), HostError> {
        let run_id = RunId(run_id.to_owned());
        // Resolve only an already-resident runtime. Cancellation must never open
        // a Session, resolve current config, or touch its sandbox merely to stop
        // the exact durable attempt.
        let resident = self
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten();

        // This operational surface retains its existing local-pool requirement;
        // the shared helper below owns only ordering, not topology expansion.
        self.dispatch_pool_or_err()?;
        let cancelled = self
            .persist_dispatch_cancellation(
                &run_id,
                resident.as_ref().map(|ctx| ctx.runtime.as_ref()),
            )
            .await?;
        if !cancelled {
            return Err(HostError::bad_request(format!(
                "run not found: {}",
                run_id.0
            )));
        }
        Ok(())
    }

    /// Wake a live run by id through the durable live-control seam (ADR-0018): a
    /// live-only nudge. Fail-closed — no live subscriber is a hard error (G5).
    pub async fn wake_durable(&self, thread: &str, run_id: &str) -> Result<(), HostError> {
        self.durable_ingress(thread)
            .await?
            .live_control()
            .wake(run_id)
            .await
            .map_err(|e| HostError::bad_request(e.to_string()))
    }

    /// Pause an active run at its next safe boundary. Acceptance is live-only;
    /// the resulting `ManualPause` ticket is committed durably by the executor.
    pub async fn pause_durable(
        &self,
        thread: &str,
        requested_run_id: Option<&str>,
    ) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        // An omitted Run id addresses the one exact claim this Runtime currently
        // owns. Foreground request lifetime is not execution ownership: queued,
        // remote, idle and stale attempts therefore all fail closed here.
        let run_id = match requested_run_id {
            Some(run_id) => run_id.to_string(),
            None => ctx
                .runtime
                .active_attempt_run_id(&ctx.thread_id)
                .await
                .map(|run_id| run_id.0)
                .ok_or_else(|| HostError::bad_request("thread has no locally owned active run"))?,
        };
        ctx.durable_ingress
            .as_ref()
            .ok_or_else(|| HostError::bad_request("pause requires durable ingress"))?
            .live_control()
            .pause(&run_id)
            .await
            .map_err(|e| HostError::bad_request(e.to_string()))?;
        Ok(run_id)
    }

    /// Stage a durable cross-thread delivery answering `thread`'s awaiting run, then
    /// let the daemon relay it (ADR-0017, slice E follow-up). Resolves the awaiting
    /// run's awaiting ticket from committed truth, stages a decision into the outbox
    /// via `DispatchService::send`, and the daemon relays it to the run's pending
    /// input and wakes it. Exercises the outbox stage→relay path. Requires the
    /// daemon and an awaiting run.
    pub async fn stage_decision(&self, thread: &str, allow: bool) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        let pool = self.dispatch_pool_or_err()?;
        let thread_id = ThreadId(thread.to_string());
        let (run_id, ticket) = ctx
            .commit
            .open_wait_for_thread(&thread_id)
            .await
            .map_err(HostError::internal)?
            .ok_or_else(|| {
                HostError::bad_request("no awaiting run on this thread to deliver to")
            })?;
        let input = awaken_run_ingress::PendingInput {
            message_id: awaken_runtime::fresh_process_id("xthread"),
            run_id: run_id.clone(),
            thread_id,
            correlation_id: ticket.correlation_id,
            available_at_ms: None,
            context_messages: Vec::new(),
            result: if allow {
                ResumeResult::allow()
            } else {
                ResumeResult::deny(None)
            },
        };
        pool.send(input)
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(run_id.0)
    }

    /// RunResume the thread's durable operator pause with text. The committed ticket
    /// is the authority: tool/auth waits are rejected here and must use their
    /// protocol-specific result/decision surface.
    pub async fn stage_manual_resume(
        &self,
        thread: &str,
        text: String,
    ) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        let pool = self.dispatch_pool_or_err()?;
        let thread_id = ThreadId(thread.to_string());
        let (run_id, ticket) = ctx
            .commit
            .open_wait_for_thread(&thread_id)
            .await
            .map_err(HostError::internal)?
            .ok_or_else(|| HostError::bad_request("no awaiting run on this thread to resume"))?;
        if ticket.reason() != awaken_agent_contract::agent::awaiting::AwaitReason::ManualPause {
            return Err(HostError::bad_request(
                "durable text resume requires a manual-pause ticket",
            ));
        }
        pool.send(awaken_run_ingress::PendingInput {
            message_id: awaken_runtime::fresh_process_id("manual-resume"),
            run_id: run_id.clone(),
            thread_id,
            correlation_id: ticket.correlation_id,
            available_at_ms: None,
            context_messages: Vec::new(),
            result: ResumeResult::Input(text),
        })
        .await
        .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(run_id.0)
    }
}
