//! Per-thread live session state ([`SessionState`]) and its runtime context
//! bundle ([`SessionCtx`]) — the mutable position and isolated runtime a
//! [`SharedHost`] keeps for each thread id.

use super::*;

/// A thread's mutable position: the run awaiting a decision (if any) and the
/// system messages buffered for the next turn.
#[derive(Default)]
pub(crate) struct SessionState {
    pub(crate) awaiting_run: Option<RunId>,
    pub(crate) pending_system: Vec<String>,
    /// The distinct compaction-fold count at the current turn's start. A fold
    /// during the turn grows it; the terminal step compares against this baseline
    /// to surface the `agent.thread_context_compacted` marker once (spanning a
    /// awaiting→resumed turn, which shares this baseline).
    pub(crate) compactions_before: usize,
}

/// One thread's live state: an isolated runtime, its config, its commit
/// coordinator (the source of committed truth), its sandbox root, and its
/// position.
pub(crate) struct SessionCtx {
    pub(crate) runtime: Arc<Runtime>,
    /// The delivery seam a turn's execution goes through (slice C): `DirectRunIngress`
    /// by default; a `DurableRunIngress` when durable dispatch is enabled (slice D).
    /// Both drive the same `runtime`/`commit`; only the delivery guarantees differ.
    pub(crate) ingress: Arc<dyn RunIngress>,
    /// True when `ingress` is durable: a turn is submitted through the dispatch
    /// queue (`submit_background`) rather than executed inline (slice D).
    pub(crate) durable: bool,
    /// The concrete durable ingress, present iff `durable`. Kept alongside the
    /// boxed `ingress` so the ADR-0009 operational verbs (recover / reap /
    /// dead-letter GC / superseding submit — slice E) stay reachable; the boxed
    /// trait object erases them.
    pub(crate) durable_ingress: Option<Arc<DurableRunIngress<AnyDispatchStore>>>,
    pub(crate) config: ExecutableAgentSnapshot,
    pub(crate) commit: Arc<HostCommit>,
    /// Canonical topology-independent attempt capabilities. Direct execution
    /// clones this value; durable ingress receives the same clone and replaces
    /// only its claimed commit/read/cancellation authorities.
    pub(crate) attempt_context: awaken_runtime_contract::RuntimeRunContext,
    /// Committed-terminal Runtime extensions installed for this Thread.
    pub(crate) terminal_observers:
        Vec<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
    /// This thread's interrupted-stream checkpoint store (Phase 3), wired into
    /// every run context so an inference drop flushes durably at its boundary.
    pub(crate) stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
    /// Session lifetime for the ACP-facing projection of the configured
    /// WebSearch RawTool. Native sessions leave this empty.
    pub(crate) _web_search_mcp: Option<crate::AcpToolExport>,
    pub(crate) thread_id: ThreadId,
    /// The thread's sandbox environment, reused to build an Outcome Worker runtime
    /// for `define_outcome` (same tools, same environment).
    pub(crate) env: Option<Arc<crate::session_environment::SessionEnvironment>>,
    /// The thread's skill registry (delivered + workspace), used to expand user
    /// `/skill-name` invocations. `None` when skills are not offered.
    pub(crate) skill_registry: Option<Arc<dyn SkillRegistry>>,
    /// The in-flight run's cancellation token, so a concurrent `interrupt` (a
    /// separate request) can cancel it. A plain `std::sync::Mutex` (brief locks),
    /// held by neither the run loop nor the state lock, so interrupt never blocks
    /// on the loop that holds `state`.
    pub(crate) cancel: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    /// Stable identity of the foreground Run currently being driven. Direct
    /// execution uses `cancel`; durable execution uses this id to persist a
    /// cancellation intent for whichever pool worker owns the claim.
    pub(crate) active_run: std::sync::Mutex<Option<RunId>>,
    /// The in-flight run's transient-retry counter (incremented by the inference
    /// seam on each transparent retry), so `finish_step` can report whether the
    /// turn was auto-recovered (`session.status_rescheduled`). Same brief-lock
    /// discipline as `cancel`; a fresh counter is installed per run in `context`.
    pub(crate) reschedule: std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicU32>>>,
    /// The in-flight run's live inbox plus the previous attempt's unconsumed
    /// leftovers. Same locking discipline as `cancel`; lifecycle and lookup
    /// live in [`crate::live_inbox`].
    pub(crate) live_inbox: std::sync::Mutex<crate::live_inbox::LiveInboxSlot>,
    pub(crate) state: tokio::sync::Mutex<SessionState>,
    /// Serializes Outcome commands without holding ordinary Session position
    /// state across Worker/Judge IO. `interrupt` never takes this lock.
    pub(crate) outcome: tokio::sync::Mutex<()>,
    /// One externally executing Run at a time on the Worker Thread. Position
    /// state is locked only for short reads/writes, never across model/tool IO.
    pub(crate) execution: tokio::sync::Mutex<()>,
}

impl SessionCtx {
    pub(crate) fn resume_activation(
        &self,
        ticket: &awaken_agent_contract::agent::awaiting::ResumeTicket,
    ) -> RunActivation {
        let mut activation = RunActivation::new(
            ticket.run_id.clone(),
            self.thread_id.clone(),
            self.config.clone(),
            Vec::new(),
        );
        activation.delegation_origin = ticket.delegation_origin.clone();
        activation.data_subject_id = ticket
            .data_subject_id
            .clone()
            .map(awaken_runtime_contract::DataSubjectId);
        activation
    }

    /// A run context carrying a fresh cancellation token, registered on this ctx so
    /// a concurrent `interrupt` can cancel the run it drives. Only one run is in
    /// flight per thread at a time (the `state` lock serializes them), so the slot
    /// always holds the current run's token.
    pub(crate) fn context(&self) -> RuntimeRunContext {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex poisoned") = Some(token.clone());
        // A fresh transient-retry counter for this run; the inference seam bumps it
        // and `finish_step` reads it to report `session.status_rescheduled`.
        let reschedule = Arc::new(std::sync::atomic::AtomicU32::new(0));
        *self.reschedule.lock().expect("reschedule mutex poisoned") = Some(reschedule.clone());
        let context = self
            .attempt_context
            .clone()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
            .with_cancellation(token)
            .with_reschedules(reschedule);
        match &self.stream_checkpoint {
            Some(checkpoint) => context.with_stream_checkpoint(checkpoint.clone()),
            None => context,
        }
    }
}
