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
    /// How many committed `Continuation` (outcome) rounds have already been
    /// projected, so a second `define_outcome` on the thread reports only its own.
    pub(crate) consumed_rounds: usize,
    /// Cursor for out-of-band memory extraction: the committed-message count that
    /// has already been handed to the extractor, so each turn extracts only the
    /// new messages instead of re-processing (and re-billing) the whole history.
    pub(crate) last_extracted_len: usize,
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
    pub(crate) config: RunnableConfig,
    pub(crate) commit: Arc<HostCommit>,
    /// This thread's interrupted-stream checkpoint store (Phase 3), wired into
    /// every run context so an inference drop flushes durably at its boundary.
    pub(crate) stream_checkpoint: Arc<dyn StreamCheckpointStore>,
    /// Where this session's runs execute tool calls (ADR-0044/0046), cloned from
    /// `SharedHost::hand_placement` at session creation: the session-wide remote hand
    /// and the per-run placement provider, behind one type owning their precedence.
    pub(crate) hand_placement: crate::hand_placement::HandPlacement,
    /// Subject-tagged captured-content sink for this session (ADR-0050), from
    /// the host. `None` = content is recorded to spans only.
    pub(crate) capture_sink: Option<Arc<dyn awaken_runtime_contract::CaptureSink>>,
    pub(crate) thread_id: ThreadId,
    /// The thread's sandbox environment, reused to build a goal-enabled runtime
    /// for `define_outcome` (same tools, same environment).
    pub(crate) env: Arc<LocalSandbox>,
    /// The thread's skill registry (delivered + workspace), used to expand user
    /// `/skill-name` invocations. `None` when skills are not offered.
    pub(crate) skill_registry: Option<Arc<dyn SkillRegistry>>,
    /// The in-flight run's cancellation token, so a concurrent `interrupt` (a
    /// separate request) can cancel it. A plain `std::sync::Mutex` (brief locks),
    /// held by neither the run loop nor the state lock, so interrupt never blocks
    /// on the loop that holds `state`.
    pub(crate) cancel: std::sync::Mutex<Option<CancellationToken>>,
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
}

impl SessionCtx {
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
        let mut ctx = RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
            .with_stream_checkpoint(self.stream_checkpoint.clone())
            .with_cancellation(token)
            .with_reschedules(reschedule)
            // ADR-0050 D5: resolve the content-capture decision for this turn.
            // Open/single-machine reads the env default; managed overrides with
            // the ceiling × request × consent meet.
            .with_capture(crate::redact::env_capture_decision());
        // ADR-0050: attribute captured content to a subject and write it to the
        // sink, when a sink (session-wired or the process-global) and a subject
        // (open surface: AWAKEN_CONTENT_SUBJECT) are set. Content only flows when
        // the capture level permits.
        let sink = self
            .capture_sink
            .clone()
            .or_else(crate::data_subject_api::process_capture_sink);
        if let (Some(sink), Ok(subject)) = (sink, std::env::var("AWAKEN_CONTENT_SUBJECT"))
            && !subject.is_empty()
        {
            ctx = ctx.with_capture_sink(awaken_runtime_contract::DataSubjectId(subject), sink);
        }
        // ADR-0044: route this run's tool calls to the host's remote hand, if one
        // is wired; otherwise the kernel's in-process LocalToolExecutor runs them.
        if let Some(hand) = self.hand_placement.session_hand() {
            ctx = ctx.with_tool_executor(hand.clone());
        }
        ctx
    }

    /// A run context whose tool executor is chosen per run by the host's
    /// [`ToolExecutorProvider`] (ADR-0046). When a provider is installed and
    /// places this run (returns `Some`), its executor overrides the session-wide
    /// `remote_hand`; otherwise this is exactly [`context`](Self::context).
    pub(crate) async fn context_for(
        &self,
        activation: &RunActivation,
    ) -> Result<RuntimeRunContext, awaken_runtime_contract::tool::ToolExecutorSelectionError> {
        let mut ctx = self.context();
        if let Some(executor) = self.hand_placement.placed(activation).await? {
            ctx = ctx.with_tool_executor(executor);
        }
        Ok(ctx)
    }
}
