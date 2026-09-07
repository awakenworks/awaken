//! Claim-bound Runtime projections of one frozen Session.
//!
//! The host remains the sole owner of Session realization. An embedding
//! application supplies no late desired state: mounts, environment values,
//! prompts, and MCP generations come only from the frozen Control projection
//! and are installed into the same Session slot for the Native/ACP backend path.

mod agent_filesystem_context;
mod execution_authority;
mod realization_control_deadline;
mod realization_renewal;

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_dispatch::RunClaim;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{ExecutorCapabilities, RunAttemptExecutor};

use agent_filesystem_context::consistent_agent_filesystem_context;
#[cfg(test)]
pub(crate) use realization_renewal::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS;

/// Already-closed process-local projections awaiting nonterminal cleanup.
/// Dropping this value cannot reopen admission or certify cleanup. A retry
/// reselects the retained slot owners through the same Host operation.
#[must_use = "closed projections still require cleanup or an explicit retry"]
pub struct SessionRealizationRevocation {
    selected: Vec<(
        String,
        crate::session_slot::SessionRealizationRevocationSelection,
    )>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenewalFailureDisposition {
    /// Control proves that this exact local owner may no longer execute.
    RevokeImmediately,
    /// The old lease remains the proof; retry until its durable expiry.
    RetryWhileLeaseLive,
}

/// Resource behavior for the one complete frozen-projection installer.
/// Keeping this typed prevents terminal recovery from entering ordinary
/// materialization while preserving one publication/baseline/lease owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrozenResourceProjectionMode {
    Dispatch,
    Synchronize,
    ValidateInstalled,
    Terminal,
}

impl FrozenResourceProjectionMode {
    const fn synchronizes(self) -> bool {
        matches!(self, Self::Dispatch | Self::Synchronize)
    }

    const fn transition_use(self) -> awaken_session_contract::FrozenResourceTransitionUse {
        if self.synchronizes() {
            awaken_session_contract::FrozenResourceTransitionUse::ApplyEffects
        } else {
            awaken_session_contract::FrozenResourceTransitionUse::ValidateInstalled
        }
    }

    const fn allows_unattempted_amendment(self) -> bool {
        matches!(self, Self::Dispatch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrozenResourceTransitionInstallDecision {
    Stage,
    Replace,
    Reject,
}

/// Decide whether one complete aggregate projection may replace the Runtime's
/// disposable Resource projection. Durable amendment eligibility remains owned
/// by the Session aggregate; this kernel only prevents a claimed realization
/// from projecting different content into the same observed generation.
const fn frozen_resource_generation_install_decision(
    previous_exists: bool,
    exact_replay: bool,
    workspace_matches: bool,
    previous_revision: u64,
    incoming_revision: u64,
    authority_amends_unattempted: bool,
) -> FrozenResourceTransitionInstallDecision {
    use FrozenResourceTransitionInstallDecision::{Reject, Replace, Stage};

    if !previous_exists || exact_replay {
        return Stage;
    }
    if workspace_matches
        && (incoming_revision > previous_revision
            || (authority_amends_unattempted && incoming_revision == previous_revision))
    {
        Replace
    } else {
        Reject
    }
}

/// Project aggregate-owned transition values into the one generation kernel.
/// This adapter contains no independent policy and is the only place where
/// Runtime cache identity is translated into the kernel's complete fact set.
fn frozen_resource_transition_install_decision(
    existing: Option<&awaken_session_contract::SessionResourceTransition>,
    incoming: &awaken_session_contract::SessionResourceTransition,
    authority_amends_unattempted: bool,
) -> FrozenResourceTransitionInstallDecision {
    match existing {
        Some(existing) => frozen_resource_generation_install_decision(
            true,
            existing.desired() == incoming.desired(),
            existing.desired().workspace_id == incoming.desired().workspace_id,
            existing.desired().revision,
            incoming.desired().revision,
            authority_amends_unattempted,
        ),
        None => frozen_resource_generation_install_decision(
            false,
            false,
            false,
            0,
            incoming.desired().revision,
            authority_amends_unattempted,
        ),
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_resource_replacement_requires_newer_or_authority_amended_generation() {
    use FrozenResourceTransitionInstallDecision::{Reject, Replace, Stage};

    let previous_exists: bool = kani::any();
    let exact_replay: bool = kani::any();
    let workspace_matches: bool = kani::any();
    let previous_revision: u64 = kani::any();
    let incoming_revision: u64 = kani::any();
    let mode = match kani::any::<u8>() % 4 {
        0 => FrozenResourceProjectionMode::Dispatch,
        1 => FrozenResourceProjectionMode::Synchronize,
        2 => FrozenResourceProjectionMode::ValidateInstalled,
        _ => FrozenResourceProjectionMode::Terminal,
    };
    let authority_amends_unattempted = mode.allows_unattempted_amendment();
    assert_eq!(
        authority_amends_unattempted,
        mode == FrozenResourceProjectionMode::Dispatch
    );

    let decision = frozen_resource_generation_install_decision(
        previous_exists,
        exact_replay,
        workspace_matches,
        previous_revision,
        incoming_revision,
        authority_amends_unattempted,
    );
    let replacement_authorized = previous_exists
        && !exact_replay
        && workspace_matches
        && (incoming_revision > previous_revision
            || (authority_amends_unattempted && incoming_revision == previous_revision));
    assert_eq!(decision == Replace, replacement_authorized);
    assert_eq!(decision == Stage, !previous_exists || exact_replay);
    assert_eq!(
        decision == Reject,
        !replacement_authorized && previous_exists && !exact_replay
    );
}

fn realization_renewal_failure_disposition(
    error: &awaken_session_contract::SessionRealizationControlFailure,
) -> RenewalFailureDisposition {
    if error.proves_current_realization_cannot_continue() {
        RenewalFailureDisposition::RevokeImmediately
    } else {
        RenewalFailureDisposition::RetryWhileLeaseLive
    }
}

fn failed_realization_invalidates_projection(
    current: &awaken_session_contract::SessionRealizationLease,
    failed: &awaken_session_contract::SessionRealizationLease,
) -> bool {
    awaken_session_contract::realization_lease_generation_authorizes(current, failed)
}

fn terminal_cleanup_drive_error(
    session_id: &str,
    error: awaken_session_contract::SessionRealizationDriveError,
) -> crate::HostError {
    match error {
        awaken_session_contract::SessionRealizationDriveError::Effect(error) => {
            crate::managed_adapter_error::from_run_error(error)
        }
        error => crate::HostError::internal(format!(
            "Session `{session_id}` terminal cleanup remained pending: {error}"
        )),
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_realization_renewal_failure_disposition_is_total_exact_and_fail_closed() {
    use awaken_session_contract::SessionRealizationControlFailure;

    let selector = kani::any::<u8>() % 8;
    let error = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        _ => SessionRealizationControlFailure::Unavailable(String::new()),
    };
    // Keep the proof oracle independent from both the contract disposition and
    // this consumer. Terminal Control truth plus explicit StaleOwnership prove
    // loss; NotReady, Conflict, and Unavailable retain the retry path.
    let expected_retirement = matches!(selector, 0 | 2 | 3 | 4 | 6);
    let disposition = realization_renewal_failure_disposition(&error);

    assert_eq!(
        disposition == RenewalFailureDisposition::RevokeImmediately,
        expected_retirement
    );
    if !expected_retirement {
        assert_eq!(disposition, RenewalFailureDisposition::RetryWhileLeaseLive);
    }
}

#[cfg(test)]
mod realization_renewal_tests {
    use super::*;
    use awaken_session_contract::SessionRealizationControlFailure;

    #[test]
    fn terminal_session_replies_retire_only_the_stale_local_projection() {
        // Renewal decision table: N1 NotFound, N2 Terminal, and N3 Invalid prove the local
        // projection no longer has a renewable frozen Control owner -> retire it
        // quietly; stale ownership is an equally conclusive ownership loss;
        // NotReady/conflict/unavailable retain the still-live lease for retry.
        for error in [
            SessionRealizationControlFailure::NotFound,
            SessionRealizationControlFailure::Retired,
            SessionRealizationControlFailure::Terminal,
            SessionRealizationControlFailure::Invalid("bad target".into()),
        ] {
            assert_eq!(
                realization_renewal_failure_disposition(&error),
                RenewalFailureDisposition::RevokeImmediately
            );
        }
        for error in [
            SessionRealizationControlFailure::NotReady,
            SessionRealizationControlFailure::Conflict,
            SessionRealizationControlFailure::Unavailable("network unavailable".into()),
        ] {
            assert_eq!(
                realization_renewal_failure_disposition(&error),
                RenewalFailureDisposition::RetryWhileLeaseLive
            );
        }
        assert_eq!(
            realization_renewal_failure_disposition(
                &SessionRealizationControlFailure::StaleOwnership
            ),
            RenewalFailureDisposition::RevokeImmediately
        );
    }

    #[test]
    fn failed_realization_invalidates_only_its_local_generation() {
        // Failure-projection cause/effect graph: C1 owner, incarnation, and
        // epoch match; C2 the local expiry is equal/later than the admitted
        // effect; C3 a replacement generation is already installed. Effects:
        // E1 invalidate the failed/renewed local projection; E2 preserve a
        // regressed or replacement projection. The aggregate remains the lease
        // authority and the Runtime keeps no parallel failure state.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | F1 exact | yes | equal | no | E1 |
        // | F2 renewed | yes | later | no | E1 |
        // | F3 regressed | yes | earlier | no | E2 |
        // | F4 replacement | no | any | yes | E2 |
        let failed = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 7,
            expires_at_unix_ms: 10,
        };
        for (rule, current, expected) in [
            ("F1", failed.clone(), true),
            (
                "F2",
                awaken_session_contract::SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..failed.clone()
                },
                true,
            ),
            (
                "F3",
                awaken_session_contract::SessionRealizationLease {
                    expires_at_unix_ms: 9,
                    ..failed.clone()
                },
                false,
            ),
            (
                "F4",
                awaken_session_contract::SessionRealizationLease {
                    epoch: 8,
                    expires_at_unix_ms: 20,
                    ..failed.clone()
                },
                false,
            ),
        ] {
            assert_eq!(
                failed_realization_invalidates_projection(&current, &failed),
                expected,
                "{rule}"
            );
        }
    }
}

/// The single Session-local attempt projection boundary for foreground,
/// durable, Native, ACP, and A2A attempts.
///
/// Mutating only Host state cannot affect an already-serialized activation.
/// Wrapping the authoritative attempt router keeps one mechanism for every
/// topology. The projection applies the complete Session tool replacement and
/// request-only Session context without mutating the retained publication or
/// the durable activation input. Deterministic message ids make a retried
/// attempt byte-for-byte stable while each new attempt reads only the current
/// Resource/MemoryStore/Skill projection.
pub(crate) struct SessionContextAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    slots: crate::session_slot::SessionRuntimeSlots,
    session_id: String,
}

impl SessionContextAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        slots: crate::session_slot::SessionRuntimeSlots,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            slots,
            session_id: session_id.into(),
        }
    }

