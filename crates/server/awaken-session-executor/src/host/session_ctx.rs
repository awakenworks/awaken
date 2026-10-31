// @formal entrypoint acp.tool_export_requires_claim_execution_environment
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_run.reservation_never_executes
//! Per-thread runtime context ([`SessionCtx`]) owned by [`ExecutorComponents`].

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Derived validity key for a process-local Runtime cache. It includes exactly
/// the frozen publication inputs captured by Runtime plugins; claim/run identity
/// remains attempt context and must not replace the live foreground Worker Arc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimePublicationIdentity {
    pub(crate) publication_fingerprint: String,
}

/// Process-local authority carried by a cached Session context. Coordination
/// may freeze and observe a durable Run but must never execute it; only a
/// context rebuilt at an execution boundary may own physical effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeContextPurpose {
    CoordinationEnvelope,
    Execution,
}

/// The one closed decision for borrowing or creating a physical Session
/// Environment while constructing a Runtime context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeEnvironmentAdmission {
    Omit,
    Reuse,
    Create,
    RejectMissingBinding,
}

/// Select physical Environment ownership before any provider effect occurs.
/// Coordination always remains environment-free, including when an independently
/// owned Environment survived a Runtime-cache eviction.
#[must_use]
pub(crate) const fn runtime_environment_admission(
    purpose: RuntimeContextPurpose,
    retained: bool,
    a2a_only: bool,
    expected_binding: bool,
    can_defer: bool,
) -> RuntimeEnvironmentAdmission {
    if matches!(purpose, RuntimeContextPurpose::CoordinationEnvelope) || a2a_only {
        RuntimeEnvironmentAdmission::Omit
    } else if retained {
        RuntimeEnvironmentAdmission::Reuse
    } else if expected_binding {
        RuntimeEnvironmentAdmission::RejectMissingBinding
    } else if can_defer {
        RuntimeEnvironmentAdmission::Omit
    } else {
        RuntimeEnvironmentAdmission::Create
    }
}

/// One closed decision for publishing executable ACP tool routes. Coordination
/// contexts may inspect durable state but never expose physical effects.
#[must_use]
pub(crate) const fn acp_tool_export_admitted(
    purpose: RuntimeContextPurpose,
    is_acp: bool,
    environment_present: bool,
) -> bool {
    matches!(purpose, RuntimeContextPurpose::Execution) && is_acp && environment_present
}

#[cfg(kani)]
/// @obligation acp.tool_export_requires_claim_execution_environment
/// @proof-input-debt free_symbol acp_tool_export_admitted environment_present
#[kani::proof]
fn acp_tool_export_requires_claim_execution_environment() {
    // Causes: C1 coordination/execution purpose; C2 ACP/native backend; C3 a
    // realized Environment. Effects: E1 export executable ACP tools, E2 omit.
    // Decision table: R1 execution+ACP+environment=>E1; every other one of the
    // eight combinations=>E2. This proves coordination cannot borrow effects,
    // native execution cannot acquire ACP routes, and unrealized execution
    // cannot publish a route whose executor substrate does not exist.
    let purpose = if kani::any::<bool>() {
        RuntimeContextPurpose::CoordinationEnvelope
    } else {
        RuntimeContextPurpose::Execution
    };
    let is_acp = kani::any::<bool>();
    let environment_present = kani::any::<bool>();
    assert_eq!(
        acp_tool_export_admitted(purpose, is_acp, environment_present),
        purpose == RuntimeContextPurpose::Execution && is_acp && environment_present
    );
}

