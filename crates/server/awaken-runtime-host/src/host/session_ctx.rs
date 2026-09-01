//! Per-thread runtime context ([`SessionCtx`]) owned by [`SharedHost`].

use super::*;

/// Derived validity key for a process-local Runtime cache. It includes exactly
/// the frozen publication inputs captured by Runtime plugins; claim/run identity
/// remains attempt context and must not replace the live foreground Worker Arc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimePublicationIdentity {
    pub(crate) publication_fingerprint: String,
}

impl RuntimePublicationIdentity {
    pub(crate) fn from_publications(
        root: &ExecutableAgentSnapshot,
        non_root: &[ExecutableAgentSnapshot],
        effective_model_ref: &str,
    ) -> Self {
        let mut targets = non_root
            .iter()
            .map(|snapshot| {
                (
                    snapshot.root_agent_id.0.clone(),
                    snapshot.fingerprint.0.clone(),
                )
            })
            .collect::<Vec<_>>();
        targets.sort_unstable();
        let publication_fingerprint = awaken_runtime_contract::content_fingerprint(&(
            root.fingerprint.0.as_str(),
            effective_model_ref,
            targets,
        ))
        .expect("Runtime publication cache inputs are serializable");
        Self {
            publication_fingerprint,
        }
    }
}

/// Ephemeral input reconstructed from one guarded `RunDispatch`. Publication
/// snapshots are deliberately passed down the call stack rather than cached in
/// `SessionRuntimeSlot`.
#[derive(Clone)]
pub(crate) struct ClaimedRuntimeInput {
    pub(crate) identity: RuntimePublicationIdentity,
    pub(crate) publications: Arc<awaken_runtime_contract::StaticPublishedAgentSnapshots>,
    pub(crate) effective_model_ref: String,
}

/// Session-scoped physical capabilities borrowed by a separately dispatched
/// child. It intentionally contains no parent Runtime or Agent plugins.
pub(crate) struct ChildExecutionSubstrate {
    pub(crate) environment: Arc<crate::session_environment::SessionEnvironment>,
    pub(crate) commit: Arc<HostCommit>,
    pub(crate) attempt_context: awaken_runtime_contract::RuntimeRunContext,
}

/// The one foreground-delivery choice for a materialized Thread.
///
/// This replaces the former trait object, boolean, and optional concrete durable
/// ingress that all encoded the same decision. Durable execution is admitted by
/// `RunDispatch`; direct execution alone carries an inline attempt driver.
pub(crate) enum ForegroundRunDelivery {
    Direct(Arc<DirectAttemptDriver>),
    Durable,
}

impl ForegroundRunDelivery {
    pub(crate) fn is_durable(&self) -> bool {
        matches!(self, Self::Durable)
    }

    pub(crate) fn direct(&self) -> Option<&Arc<DirectAttemptDriver>> {
        match self {
            Self::Direct(driver) => Some(driver),
            Self::Durable => None,
        }
    }
}

/// One thread's live state: an isolated runtime, its config, its commit
/// coordinator (the source of committed truth), its sandbox root, and its
/// process-local coordination locks.
pub(crate) struct SessionCtx {
    pub(crate) runtime: Arc<Runtime>,
    /// One authoritative foreground delivery choice; no parallel boolean or
    /// erased/concrete pair is retained.
    pub(crate) delivery: ForegroundRunDelivery,
    /// The one fully configured Worker that executes a claim already accepted by
    /// this process's dispatch authority. Foreground delivery durability is an
    /// independent choice: durable contexts share this exact `Arc` with their
    /// durable coordinator, while direct contexts retain it only for pool-routed
    /// work such as asynchronous Session continuations.
    pub(crate) claimed_worker: Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>,
    pub(crate) runtime_publication_identity: Option<RuntimePublicationIdentity>,
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
    /// Session-lifetime leases for every Host-owned tool exported to an ACP
    /// workload (WebSearch plus semantic Skill/Memory tools).
    pub(crate) _acp_tool_exports: Vec<crate::AcpToolExport>,
    pub(crate) thread_id: ThreadId,
    /// The thread's sandbox environment, reused to build an Outcome Worker runtime
    /// for `define_outcome` (same tools, same environment).
    pub(crate) env: Option<Arc<crate::session_environment::SessionEnvironment>>,
    /// The thread's skill registry (delivered + workspace), used to expand user
    /// `/skill-name` invocations. `None` when skills are not offered.
    pub(crate) skill_registry: Option<Arc<dyn SkillRegistry>>,
    /// The in-flight run's cancellation token, so a concurrent `interrupt` (a
    /// separate request) can cancel it. A plain `std::sync::Mutex` (brief locks),
    /// held by neither the run loop nor the execution lock, so interrupt never
    /// waits for model or tool IO.
    pub(crate) cancel: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    /// Stable identity of the foreground Run currently being driven. Direct
    /// execution uses `cancel`; durable execution uses this id to persist a
    /// cancellation intent for whichever pool worker owns the claim.
    pub(crate) active_run: std::sync::Mutex<Option<RunId>>,
    /// Serializes publication of committed step projections to the process-local
    /// protocol hub. It contains no execution position: awaiting truth is read
    /// exclusively from the committed Thread view.
    pub(crate) projection: tokio::sync::Mutex<()>,
    /// Serializes Outcome commands without holding ordinary Session position
    /// state across Worker/Judge IO. `interrupt` never takes this lock.
    pub(crate) outcome: tokio::sync::Mutex<()>,
    /// Orders same-process Host commands that perform multi-read admission
    /// checks before entering Direct or durable Run ingress. It is not physical
    /// execution authority: Direct ingress owns its per-Thread gate and durable
    /// ingress owns the persisted physical-attempt slot across replicas.
    pub(crate) command: tokio::sync::Mutex<()>,
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
    /// a concurrent `interrupt` can cancel the run it drives. The ingress owns
    /// per-Thread execution exclusivity; `command` only keeps Host admission and
    /// this process-local control slot ordered around that ingress boundary.
    pub(crate) fn context(&self) -> RuntimeRunContext {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex poisoned") = Some(token.clone());
        let context = self
            .attempt_context
            .clone()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
            .with_cancellation(token);
        match &self.stream_checkpoint {
            Some(checkpoint) => context.with_stream_checkpoint(checkpoint.clone()),
            None => context,
        }
    }
}