    fn project(
        &self,
        mut activation: RunActivation,
        context: &mut awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunActivation> {
        if let Some(result) = self
            .slots
            .read(&self.session_id, |slot| {
                slot.tools.as_ref().map(|tools| {
                    crate::config::project_session_tools(&mut activation.snapshot, tools)
                })
            })
            .flatten()
        {
            result.map_err(|error| {
                awaken_runtime_contract::execution::Error::Resolution(format!(
                    "fingerprint Session tool projection: {error}"
                ))
            })?;
        }
        let filesystem = consistent_agent_filesystem_context(&activation.snapshot)?;
        let prompts = self
            .slots
            .prompts(
                &self.session_id,
                filesystem.working_directory.as_deref(),
                filesystem.has_acp,
                filesystem.has_native,
            )
            .map_err(awaken_runtime_contract::execution::Error::Resolution)?;
        if prompts.is_empty() {
            return Ok(activation);
        }
        let already_present = context
            .request_context
            .iter()
            .chain(&activation.input)
            .filter(|message| message.role == Role::System)
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                awaken_agent_contract::agent::content::ContentBlock::Text { text } => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        context.request_context.extend(
            prompts
                .iter()
                .filter(|prompt| !already_present.contains(prompt.as_str()))
                .map(|prompt| {
                    Message::text(
                        MessageId(format!(
                            "session-context:{}",
                            awaken_session_contract::stable_fingerprint(&(
                                self.session_id.as_str(),
                                prompt.as_str(),
                            ))
                        )),
                        Role::System,
                        prompt.clone(),
                    )
                }),
        );
        Ok(activation)
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for SessionContextAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        mut context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        let activation = self.project(activation, &mut context)?;
        self.inner.execute(activation, context).await
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        self.inner.capabilities()
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for SessionContextAttemptExecutor {
    fn physical_attempt_recovery(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::AttemptRecoveryMode {
        self.inner.physical_attempt_recovery(activation, context)
    }

    async fn resume(
        &self,
        activation: RunActivation,
        command: awaken_runtime_contract::resume::ResumeCommand,
        mut context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        let activation = self.project(activation, &mut context)?;
        self.inner.resume(activation, command, context).await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<()> {
        self.inner.cancel(activation, context).await
    }
}

/// Projects process-local ACP attempt context selected during Session assembly.
/// Automatic Memory contributes request-only context; provider-owned tools
/// contribute a secret-free launch plan. Standard Skills and MemoryStore
/// bindings remain on the shared filesystem/semantic-tool delivery path.
pub(crate) struct AcpContextAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    memory: Option<awaken_ext_memory::MemoryRecall>,
    provider_server_tools: Vec<awaken_runtime_contract::resolved::ProviderServerTool>,
}

impl AcpContextAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        memory: Option<awaken_ext_memory::MemoryRecall>,
        provider_server_tools: Vec<awaken_runtime_contract::resolved::ProviderServerTool>,
    ) -> Self {
        Self {
            inner,
            memory,
            provider_server_tools,
        }
    }

    async fn load_context(
        &self,
        activation: &RunActivation,
        context: &mut awaken_runtime_contract::RuntimeRunContext,
    ) {
        if !activation
            .snapshot
            .resolved_spec
            .model_binding
            .backend_ref
            .starts_with("acp:")
        {
            return;
        }
        context
            .provider_server_tools
            .clone_from(&self.provider_server_tools);
        if let Some(memory) = &self.memory
            && let Some(recalled) = memory.context(&activation.input).await
        {
            context.request_context.push(Message::text(
                MessageId(format!("acp-memory:{}", activation.run_id.0)),
                Role::System,
                recalled,
            ));
        }
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for AcpContextAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        mut context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.load_context(&activation, &mut context).await;
        self.inner.execute(activation, context).await
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        self.inner.capabilities()
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for AcpContextAttemptExecutor {
    fn physical_attempt_recovery(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::AttemptRecoveryMode {
        self.inner.physical_attempt_recovery(activation, context)
    }

    async fn resume(
        &self,
        activation: RunActivation,
        command: awaken_runtime_contract::resume::ResumeCommand,
        mut context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.load_context(&activation, &mut context).await;
        self.inner.resume(activation, command, context).await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<()> {
        self.inner.cancel(activation, context).await
    }
}

#[cfg(test)]
mod acp_context_tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::execution::{Error, RunExecutor};
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    struct UnusedExecutor;

    #[async_trait::async_trait]
    impl RunExecutor for UnusedExecutor {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<RunState, Error> {
            panic!("load_context test never executes the inner adapter")
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for UnusedExecutor {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: awaken_runtime_contract::resume::ResumeCommand,
            _context: awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<RunState, Error> {
            panic!("load_context test never resumes the inner adapter")
        }
    }

    fn activation(backend: &str) -> RunActivation {
        RunActivation::new(
            RunId("run-1".into()),
            ThreadId("session-1".into()),
            ExecutableAgentSnapshot::builder("agent")
                .model(ModelBinding::new("provider", "model", backend))
                .build(),
            vec![Message::text(
                MessageId("user-1".into()),
                Role::User,
                "current request",
            )],
        )
    }

    /// Session context FMECA cause/effect graph: C1=frozen prompts exist,
    /// C2=durable User input exists, C3=the exact prompt is already in this
    /// attempt context. Effects: E1=append one request-only System message;
    /// E2=leave activation input byte-stable; E3=do not duplicate a replay.
    /// Rules: C1+C2+!C3 -> E1+E2; C1+*+C3 -> E2+E3; !C1 -> E2.
    #[test]
    fn session_context_is_request_only_and_attempt_idempotent() {
        let slots = crate::session_slot::SessionRuntimeSlots::default();
        let executor = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
        );
        // Construction happens before the claim-fenced frozen projection is
        // accepted; realization installs the one authoritative slot later.
        slots.update("session-1", |slot| {
            slot.resources.prompts = vec!["frozen session prompt".into()];
        });
        let durable = activation("genai");
        let mut context = awaken_runtime_contract::RuntimeRunContext::default();
        let projected = executor.project(durable.clone(), &mut context).unwrap();
        assert_eq!(projected.input, durable.input, "E2");
        assert_eq!(context.request_context.len(), 1, "C1+C2+!C3 -> E1");
        assert_eq!(context.request_context[0].role, Role::System, "E1");
        assert_eq!(
            context.request_context[0].text_content(),
            "frozen session prompt",
            "E1"
        );
        let replayed = executor.project(projected, &mut context).unwrap();
        assert_eq!(context.request_context.len(), 1, "C1+C3 -> E3");
        assert_eq!(replayed.input, durable.input, "E2");

        let empty = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            crate::session_slot::SessionRuntimeSlots::default(),
            "session-1",
        );
        let mut empty_context = awaken_runtime_contract::RuntimeRunContext::default();
        let unchanged = activation("genai");
        assert_eq!(
            empty
                .project(unchanged.clone(), &mut empty_context)
                .unwrap(),
            unchanged,
            "!C1 -> E2"
        );
        assert!(empty_context.request_context.is_empty(), "!C1");
    }

    #[test]
    fn resource_revision_replacement_exposes_only_current_context() {
        // Cause/effect table: C1 revision R1 owns prompt P1 -> attempt A1 sees
        // only P1; C2 the same slot atomically advances to R2/P2 -> fresh A2
        // sees only P2; C3 A1 already captured P1 -> it remains immutable but
        // cannot contaminate A2. Every activation input stays byte-identical.
        let slots = crate::session_slot::SessionRuntimeSlots::default();
        let executor = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
        );
        slots.update("session-1", |slot| {
            slot.resources.prompts = vec!["resource revision one".into()]
        });
        let durable = activation("genai");
        let mut first = awaken_runtime_contract::RuntimeRunContext::default();
        let projected = executor.project(durable.clone(), &mut first).unwrap();
        assert_eq!(projected.input, durable.input, "C1");
        assert_eq!(
            first.request_context[0].text_content(),
            "resource revision one",
            "C1"
        );

        slots.update("session-1", |slot| {
            slot.resources.prompts = vec!["resource revision two".into()]
        });
        let mut second = awaken_runtime_contract::RuntimeRunContext::default();
        let projected = executor.project(durable.clone(), &mut second).unwrap();
        assert_eq!(projected.input, durable.input, "C2");
        assert_eq!(second.request_context.len(), 1, "C2");
        assert_eq!(
            second.request_context[0].text_content(),
            "resource revision two",
            "C2"
        );
        assert_eq!(
            first.request_context[0].text_content(),
            "resource revision one",
            "C3"
        );
    }

    #[test]
    fn concurrent_attempt_waits_for_one_complete_resource_context_revision() {
        // Concurrency decision table: C1=R1 is resident; C2=the sole slot lock
        // is held while replacing the complete prompt vector with R2; C3=a new
        // attempt reads during C2. Effect E1=C3 waits and observes all of R2,
        // never a mixed R1/R2 vector. The barrier makes the interleaving exact:
        // the reader starts only after the writer owns the publication lock.
        let slots = crate::session_slot::SessionRuntimeSlots::default();
        slots.update("session-1", |slot| {
            slot.resources.prompts = vec!["r1-a".into(), "r1-b".into()]
        });
        let executor = Arc::new(SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
        ));
        let release = Arc::new(std::sync::Barrier::new(2));
        let (writer_locked_tx, writer_locked_rx) = std::sync::mpsc::channel();
        let writer_slots = slots.clone();
        let writer_release = release.clone();
        let writer = std::thread::spawn(move || {
            writer_slots.update("session-1", |slot| {
                writer_locked_tx.send(()).unwrap();
                writer_release.wait();
                slot.resources.prompts = vec!["r2-a".into(), "r2-b".into()];
            });
        });
        writer_locked_rx.recv().unwrap();

        let (reader_started_tx, reader_started_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            reader_started_tx.send(()).unwrap();
            let mut context = awaken_runtime_contract::RuntimeRunContext::default();
            executor.project(activation("genai"), &mut context).unwrap();
            context
                .request_context
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>()
        });
        reader_started_rx.recv().unwrap();
        release.wait();
        writer.join().unwrap();
        assert_eq!(reader.join().unwrap(), ["r2-a", "r2-b"], "C1+C2+C3 -> E1");
    }

    #[test]
    fn claimed_attempt_projects_the_complete_session_tool_configuration() {
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };

        // Cause/effect graph: C1 an immutable claimed activation has no Flow MCP
        // toolset; C2 the frozen Session supplies an enabled/disabled Flow policy;
        // C3 it supplies an exact client tool; C4 the same attempt is retried.
        // Effects: E1 the attempt-local snapshot exposes only the Session policy
        // and client surface; E2 the retained publication stays unchanged; E3 a
        // disabled tool remains disabled; E4 replay is byte-stable. The Runtime's
        // model-face decision table owns the downstream dynamic-tool filtering.
        //
        // | Rule | C1 | Session tools | retry | effect |
        // | R1 | yes | enabled MCP + client | no | E1 + E2 |
        // | R2 | yes | disabled MCP | no | E2 + E3 |
        // | R3 | yes | enabled MCP + client | yes | E4 |
        // | R4 | yes | absent | no | unchanged |
        let slots = crate::session_slot::SessionRuntimeSlots::default();
        let executor = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
        );
        let publication = activation("genai");
        let retained = publication.snapshot.clone();
        let flow_policy = |enabled| awaken_session_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Mcp {
                    server_name: "flow".into(),
                },
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::new(
                    "workflow_get",
                    ToolExecutionPolicy {
                        enabled,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                )],
            }],
            client_tools: vec![awaken_agent_contract::ClientToolDescriptor {
                name: "resource_request".into(),
                description: "Request one frozen WorkUnit resource".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
        };
        slots.update("session-1", |slot| slot.tools = Some(flow_policy(true)));

        let mut context = awaken_runtime_contract::RuntimeRunContext::default();
        let projected = executor
            .project(publication, &mut context)
            .expect("R1 projection");
        assert_eq!(
            projected
                .snapshot
                .resolved_spec
                .plugin_config
                .agent
                .tool_policy("mcp__flow__workflow_get"),
            Some(ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAllow,
            }),
            "R1/E1"
        );
        assert!(
            projected
                .snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .any(|tool| tool.id == "resource_request"),
            "R1/E1"
        );
        assert!(
            retained
                .resolved_spec
                .plugin_config
                .agent
                .toolsets
                .is_empty(),
            "R1/E2"
        );

        let replayed = executor
            .project(projected.clone(), &mut context)
            .expect("R3 replay");
        assert_eq!(replayed.snapshot, projected.snapshot, "R3/E4");

        slots.update("session-1", |slot| slot.tools = Some(flow_policy(false)));
        let disabled = executor
            .project(activation("genai"), &mut context)
            .expect("R2 projection");
        assert_eq!(
            disabled
                .snapshot
                .resolved_spec
                .plugin_config
                .agent
                .tool_policy("mcp__flow__workflow_get")
                .map(|policy| policy.enabled),
            Some(false),
            "R2/E3"
        );

        let absent = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            crate::session_slot::SessionRuntimeSlots::default(),
            "session-1",
        );
        let unchanged = activation("genai");
        let mut absent_context = awaken_runtime_contract::RuntimeRunContext::default();
        assert_eq!(
            absent
                .project(unchanged.clone(), &mut absent_context)
                .expect("R4 projection"),
            unchanged,
            "R4"
        );
    }

    /// Cause/effect graph: C1 ACP backend, C2 explicitly-selected automatic
    /// Memory extension with content -> E1 one bounded request-only context;
    /// a Native backend (C1=false) -> E2 no adapter context. Standard Skills
    /// are deliberately absent from both rules because discovery is no longer
    /// duplicated by eager ACP prompt injection.
    ///
    /// | Rule | ACP | Memory | Context messages |
    /// | A1 | T | T | memory only |
    /// | A2 | F | T | empty |
    #[tokio::test]
    async fn acp_loads_only_explicit_automatic_memory_as_request_context() {
        let root = std::env::temp_dir().join(format!(
            "awaken-acp-memory-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let memory = awaken_ext_memory::MemoryDir::new(root);
        memory
            .write("preference", "Prefer concise answers.")
            .expect("seed memory");
        let loader = AcpContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            Some(awaken_ext_memory::MemoryRecall::new(
                Arc::new(memory),
                awaken_ext_memory::RecallBounds::default(),
            )),
            vec![awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch],
        );

        let acp = activation("acp:codex");
        let durable_input = acp.input.clone();
        let mut acp_context = awaken_runtime_contract::RuntimeRunContext::default();
        loader.load_context(&acp, &mut acp_context).await;
        assert_eq!(acp_context.request_context.len(), 1, "A1");
        assert_eq!(
            acp_context.provider_server_tools,
            [awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch],
            "A1 provider execution plan"
        );
        assert!(
            acp_context.request_context[0]
                .text_content()
                .contains("Prefer concise answers"),
            "A1 memory"
        );
        assert_eq!(acp.input, durable_input, "request context is non-durable");

        let native = activation("genai");
        let mut native_context = awaken_runtime_contract::RuntimeRunContext::default();
        loader.load_context(&native, &mut native_context).await;
        assert!(native_context.request_context.is_empty(), "A2");
        assert!(native_context.provider_server_tools.is_empty(), "A2");
    }
}