#[cfg(kani)]
/// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
/// @obligation session_run.reservation_never_executes
/// @proof-input-debt free_symbol runtime_environment_admission expected_binding
/// @proof-input-debt free_symbol runtime_environment_admission retained
#[kani::proof]
fn runtime_environment_admission_is_total_and_coordination_never_borrows() {
    // Causes: C1 coordination/execution purpose; C2 a resident Environment;
    // C3 remote-only execution; C4 a durable binding; C5 policy permits lazy
    // realization. Effects: E1 omit, E2 reuse, E3 create, E4 reject a missing
    // adopted binding. Constraints: an earlier topology check rejects C2+C3;
    // coordination must choose E1 for every C2-C5 combination. Rules:
    // R1 C1=coordination=>E1; R2 execution+C3=>E1;
    // R3 execution+C2+!C3=>E2; R4 execution+!C2+!C3+C4=>E4;
    // R5 execution+!C2+!C3+!C4+C5=>E1; otherwise E3.
    let purpose = if kani::any::<bool>() {
        RuntimeContextPurpose::CoordinationEnvelope
    } else {
        RuntimeContextPurpose::Execution
    };
    let retained = kani::any::<bool>();
    let a2a_only = kani::any::<bool>();
    let expected_binding = kani::any::<bool>();
    let can_defer = kani::any::<bool>();
    let selected =
        runtime_environment_admission(purpose, retained, a2a_only, expected_binding, can_defer);

    if purpose == RuntimeContextPurpose::CoordinationEnvelope {
        assert_eq!(selected, RuntimeEnvironmentAdmission::Omit);
    } else if a2a_only {
        assert_eq!(selected, RuntimeEnvironmentAdmission::Omit);
    } else if retained {
        assert_eq!(selected, RuntimeEnvironmentAdmission::Reuse);
    } else if expected_binding {
        assert_eq!(selected, RuntimeEnvironmentAdmission::RejectMissingBinding);
    } else if can_defer {
        assert_eq!(selected, RuntimeEnvironmentAdmission::Omit);
    } else {
        assert_eq!(selected, RuntimeEnvironmentAdmission::Create);
    }
}

/// Closed cache-admission kernel. Publication equality is necessary but not
/// sufficient: a coordination envelope deliberately omits execution
/// realization, so an execution request must rebuild even when every immutable
/// publication fingerprint is equal.
#[must_use]
pub(crate) const fn runtime_context_cache_reuse_admitted(
    cache_matches_attempt: bool,
    requested: RuntimeContextPurpose,
    cached: RuntimeContextPurpose,
) -> bool {
    cache_matches_attempt
        && !matches!(
            (requested, cached),
            (
                RuntimeContextPurpose::Execution,
                RuntimeContextPurpose::CoordinationEnvelope
            )
        )
}