impl crate::session_slot::SessionRuntimeSlot {
    fn install_session_request_context(&mut self, messages: Vec<Message>) {
        if self.request_context != messages {
            self.request_context = messages;
            // RuntimeRunContext is assembled once with a SessionCtx. Drop only
            // that cached projection; the next attempt uses the new prefix.
            self.runtime = None;
        }
    }

    fn validate_environment_projection(
        &self,
        thread: &str,
        projection: &crate::session_slot::FrozenEnvironmentRuntimeProjection,
    ) -> Result<(), crate::HostError> {
        if self
            .environment_projection
            .as_ref()
            .is_some_and(|existing| existing.fingerprint != projection.fingerprint)
        {
            return Err(crate::HostError::internal(format!(
                "thread {thread} is already bound to a different frozen Environment"
            )));
        }
        Ok(())
    }

    fn install_validated_environment_projection(
        &mut self,
        environment: &awaken_session_contract::EnvironmentSnapshot,
        projection: crate::session_slot::FrozenEnvironmentRuntimeProjection,
    ) {
        self.environment_projection.get_or_insert(projection);
        self.environment_snapshot = Some(environment.clone());
    }

    /// The one lowering of validated SessionInit coordinates, shared by the
    /// legacy preparation entrance and complete fenced projection publication.
    fn project_validated_session_init(
        &mut self,
        init: &awaken_session_contract::SessionInit,
        environment: crate::session_slot::FrozenEnvironmentRuntimeProjection,
    ) {
        self.install_validated_environment_projection(&init.environment, environment);
        self.workspace = Some(init.workspace_id.clone());
        self.agent_id = Some(init.agent_id.clone());
        if let Some(model) = &init.model {
            self.model_ref = Some(model.clone());
        }
        if let Some(backend) = &init.runtime {
            self.register_backend_projection(backend);
        }
        self.delegates = init.delegate_ids.clone();
        self.tools = init.tools.clone();
    }
}

impl crate::SharedHost {
    #[cfg(test)]
    pub(crate) fn install_session_realization_lease(
        &self,
        session_id: &str,
        lease: awaken_session_contract::SessionRealizationLease,
    ) -> Result<(), crate::HostError> {
        let changed = self.authorize_worker_effect(|| {
            self.session_slots.update(session_id, |slot| {
                if !slot.install_realization_lease(crate::session_slot::test_observed_lease(lease))
                {
                    return Err(crate::HostError::internal(
                        "test lease installation rejected",
                    ));
                }
                Ok(slot.realization_changed.clone())
            })
        })?;
        changed.notify_waiters();
        Ok(())
    }

    /// Close only the failed local projection of one exact realization
    /// generation. A renewed same-generation projection is also invalidated,
    /// because the aggregate failure command authorizes the shorter admitted
    /// effect and expires that whole owner/incarnation/epoch generation. A
    /// replacement generation is never touched.
    pub(crate) fn invalidate_session_realization_lease(
        &self,
        session_id: &str,
        failed: &awaken_session_contract::SessionRealizationLease,
    ) {
        self.session_slots.modify(session_id, |slot| {
            if slot
                .realization_authority
                .lease()
                .is_some_and(|current| failed_realization_invalidates_projection(current, failed))
            {
                slot.close_realization_authority(None);
            }
        });
    }

    /// Authorize an MCP effect admitted before a same-epoch lease extension.
    /// The exact request remains immutable; only the Control-installed local
    /// lease may prove that its owner incarnation and epoch still have live,
    /// monotonically extended authority.
    pub(crate) fn mcp_generation_is_authorized_at(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
        now_unix_ms: u64,
    ) -> bool {
        self.mcp_generation_root_authority(generation)
            // Direct realizer callers can present a live Control assertion
            // before installing a complete local Session projection. A relay
            // capability, by contrast, always requires the current root.
            .unwrap_or_else(|| {
                awaken_session_contract::realization_lease_is_live_at(
                    generation.lease_expires_at_unix_ms,
                    now_unix_ms,
                )
            })
    }