#[cfg(kani)]
/// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
/// @obligation session_run.reservation_never_executes
#[kani::proof]
fn coordination_envelope_never_satisfies_execution_cache_admission() {
    let cache_matches_attempt = kani::any::<bool>();
    let requested = if kani::any::<bool>() {
        RuntimeContextPurpose::CoordinationEnvelope
    } else {
        RuntimeContextPurpose::Execution
    };
    let cached = if kani::any::<bool>() {
        RuntimeContextPurpose::CoordinationEnvelope
    } else {
        RuntimeContextPurpose::Execution
    };
    let admitted = runtime_context_cache_reuse_admitted(cache_matches_attempt, requested, cached);

    assert_eq!(
        admitted,
        cache_matches_attempt
            && !(requested == RuntimeContextPurpose::Execution
                && cached == RuntimeContextPurpose::CoordinationEnvelope)
    );
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
        let publication_fingerprint = awaken_agent_contract::content_fingerprint(&(
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
/// `InstalledSession`.
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
    pub(crate) commit: Arc<SessionCommit>,
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
    pub(crate) claimed_worker: Arc<awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>>,
    pub(crate) runtime_publication_identity: Option<RuntimePublicationIdentity>,
    pub(crate) purpose: RuntimeContextPurpose,
    pub(crate) config: ExecutableAgentSnapshot,
    pub(crate) commit: Arc<SessionCommit>,
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
    pub(crate) _acp_tool_exports:
        Vec<awaken_session_environment_contract::tool_export::AcpToolExport>,
    pub(crate) thread_id: ThreadId,
    /// The thread's sandbox environment, reused to build an Outcome Worker runtime
    /// for `define_outcome` (same tools, same environment).
    pub(crate) env: Option<Arc<crate::session_environment::SessionEnvironment>>,
    /// The thread's skill registry (delivered + workspace), used to expand user
    /// `/skill-name` invocations. `None` when skills are not offered.
    pub(crate) skill_registry: Option<Arc<dyn SkillRegistry>>,
    /// The foreground Run this Session drives and the signal that interrupts
    /// it, written together. A concurrent `interrupt` (a separate request)
    /// observes both at once; neither the run loop nor the execution lock holds
    /// this brief lock, so interrupt never waits for model or tool IO. Direct
    /// execution uses the signal; durable execution uses the identity to
    /// persist a cancellation intent for whichever pool worker owns the claim.
    pub(crate) foreground: Arc<ForegroundRunRecord>,
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
    /// The resident Runtime as the live control durable cancellation nudges.
    pub(crate) fn live_control(
        &self,
    ) -> &(dyn awaken_runtime_contract::control::LiveRunControl + Send + Sync) {
        self.runtime.as_ref()
    }

    /// Whether a foreground Run is admitted. An unreadable record counts as
    /// active: it can never prove quiescence or let a caller detach a Run.
    pub(crate) fn has_active_run(&self) -> bool {
        self.foreground.has_run().unwrap_or(true)
    }

    pub(crate) fn is_coordination_envelope(&self) -> bool {
        let is_coordination_envelope = self.purpose == RuntimeContextPurpose::CoordinationEnvelope;
        debug_assert!(
            !is_coordination_envelope || self.env.is_none(),
            "coordination envelopes must remain physically unrealized"
        );
        is_coordination_envelope
    }

    pub(crate) fn resume_activation(
        &self,
        ticket: &awaken_agent_contract::agent::awaiting::ResumeTicket,
    ) -> RunActivation {
        awaken_session_run_delivery::resume_activation(
            self.thread_id.clone(),
            self.config.clone(),
            ticket,
        )
    }

    /// A run context for one attempt of `run`, carrying a fresh cancellation
    /// signal installed with `run` in the foreground record so a concurrent
    /// `interrupt` can cancel the attempt it drives. The ingress owns
    /// per-Thread execution exclusivity; `command` only keeps Host admission and
    /// this process-local control record ordered around that ingress boundary.
    pub(crate) fn context(&self, run: &RunId) -> Result<RuntimeRunContext, ForegroundRunPoisoned> {
        let token = self.foreground.arm(run)?;
        let context = self
            .attempt_context
            .clone()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
            .with_cancellation(token);
        Ok(match &self.stream_checkpoint {
            Some(checkpoint) => context.with_stream_checkpoint(checkpoint.clone()),
            None => context,
        })
    }
}

#[cfg(test)]
mod cache_admission_tests {
    use super::*;

    #[test]
    fn runtime_context_cache_admission_decision_table_is_exact() {
        // Causes: C1 publication/attempt identity matches; C2 request is
        // coordination/execution; C3 cached context is coordination/execution.
        // Effects: E1 reuse, E2 rebuild. Constraint: coordination may reuse an
        // execution context read-only, but execution can never reuse an
        // envelope that was intentionally built without physical authority.
        // Rules: R1 !C1=>E2; R2 C1+coordination+any=>E1;
        // R3 C1+execution+execution=>E1;
        // R4 C1+execution+coordination=>E2.
        for cache_matches_attempt in [false, true] {
            for requested in [
                RuntimeContextPurpose::CoordinationEnvelope,
                RuntimeContextPurpose::Execution,
            ] {
                for cached in [
                    RuntimeContextPurpose::CoordinationEnvelope,
                    RuntimeContextPurpose::Execution,
                ] {
                    let expected = cache_matches_attempt
                        && !(requested == RuntimeContextPurpose::Execution
                            && cached == RuntimeContextPurpose::CoordinationEnvelope);
                    assert_eq!(
                        runtime_context_cache_reuse_admitted(
                            cache_matches_attempt,
                            requested,
                            cached,
                        ),
                        expected,
                        "C1={cache_matches_attempt} C2={requested:?} C3={cached:?}"
                    );
                }
            }
        }
    }
}