    /// One root-authority decision shared by effect admission and the relay.
    /// Absence is explicit: it is never a live lease or proof of renewal.
    pub(crate) fn mcp_generation_root_authority(
        &self,
        generation: &awaken_session_contract::McpGenerationRef,
    ) -> Option<bool> {
        self.session_slots
            .read(&generation.session_id, |slot| {
                if slot.realization_authority.is_closed() {
                    return Some(false);
                }
                slot.realization_authority.lease().map(|current| {
                    current.runtime_incarnation == generation.runtime_incarnation
                        && current.epoch == generation.lease_epoch
                        && current.expires_at_unix_ms >= generation.lease_expires_at_unix_ms
                        && slot.realization_authority.remaining().is_some()
                })
            })
            .flatten()
    }

    /// Drive the one aggregate-owned terminal cleanup projection for a claimed
    /// realization. `true` means the terminal fence owns this slot (including a
    /// completed/not-found retirement); `false` means another actor settled the
    /// assignment before this recovery drive observed it.
    pub(crate) async fn reconcile_terminal_cleanup_for_lease(
        &self,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<bool, crate::HostError> {
        match control.terminal_cleanup_work(session_id, lease).await {
            Ok(Some(_)) => {
                // This first read distinguishes an ordinary Session (`None`)
                // from an aggregate-owned terminal fence. The topology-neutral
                // driver re-reads the closed action and remains the sole owner
                // of installation, effect ordering, receipt admission, retry,
                // and process-local acknowledgement for warm and cold slots.
                let runtime = self
                    .dispatch_session_runtime()
                    .and_then(|runtime| runtime.managed())
                    .map_err(crate::managed_adapter_error::from_run_error)?;
                awaken_session_contract::drive_session_terminal_cleanup(
                    session_id, lease, control, &runtime,
                )
                .await
                .map_err(|error| terminal_cleanup_drive_error(session_id, error))?;
                Ok(true)
            }
            Ok(None) => {
                if self
                    .retire_completed_terminal_cleanup_projection(session_id, lease)
                    .await
                {
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(awaken_session_contract::SessionRealizationControlFailure::NotFound) => {
                let _ = self.interrupt(session_id).await;
                if !self
                    .retire_completed_terminal_cleanup_projection(session_id, lease)
                    .await
                {
                    self.revoke_session_realization(session_id, None).await?;
                }
                Ok(true)
            }
            Err(error) => Err(crate::HostError::internal(format!(
                "Session `{session_id}` terminal cleanup control remained pending: {error}"
            ))),
        }
    }

    /// Claim every currently discoverable cold terminal assignment, then enter
    /// the canonical cleanup driver. The driver alone installs the current
    /// aggregate readback; the claim is routing and
    /// ownership input, not a second projection-installation path. This is the
    /// sole recovery selector; ordinary terminal settlement stays on the
    /// initiating command path. The bound prevents one recovery sweep from
    /// monopolizing the Worker, and the lifecycle awaits the whole sweep without
    /// imposing a timeout on its durable effects.
    pub async fn recover_terminal_cleanup_assignments(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<usize, crate::HostError> {
        const MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP: usize = 64;

        let control = self.session_control.as_ref().ok_or_else(|| {
            crate::HostError::internal(
                "active Worker Session projection has no Control renewal client",
            )
        })?;
        let mut recovered = 0;
        let mut terminal_error = None;
        for _ in 0..MAX_ASSIGNMENTS_PER_RECOVERY_SWEEP {
            let assignment = match control.claim_next_terminal_cleanup(target.clone()).await {
                Ok(Some(assignment)) => assignment,
                Ok(None) => break,
                Err(error) => {
                    terminal_error.get_or_insert_with(|| {
                        crate::HostError::internal(format!(
                            "cold Session terminal cleanup claim remained pending: {error}"
                        ))
                    });
                    break;
                }
            };
            recovered += 1;
            match self
                .reconcile_terminal_cleanup_for_lease(
                    control.as_ref(),
                    &assignment.session_id,
                    &assignment.authority.lease,
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    // Another actor completed the operation between claim and
                    // poll. This projection has no ordinary Run authority.
                    if let Err(error) = self
                        .revoke_session_realization(&assignment.session_id, None)
                        .await
                    {
                        terminal_error.get_or_insert(error);
                    }
                }
                Err(error) => {
                    terminal_error.get_or_insert(error);
                }
            }
        }
        match terminal_error {
            Some(error) => Err(error),
            None => Ok(recovered),
        }
    }

    /// Remove one process-local realization after its continuing authority is
    /// no longer provable. This is not a terminal Session edge: preserve the
    /// durable Sandbox so the next authorized Worker can adopt it, while local
    /// processes, routes, credentials, and runtime references are discarded.
    pub(crate) async fn revoke_session_realization(
        &self,
        session_id: &str,
        expected_projection: Option<(
            &awaken_session_contract::SessionRealizationLease,
            &Arc<tokio::sync::Mutex<()>>,
        )>,
    ) -> Result<bool, crate::HostError> {
        // Close admission under the synchronous slot mutex before waiting for
        // either async owner lock. Retain the exact closed projection for the
        // post-lock CAS; neither a renewal reply nor cancellation reopens it.
        let Some(selected) = self
            .session_slots
            .close_realization(session_id, expected_projection)
        else {
            return Ok(false);
        };
        self.retire_selected_session_realization(
            session_id,
            selected,
            expected_projection.is_some(),
        )
        .await
    }

    async fn retire_selected_session_realization(
        &self,
        session_id: &str,
        selected: crate::session_slot::SessionRealizationRevocationSelection,
        interrupt: bool,
    ) -> Result<bool, crate::HostError> {
        let _realization = selected.realization.lock().await;
        let _lifecycle = selected.lifecycle.lock().await;
        // This fence also applies to Worker-wide revocation: waiting cannot
        // authorize cleanup of a replacement installed after the close edge.
        if !self
            .session_slots
            .read(session_id, |slot| selected.matches(slot))
            .unwrap_or(false)
        {
            return Ok(false);
        }
        if interrupt {
            let _ = self
                .interrupt_with_commit_read(
                    session_id,
                    self.commit_for_read_under_lifecycle(session_id),
                )
                .await;
        }
        let retirement = self
            .retire_session_environment_for_revocation(session_id)
            .await?;
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_routes(session_id);
        }
        Ok(retirement)
    }

    /// Revoke every process-local Session projection after Worker authority is
    /// no longer provable. Durable Session environments remain available for
    /// adoption; only terminal Session lifecycle commands may dispose them.
    pub async fn revoke_all_session_realizations(&self) -> Result<usize, crate::HostError> {
        self.finish_session_realization_revocation(self.begin_session_realization_revocation())
            .await
    }

    /// Close all current Session authority BEFORE a Worker starts asynchronous
    /// drain/interruption. This selection is not a new Worker registry lease.
    pub fn begin_session_realization_revocation(&self) -> SessionRealizationRevocation {
        SessionRealizationRevocation {
            selected: self.session_slots.close_all_realizations(),
        }
    }

    /// Join every selected cleanup without letting one error or held owner lock
    /// prevent an unrelated Session from reaching its own stop edge. No detached
    /// task, additional scheduler, or cancellation-as-success path is created.
    pub async fn finish_session_realization_revocation(
        &self,
        revocation: SessionRealizationRevocation,
    ) -> Result<usize, crate::HostError> {
        let results = futures_util::future::join_all(revocation.selected.into_iter().map(
            |(id, selected)| async move {
                self.retire_selected_session_realization(&id, selected, false)
                    .await
            },
        ))
        .await;
        let mut revoked = 0;
        let mut error = None;
        for result in results {
            match result {
                Ok(true) => revoked += 1,
                Ok(false) => {}
                Err(failure) => {
                    error.get_or_insert(failure);
                }
            }
        }
        error.map_or(Ok(revoked), Err)
    }

    /// Cancel every run currently backed by a process-local Session projection.
    ///
    /// Worker authority loss must stop in-flight tool processes before local
    /// realization revocation detaches their process bindings. Otherwise a native
    /// process can continue writing after this Worker has dropped authority.
    pub async fn interrupt_all_session_runs(&self) -> usize {
        let session_ids = self.session_slots.session_ids();
        let mut interrupted = 0;
        for session_id in session_ids {
            match self.interrupt(&session_id).await {
                Ok(()) => interrupted += 1,
                Err(error) => tracing::warn!(session_id, %error,
                    "Worker could not request Session interruption before local revocation"),
            }
        }
        interrupted
    }

    /// Install the protocol-neutral Runtime coordinates derived from one frozen
    /// Session projection. Managed local preparation and claimed Worker replay
    /// share this single lowering path; neither may independently reconstruct a
    /// different workspace, Agent, model, backend, Environment, or tool surface.
    #[cfg(test)]
    pub(crate) fn project_session_init(
        &self,
        thread: &str,
        init: &awaken_session_contract::SessionInit,
    ) -> Result<(), crate::HostError> {
        let environment = crate::provisioning::project_environment(&init.environment);
        self.session_slots.update(thread, |slot| {
            slot.validate_environment_projection(thread, &environment)?;
            slot.project_validated_session_init(init, environment);
            Ok(())
        })
    }

    pub(crate) async fn install_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
        realization_lease: Option<awaken_session_contract::ObservedSessionRealizationLease>,
    ) -> Result<(), crate::HostError> {
        self.install_frozen_session_projection_with_resource_authority(
            thread,
            projection,
            claim,
            if synchronize_resources {
                FrozenResourceProjectionMode::Synchronize
            } else {
                FrozenResourceProjectionMode::ValidateInstalled
            },
            realization_lease,
        )
        .await
    }

    /// Install the Coordinator's complete frozen dispatch projection. This is
    /// the sole caller authorized to amend an unattempted same-revision Resource
    /// generation; Worker and local realization callers use the fenced method
    /// above.
    pub(crate) async fn install_dispatch_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
    ) -> Result<(), crate::HostError> {
        self.install_frozen_session_projection_with_resource_authority(
            thread,
            projection,
            None,
            FrozenResourceProjectionMode::Dispatch,
            None,
        )
        .await
    }

    async fn install_frozen_session_projection_with_resource_authority(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        claim: Option<&RunClaim>,
        resource_mode: FrozenResourceProjectionMode,
        realization_lease: Option<awaken_session_contract::ObservedSessionRealizationLease>,
    ) -> Result<(), crate::HostError> {
        if projection.baseline.fingerprint.0.trim().is_empty() {
            return Err(crate::HostError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        let has_mcp_projection = projection.mcp.iter().any(|attachment| {
            !matches!(
                attachment.state,
                awaken_session_contract::McpAttachmentState::Removed
                    | awaken_session_contract::McpAttachmentState::Failed
            )
        });
        let resource_transition = projection
            .resource_transition(resource_mode.transition_use())
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        let baseline = baseline_projection(&projection.baseline);
        let init = projection.session_init();
        let environment_projection = crate::provisioning::project_environment(&init.environment);
        // Complete projection installation owns the Resource suffix lock from
        // the first comparison through publication of every correlated fact.
        // Desired-only staging and physical reconciliation therefore cannot
        // interleave a different resources/skills/manifest generation between
        // preflight and the baseline/transition/lease update below.
        let resource_projection = self
            .session_slots
            .update(thread, |slot| slot.resource_projection.clone());
        let _resource_projection = resource_projection.lock().await;
        let (publication, model_candidate) = self.resolve_canonical_session_projection(
            &projection.workspace_id,
            crate::host::CanonicalSessionProjection::Baseline(&projection.baseline),
            projection.agent_publication.clone(),
        )?;
        if let Some(asserted) = &publication
            && self
                .session_slots
                .read(thread, |slot| {
                    slot.published_snapshot
                        .as_ref()
                        .is_some_and(|existing| existing != asserted)
                })
                .unwrap_or(false)
        {
            return Err(crate::HostError::internal(
                "Session realization cannot replace its immutable Agent publication",
            ));
        }
        if self
            .session_slots
            .read(thread, |slot| {
                slot.environment_projection
                    .as_ref()
                    .is_some_and(|existing| {
                        existing.fingerprint != environment_projection.fingerprint
                    })
            })
            .unwrap_or(false)
        {
            return Err(crate::HostError::internal(format!(
                "thread {thread} is already bound to a different frozen Environment"
            )));
        }
        // Validate the prospective complete layout before publishing any part of
        // a cold projection. The baseline is not resident yet on first install,
        // so a validator that reads only the current slot would miss a Repository
        // nested below one of its frozen mounts and could load Skills first. A
        // Worker-owned Dispatch is projection-only on the Coordinator: it uses
        // the same structural kernel without resolving a trusted-host provider
        // that only the claimed Worker may own. Every realization/cleanup path
        // and every Local dispatch retains the exact provider-effective check.
        if resource_mode == FrozenResourceProjectionMode::Dispatch
            && projection.baseline.runtime_placement
                == awaken_session_contract::SessionRuntimePlacement::Worker
        {
            self.validate_structural_managed_resource_layout(
                thread,
                &projection.resources,
                Some(&baseline.mounts),
                Some(&baseline.env),
                Some(&environment_projection),
            )?;
        } else {
            let provider = if let Some(candidate) = model_candidate.as_ref() {
                self.session_environment_provider_for_thread(thread, candidate.provisioning())?
            } else {
                self.session_environment_provider_for_thread(
                    thread,
                    &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                )?
            };
            self.validate_managed_resource_layout(
                thread,
                &projection.resources,
                provider,
                Some(&baseline.mounts),
                Some(&baseline.env),
                Some(&environment_projection),
            )?;
        }

        let baseline_to_install = if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.baseline.clone())
            .flatten()
        {
            if existing.fingerprint != baseline.fingerprint {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Session baseline"
                )));
            }
            None
        } else {
            let is_realized = self
                .session_slots
                .read(thread, |slot| {
                    slot.runtime.is_some() || slot.environment_owner.has_local_environment()
                })
                .unwrap_or(false);
            if is_realized {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} was realized before its frozen Session baseline"
                )));
            }
            Some(baseline)
        };

        if baseline_to_install.is_some()
            && matches!(
                resource_mode,
                FrozenResourceProjectionMode::ValidateInstalled
            )
            && projection.resource_revision > 0
        {
            return Err(crate::HostError::internal(format!(
                "thread {thread} cannot cold-materialize frozen Session Resources during lease-only renewal"
            )));
        }
        let transition_install = self
            .session_slots
            .read(thread, |slot| {
                frozen_resource_transition_install_decision(
                    slot.resource_transition.as_ref(),
                    &resource_transition,
                    resource_mode.allows_unattempted_amendment(),
                )
            })
            .unwrap_or(FrozenResourceTransitionInstallDecision::Stage);
        if transition_install == FrozenResourceTransitionInstallDecision::Reject {
            let existing = self
                .session_slots
                .read(thread, |slot| slot.resource_transition.clone())
                .flatten();
            let existing_revision = existing
                .as_ref()
                .map(|transition| transition.desired().revision);
            let incoming_previous_matches = existing
                .as_ref()
                .is_some_and(|existing| existing.desired() == resource_transition.previous());
            return Err(crate::HostError::internal(format!(
                "thread {thread} cannot replace its current Session Resource projection \
                 (mode {resource_mode:?}, existing desired revision {existing_revision:?}, \
                 incoming {}->{}, previous matches existing desired: {incoming_previous_matches})",
                resource_transition.previous().revision,
                resource_transition.desired().revision,
            )));
        }
        // Resource installation owns generation equality and fencing. Claimed
        // replay revalidates the exact aggregate transition. Requirements are
        // staged here, but only the physical transition body may publish the
        // active manifest.
        let preparation = if resource_mode.synchronizes() && projection.resource_revision > 0 {
            realization_lease
                .as_ref()
                .map(|lease| {
                    realization_renewal::ResourcePreparation::begin(self, thread, lease.clone())
                })
                .transpose()?
        } else {
            None
        };
        if resource_mode.synchronizes() && projection.resource_revision > 0 {
            let stage = self
                .stage_prevalidated_dispatched_resource_transition_under_resource_projection(
                    thread,
                    &resource_transition,
                    claim,
                    realization_lease.as_ref().map(|proof| &proof.lease),
                    projection.environment.binding().is_some(),
                );
            tokio::select! {
                biased;
                error = async {
                    match &preparation {
                        Some(preparation) => preparation.authority_ended().await,
                        None => std::future::pending().await,
                    }
                } => return Err(error),
                result = stage => result.map_err(|error| crate::HostError::internal(error.to_string()))?,
            }
        }
        if let Some(preparation) = &preparation {
            preparation.validate()?;
        }
        self.authorize_worker_effect(|| {
            self.session_slots.update(thread, |slot| {
                // Revalidate fallible coordinates and stage the existing owner
                // transition under the final slot lock. No correlated field is
                // published before the time proof/preparation is admitted.
                slot.validate_session_publication(publication.as_ref())?;
                slot.validate_environment_projection(thread, &environment_projection)?;
                let owner = slot.projected_environment_owner(
                    thread,
                    &projection.workspace_id,
                    &projection.environment,
                )?;
                if let Some(preparation) = &preparation {
                    preparation.finish(slot)?;
                } else if let Some(lease) = realization_lease {
                    if !slot.install_realization_lease(lease) {
                        return Err(crate::HostError::internal(
                            "Session realization time proof is not current",
                        ));
                    }
                    slot.realization_changed.notify_waiters();
                }
                // This suffix is infallible and holds both Worker authority and
                // the slot lock: revoke/replacement cannot interleave a partial
                // publication between the check above and these writes.
                slot.retain_validated_session_publication(publication.as_ref());
                slot.install_projected_environment_owner(owner, &projection.environment);
                slot.project_validated_session_init(&init, environment_projection);
                slot.install_session_request_context(projection.request_context.clone());
                if let Some(baseline) = baseline_to_install {
                    slot.baseline = Some(baseline);
                }
                // The complete frozen projection is the cold Worker's first
                // observation of Session admission. Publish the existing execution
                // marker in the same atomic slot update as its baseline and
                // transition; local admission may set it earlier, while revocation
                // remains authorized to clear it with the rest of live execution
                // state.
                slot.session_dispatch = true;
                slot.has_mcp_projection = has_mcp_projection;
                slot.resource_transition = Some(resource_transition);
                Ok::<_, crate::HostError>(())
            })?;
            Ok(())
        })
    }

    /// Terminal effect fixtures supply an already-received assignment. Actual
    /// transport-latency tests call the production entrance with their explicit
    /// request-start observation instead of this convenience adapter.
    #[cfg(test)]
    pub(crate) async fn install_test_terminal_cleanup_projection(
        &self,
        assignment: &awaken_session_contract::SessionTerminalCleanupAssignment,
    ) -> Result<(), crate::HostError> {
        let started = awaken_local_process::authority_proof_started().unwrap();
        self.install_terminal_cleanup_projection(
            assignment,
            assignment.authority.clone().observed(started),
        )
        .await
    }

    /// Install one aggregate-frozen terminal assignment without entering the
    /// active realization driver. Cleanup I/O remains owned by the terminal
    /// effect path.
    pub(crate) async fn install_terminal_cleanup_projection(
        &self,
        assignment: &awaken_session_contract::SessionTerminalCleanupAssignment,
        proof: awaken_session_contract::ObservedSessionRealizationLease,
    ) -> Result<(), crate::HostError> {
        if proof.lease != assignment.authority.lease {
            return Err(crate::HostError::internal(
                "terminal assignment time proof names another lease",
            ));
        }
        let realization = self.session_slots.realization_lock(&assignment.session_id);
        let _realization = realization.lock().await;
        self.install_frozen_session_projection_with_resource_authority(
            &assignment.session_id,
            assignment.projection.clone(),
            None,
            FrozenResourceProjectionMode::Terminal,
            Some(proof),
        )
        .await?;
        self.session_slots.update(&assignment.session_id, |slot| {
            slot.terminal_environment_state = Some(assignment.projection.environment.clone());
        });
        Ok(())
    }

    pub(crate) fn install_environment_projection(
        &self,
        thread: &str,
        environment: &awaken_session_contract::EnvironmentSnapshot,
    ) -> Result<(), crate::HostError> {
        let projection = crate::provisioning::project_environment(environment);
        self.session_slots.update(thread, |slot| {
            slot.validate_environment_projection(thread, &projection)?;
            slot.install_validated_environment_projection(environment, projection);
            Ok(())
        })
    }

    pub(crate) fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::MountRequirement> {
        self.session_slots
            .read(thread, |slot| {
                project_session_mounts(
                    slot.resources.mounts.clone(),
                    slot.baseline
                        .as_ref()
                        .map(|baseline| baseline.mounts.as_slice()),
                    slot.content_delivery,
                )
            })
            .unwrap_or_default()
    }

    pub(crate) fn thread_session_env(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::EnvVar> {
        self.session_slots
            .read(thread, |slot| {
                slot.baseline
                    .as_ref()
                    .map(|baseline| baseline.env.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Return the exact credential-custody profile frozen in this Session's
    /// Environment. Inference admission uses this projection only when the
    /// immutable Agent publication constrains a plaintext boundary without
    /// naming one exact holder.
    pub(crate) fn thread_credential_realization(
        &self,
        thread: &str,
    ) -> Option<awaken_runtime_contract::CredentialRealizationProfile> {
        self.session_slots
            .read(thread, |slot| {
                slot.environment_snapshot
                    .as_ref()
                    .map(|environment| environment.credential_realization.clone())
            })
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn thread_session_prompts(&self, thread: &str) -> Vec<String> {
        self.session_slots
            .prompts(thread, None, false, true)
            .expect("test Native prompt projection")
    }
}

pub(crate) fn project_session_mounts(
    mut resources: Vec<awaken_provisioning_contract::MountRequirement>,
    baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
    content_delivery: Option<crate::session_slot::ManagedContentDelivery>,
) -> Vec<awaken_provisioning_contract::MountRequirement> {
    if let Some(baseline_mounts) = baseline_mounts {
        resources.extend_from_slice(baseline_mounts);
    }
    if content_delivery == Some(crate::session_slot::ManagedContentDelivery::SemanticTools) {
        resources.retain(|mount| {
            !matches!(
                mount.source,
                awaken_provisioning_contract::MountSource::MemoryStore { .. }
            )
        });
    }
    resources
}

pub(crate) fn baseline_projection(
    baseline: &awaken_session_contract::SessionBaseline,
) -> crate::session_slot::FrozenBaselineRuntimeProjection {
    baseline.clone()
}

#[cfg(test)]
mod terminal_cleanup_drive_error_tests {
    use super::terminal_cleanup_drive_error;
    use crate::HostErrorKind;

    #[test]
    fn terminal_driver_error_mapping_preserves_effect_faults_and_control_boundary() {
        // Cause/effect graph: C1 the canonical driver returns a classified
        // Runtime effect failure; C2 it returns a Control/protocol failure.
        // Effects: E1 preserve the Runtime fault kind and stable code through
        // the existing inverse adapter; E2 keep Control/protocol failure at the
        // Host's terminal reconciliation boundary with the Session identity.
        // Decision rules: M1=C1=>E1; M2=C2=>E2.
        let effect = terminal_cleanup_drive_error(
            "session-a",
            awaken_session_contract::SessionRealizationDriveError::Effect(
                awaken_session_contract::RunError::unavailable_classified(
                    "terminal_provider_retry",
                    "provider is temporarily unavailable",
                ),
            ),
        );
        assert_eq!(effect.kind, HostErrorKind::Unavailable, "M1/E1");
        assert_eq!(effect.code, "terminal_provider_retry", "M1/E1");

        let control = terminal_cleanup_drive_error(
            "session-a",
            awaken_session_contract::SessionRealizationDriveError::Control(
                awaken_session_contract::SessionRealizationControlFailure::Conflict,
            ),
        );
        assert_eq!(control.kind, HostErrorKind::Internal, "M2/E2");
        assert!(control.message.contains("session-a"), "M2/E2");
        assert!(control.message.contains("changed concurrently"), "M2/E2");
    }
}

#[cfg(test)]
mod resource_transition_install_tests {
    use super::{
        FrozenResourceTransitionInstallDecision, frozen_resource_transition_install_decision,
    };

    fn transition(
        workspace: &str,
        revision: u64,
        skill: Option<&str>,
    ) -> awaken_session_contract::SessionResourceTransition {
        let resources = awaken_session_contract::ResolvedSessionResources::try_new(
            Vec::new(),
            skill
                .map(|skill_id| awaken_session_contract::ResolvedSkillBinding {
                    kind: awaken_agent_contract::AgentSkillKind::Custom,
                    skill_id: skill_id.into(),
                    version: 1,
                    bundle_sha256: format!("sha256-{skill_id}"),
                })
                .into_iter()
                .collect(),
        )
        .expect("test Resource projection is valid");
        let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
            workspace, revision, resources,
        );
        awaken_session_contract::SessionResourceTransition::new(manifest.clone(), manifest)
            .expect("test transition belongs to one Workspace")
    }

    fn transition_from(
        previous: awaken_session_contract::SessionResourceManifest,
        desired: awaken_session_contract::SessionResourceManifest,
    ) -> awaken_session_contract::SessionResourceTransition {
        awaken_session_contract::SessionResourceTransition::new(previous, desired)
            .expect("test transition belongs to one Workspace")
    }

    #[test]
    fn complete_projection_install_decision_has_one_authority_table() {
        use FrozenResourceTransitionInstallDecision::{Reject, Replace, Stage};

        // Cause/effect graph: C1 a staged transition exists; C2 incoming desired
        // generation is an exact replay (its prior endpoint may already have
        // advanced after physical completion); C3 Workspace matches; C4 desired revision is newer,
        // equal, or older; C5 the typed caller is Coordinator Dispatch and may
        // project the aggregate's unattempted amendment. Effects are Stage,
        // Replace, or Reject before any Resource read or slot mutation.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // | I1 | F | any | any | any | any | Stage |
        // | I2 | T | T | any | any | any | Stage |
        // | I3 | T | F | T | newer | any | Replace |
        // | I4 | T | F | T | equal | T | Replace |
        // | I5 | T | F | T | equal | F | Reject |
        // | I6 | T | F | T | older | any | Reject |
        // | I7 | T | F | F | any | any | Reject |
        let old = transition_from(
            transition("workspace-a", 8, Some("base")).desired().clone(),
            transition("workspace-a", 9, Some("old")).desired().clone(),
        );
        let exact = old.clone();
        let exact_after_completion =
            transition_from(exact.desired().clone(), exact.desired().clone());
        let newer = transition("workspace-a", 10, Some("new"));
        let amended = transition("workspace-a", 9, Some("amended"));
        let older = transition("workspace-a", 8, Some("older"));
        let foreign = transition("workspace-b", 10, Some("foreign"));
        let rules = [
            (None, &old, false, Stage),
            (Some(&old), &exact, false, Stage),
            (Some(&old), &exact_after_completion, false, Stage),
            (Some(&old), &newer, false, Replace),
            (Some(&old), &amended, true, Replace),
            (Some(&old), &amended, false, Reject),
            (Some(&old), &older, true, Reject),
            (Some(&old), &foreign, true, Reject),
        ];
        for (existing, incoming, authority, expected) in rules {
            assert_eq!(
                frozen_resource_transition_install_decision(existing, incoming, authority),
                expected
            );
        }
    }
}

#[cfg(test)]
mod network_policy_tests {
    /// FMECA: F1 a prepared image and packages are both realized (S8/O3/D4,
    /// RPN96) causes nondeterministic/double installation; mitigation makes the
    /// digest the sole rootfs and clears package requirements. F2 no prepared
    /// image drops authored packages (S7/O3/D3, RPN63); mitigation losslessly
    /// projects non-empty managers with an exact revision resolution id.
    /// Cause/effect graph: C1=prepared digest present; C2=authored packages;
    /// E1=image rootfs; E2=runtime package install; constraint C1 XOR E2.
    /// | Rule | C1 | C2 | E1 | E2 |
    /// | R1   | 0  | 1  | 0  | 1  |
    /// | R2   | 1  | 1  | 1  | 0  |
    /// Empty managers disappear and no protocol DTO reaches provisioning.
    #[test]
    fn environment_packages_project_losslessly_to_the_provisioning_contract() {
        let environment = awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env_packages".into(),
            revision: awaken_session_contract::EnvironmentRevision(3),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("fp".into()),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: awaken_session_contract::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                pip: vec!["httpx==0.28".into()],
                ..Default::default()
            },
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let projected = crate::provisioning::project_environment(&environment);
        assert_eq!(
            projected.packages.managers,
            [
                ("npm".into(), vec!["tsx@4".into()]),
                ("pip".into(), vec!["httpx==0.28".into()]),
            ]
            .into_iter()
            .collect()
        );
        assert!(!projected.packages.managers.contains_key("apt"));
        assert_eq!(
            projected.packages.resolution_id.as_deref(),
            Some("env_packages:3")
        );

        let mut prepared = environment;
        prepared.prepared_image = Some("registry/awaken@sha256:prepared".into());
        let projected = crate::provisioning::project_environment(&prepared);
        assert!(projected.packages.is_empty(), "R2");
        assert!(matches!(
            projected.sandbox.and_then(|value| value.environment),
            Some(awaken_provisioning_contract::EnvironmentKind::Image { reference })
                if reference == "registry/awaken@sha256:prepared"
        ));
    }
}
