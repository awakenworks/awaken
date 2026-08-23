//! Claim-bound Runtime projections of one frozen Session.
//!
//! The host remains the sole owner of Session realization. An embedding
//! application supplies no late desired state: mounts, environment values,
//! prompts, and MCP generations come only from the frozen Control projection
//! and are installed into the same Session slot for the Native/ACP backend path.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_run_ingress::RunClaim;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{ExecutorCapabilities, RunAttemptExecutor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenewalFailureDisposition {
    /// Control proves the Session projection is no longer renewable.
    Retire,
    /// Authority cannot be proved; surface diagnostics and revoke locally.
    DiagnoseAndRevoke,
}

fn realization_renewal_failure_disposition(
    error: &awaken_session_contract::SessionRealizationControlFailure,
) -> RenewalFailureDisposition {
    match error.disposition() {
        awaken_session_contract::SessionRealizationControlDisposition::Terminal => {
            RenewalFailureDisposition::Retire
        }
        awaken_session_contract::SessionRealizationControlDisposition::NotReady
        | awaken_session_contract::SessionRealizationControlDisposition::Retryable => {
            RenewalFailureDisposition::DiagnoseAndRevoke
        }
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
    // this consumer. Terminal Control truth is exactly NotFound, Retired,
    // Terminal, or Invalid; every other failure must retain the
    // diagnostic/retry path.
    let expected_retirement = matches!(selector, 0 | 2 | 3 | 6);
    let disposition = realization_renewal_failure_disposition(&error);

    assert_eq!(
        disposition == RenewalFailureDisposition::Retire,
        expected_retirement
    );
    if !expected_retirement {
        assert_eq!(disposition, RenewalFailureDisposition::DiagnoseAndRevoke);
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
        // quietly; N4 NotReady/stale/conflict and N5 unavailable do not prove a
        // terminal Control state -> surface diagnostics, but the shared safety
        // effect still interrupts and revokes only this local Session.
        for error in [
            SessionRealizationControlFailure::NotFound,
            SessionRealizationControlFailure::Retired,
            SessionRealizationControlFailure::Terminal,
            SessionRealizationControlFailure::Invalid("bad target".into()),
        ] {
            assert_eq!(
                realization_renewal_failure_disposition(&error),
                RenewalFailureDisposition::Retire
            );
        }
        for error in [
            SessionRealizationControlFailure::NotReady,
            SessionRealizationControlFailure::StaleOwnership,
            SessionRealizationControlFailure::Conflict,
            SessionRealizationControlFailure::Unavailable("network unavailable".into()),
        ] {
            assert_eq!(
                realization_renewal_failure_disposition(&error),
                RenewalFailureDisposition::DiagnoseAndRevoke
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
/// deterministic baseline prompts without mutating the retained publication.
/// Deterministic message ids make a retried uncommitted attempt
/// byte-for-byte stable; committed history prevents later Runs from reinjecting
/// the baseline.
pub(crate) struct SessionPromptAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    slots: crate::session_slot::SessionRuntimeSlots,
    session_id: String,
}

impl SessionPromptAttemptExecutor {
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
        let prompts = self.slots.prompts(&self.session_id);
        if prompts.is_empty() {
            return Ok(activation);
        }
        let already_present = |prompt: &str| {
            activation.input.iter().any(|message| {
                message.role == Role::System
                    && message.content.iter().any(|content| {
                        matches!(
                            content,
                            awaken_agent_contract::agent::content::ContentBlock::Text { text }
                                if text == prompt
                        )
                    })
            })
        };
        let mut projected = prompts
            .iter()
            .enumerate()
            .filter(|(_, prompt)| !already_present(prompt))
            .map(|(index, prompt)| {
                Message::text(
                    MessageId(format!(
                        "session-baseline:{}:{index}",
                        activation.thread_id.0
                    )),
                    Role::System,
                    prompt.clone(),
                )
            })
            .collect::<Vec<_>>();
        projected.append(&mut activation.input);
        activation.input = projected;
        Ok(activation)
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for SessionPromptAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.inner.execute(self.project(activation)?, context).await
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        self.inner.capabilities()
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for SessionPromptAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        command: awaken_runtime_contract::resume::ResumeCommand,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.inner
            .resume(self.project(activation)?, command, context)
            .await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<()> {
        self.inner.cancel(activation, context).await
    }
}

/// Loads selected Skills and bounded Memory recall into ACP's request-only
/// context. Native execution keeps using its existing Skill tools and
/// `BeforeInference` Memory hook; this adapter only bridges the external backend
/// through the neutral `RuntimeRunContext` field.
pub(crate) struct AcpContextAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    skills: Option<Arc<dyn awaken_ext_skills::SkillRegistry>>,
    memory: Option<awaken_ext_memory::MemoryRecall>,
    session_id: String,
}

impl AcpContextAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        skills: Option<Arc<dyn awaken_ext_skills::SkillRegistry>>,
        memory: Option<awaken_ext_memory::MemoryRecall>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            skills,
            memory,
            session_id: session_id.into(),
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
        if let Some(skills) = &self.skills {
            let loaded = skills
                .list()
                .into_iter()
                .filter(|skill| skill.model_invocable)
                .map(|skill| {
                    awaken_ext_skills::render_backend_context(&skill, Some(&self.session_id))
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if !loaded.is_empty() {
                context.request_context.push(Message::text(
                    MessageId(format!("acp-skills:{}", activation.run_id.0)),
                    Role::System,
                    loaded,
                ));
            }
        }
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

    /// Session prompt FMECA cause/effect graph:
    /// C1=frozen prompts exist, C2=the durable User input was committed before
    /// the Worker claim, C3=the exact deterministic prompt is already present.
    /// E1=prepend the prompt exactly once, E2=leave input byte-stable.
    /// Decision rules: C1 C2 !C3 -> E1; C1 * C3 -> E2; !C1 * * -> E2.
    /// Runtime's committed-id filter, rather than transcript emptiness, owns
    /// replay/later-Run de-duplication; a committed User Message is not proof
    /// that the frozen System prompt has ever reached inference.
    #[test]
    fn session_prompt_projection_survives_precommitted_user_input_without_duplication() {
        let slots = crate::session_slot::SessionRuntimeSlots::default();
        let executor =
            SessionPromptAttemptExecutor::new(Arc::new(UnusedExecutor), slots.clone(), "session-1");
        // Construction happens before the claim-fenced frozen projection is
        // accepted; realization installs the one authoritative slot later.
        slots.update("session-1", |slot| {
            slot.baseline = Some(crate::session_slot::FrozenBaselineRuntimeProjection {
                fingerprint: awaken_session_contract::SessionBaselineFingerprint("baseline".into()),
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: vec!["frozen session prompt".into()],
            });
        });
        let projected = executor.project(activation("genai")).unwrap();
        assert_eq!(projected.input.len(), 2, "C1+C2+!C3 -> E1");
        assert_eq!(projected.input[0].role, Role::System, "E1");
        assert_eq!(
            projected.input[0].text_content(),
            "frozen session prompt",
            "E1"
        );
        let replayed = executor.project(projected).unwrap();
        assert_eq!(
            replayed
                .input
                .iter()
                .filter(|message| message.role == Role::System)
                .count(),
            1,
            "C1+C3 -> E2"
        );

        let empty = SessionPromptAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            crate::session_slot::SessionRuntimeSlots::default(),
            "session-1",
        );
        assert_eq!(
            empty.project(activation("genai")).unwrap().input.len(),
            1,
            "!C1 -> E2"
        );
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
        let executor =
            SessionPromptAttemptExecutor::new(Arc::new(UnusedExecutor), slots.clone(), "session-1");
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

        let projected = executor.project(publication).expect("R1 projection");
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

        let replayed = executor.project(projected.clone()).expect("R3 replay");
        assert_eq!(replayed.snapshot, projected.snapshot, "R3/E4");

        slots.update("session-1", |slot| slot.tools = Some(flow_policy(false)));
        let disabled = executor
            .project(activation("genai"))
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

        let absent = SessionPromptAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            crate::session_slot::SessionRuntimeSlots::default(),
            "session-1",
        );
        let unchanged = activation("genai");
        assert_eq!(
            absent.project(unchanged.clone()).expect("R4 projection"),
            unchanged,
            "R4"
        );
    }

    /// Cause/effect graph:
    /// C1 ACP backend, C2 selected model-invocable Skill, C3 non-empty Memory
    /// -> E1 one Skill context and E2 one bounded Memory context; a Native
    /// backend (C1=false) -> E3 no adapter context because its existing Skill
    /// tools and BeforeInference hook remain authoritative.
    ///
    /// | Rule | ACP | Skill | Memory | Context messages |
    /// | A1 | T | T | T | skill + memory |
    /// | A2 | F | T | T | empty |
    #[tokio::test]
    async fn acp_loads_selected_skills_and_memory_as_request_only_context() {
        let skill = awaken_ext_skills::SkillSpec::new(
            "review",
            "Review",
            "Review carefully",
            "Use ${SESSION_ID} and inspect the evidence.",
        );
        let skills: Arc<dyn awaken_ext_skills::SkillRegistry> =
            Arc::new(awaken_ext_skills::FixedSkillRegistry::from_specs([skill]));
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
            Some(skills),
            Some(awaken_ext_memory::MemoryRecall::new(
                Arc::new(memory),
                awaken_ext_memory::RecallBounds::default(),
            )),
            "session-1",
        );

        let acp = activation("acp:codex");
        let durable_input = acp.input.clone();
        let mut acp_context = awaken_runtime_contract::RuntimeRunContext::default();
        loader.load_context(&acp, &mut acp_context).await;
        assert_eq!(acp_context.request_context.len(), 2, "A1");
        assert!(
            acp_context.request_context[0]
                .text_content()
                .contains("Use session-1"),
            "A1 skill template"
        );
        assert!(
            acp_context.request_context[1]
                .text_content()
                .contains("Prefer concise answers"),
            "A1 memory"
        );
        assert_eq!(acp.input, durable_input, "request context is non-durable");

        let native = activation("genai");
        let mut native_context = awaken_runtime_contract::RuntimeRunContext::default();
        loader.load_context(&native, &mut native_context).await;
        assert!(native_context.request_context.is_empty(), "A2");
    }
}

impl crate::SharedHost {
    pub(crate) fn install_session_request_context(&self, session_id: &str, messages: Vec<Message>) {
        self.session_slots.update(session_id, |slot| {
            if slot.request_context != messages {
                slot.request_context = messages;
                // RuntimeRunContext is assembled once with a SessionCtx. Drop
                // only that cached projection so the next claimed attempt is
                // rebuilt from the newly installed immutable prefix.
                slot.runtime = None;
            }
        });
    }

    pub(crate) fn install_expected_environment_binding(
        &self,
        session_id: &str,
        binding: Option<String>,
    ) -> Result<(), crate::HostError> {
        self.session_slots.update(session_id, |slot| {
            if let (Some(existing), Some(asserted)) = (&slot.expected_environment_binding, &binding)
                && existing != asserted
            {
                return Err(crate::HostError::internal(format!(
                    "Session {session_id} is already bound to a different durable environment"
                )));
            }
            slot.expected_environment_binding = binding;
            Ok(())
        })
    }

    pub(crate) fn install_session_realization_lease(
        &self,
        session_id: &str,
        lease: awaken_session_contract::SessionRealizationLease,
    ) {
        let changed = self.session_slots.update(session_id, |slot| {
            slot.realization_lease = Some(lease);
            slot.realization_changed.clone()
        });
        changed.notify_waiters();
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
        if awaken_session_contract::realization_lease_is_live_at(
            generation.lease_expires_at_unix_ms,
            now_unix_ms,
        ) {
            return true;
        }
        self.session_slots
            .read(&generation.session_id, |slot| {
                slot.realization_lease.as_ref().is_some_and(|current| {
                    current.runtime_incarnation == generation.runtime_incarnation
                        && current.epoch == generation.lease_epoch
                        && current.expires_at_unix_ms >= generation.lease_expires_at_unix_ms
                        && awaken_session_contract::realization_lease_is_live_at(
                            current.expires_at_unix_ms,
                            now_unix_ms,
                        )
                })
            })
            .unwrap_or(false)
    }

    /// Drive the one aggregate-owned terminal cleanup projection for a resident
    /// realization. `true` means the terminal fence owns this slot (including a
    /// completed/not-found retirement); `false` resumes ordinary lease renewal.
    /// Warm slots and cold recovery assignments both enter this exact helper.
    async fn reconcile_terminal_cleanup_for_lease(
        &self,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<bool, crate::HostError> {
        match control.terminal_cleanup_commands(session_id, lease).await {
            Ok(Some(commands)) => {
                let mut terminal_error = None;
                for command in commands {
                    match self
                        .execute_dispatched_terminal_cleanup(command.clone())
                        .await
                    {
                        Ok(completion) => {
                            if let Err(error) = control
                                .record_terminal_cleanup_completion(lease, completion)
                                .await
                                && !matches!(
                                    error,
                                    awaken_session_contract::SessionRealizationControlFailure::NotFound
                                )
                            {
                                terminal_error.get_or_insert_with(|| {
                                    crate::HostError::internal(format!(
                                        "Session `{session_id}` terminal cleanup receipt remained pending: {error}"
                                    ))
                                });
                            }
                        }
                        Err(error) => {
                            terminal_error.get_or_insert_with(|| {
                                crate::HostError::internal(format!(
                                    "Session `{session_id}` terminal cleanup effect remained pending: {error}"
                                ))
                            });
                        }
                    }
                }
                match terminal_error {
                    Some(error) => Err(error),
                    None => Ok(true),
                }
            }
            Ok(None) => Ok(false),
            Err(awaken_session_contract::SessionRealizationControlFailure::NotFound) => {
                let _ = self.interrupt(session_id).await;
                self.revoke_session_realization(session_id).await;
                Ok(true)
            }
            Err(error) => Err(crate::HostError::internal(format!(
                "Session `{session_id}` terminal cleanup control remained pending: {error}"
            ))),
        }
    }

    /// Claim and install every currently discoverable cold terminal assignment,
    /// then enter the same cleanup helper as resident projections. The bound
    /// prevents one heartbeat from monopolizing the Worker; repeated calls are
    /// safe because Control skips assignments already owned by this incarnation.
    pub async fn recover_terminal_cleanup_assignments(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<usize, crate::HostError> {
        const MAX_ASSIGNMENTS_PER_HEARTBEAT: usize = 64;

        let control = self.session_control.as_ref().ok_or_else(|| {
            crate::HostError::internal(
                "active Worker Session projection has no Control renewal client",
            )
        })?;
        let mut recovered = 0;
        let mut terminal_error = None;
        for _ in 0..MAX_ASSIGNMENTS_PER_HEARTBEAT {
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
            if let Err(error) =
                crate::host::HostWorkerResolver::install_terminal_cleanup_assignment(
                    self,
                    &assignment,
                )
                .await
            {
                terminal_error.get_or_insert_with(|| {
                    crate::HostError::internal(format!(
                        "Session `{}` terminal cleanup projection remained pending: {error}",
                        assignment.session_id
                    ))
                });
                continue;
            }
            recovered += 1;
            match self
                .reconcile_terminal_cleanup_for_lease(
                    control.as_ref(),
                    &assignment.session_id,
                    &assignment.lease,
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    // Another actor completed the operation between claim and
                    // poll. This projection has no ordinary Run authority.
                    self.revoke_session_realization(&assignment.session_id)
                        .await;
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

    /// Reconcile terminal cleanup first, then renew every active Session
    /// realization approaching expiry through the same Control phase protocol
    /// used for initial creation and hot replacement.
    /// Environment-only Sessions participate because image/package realization
    /// can outlive the initial lease even when no MCP attachment exists.
    /// A failed renewal revokes that Session's process-local projection before
    /// the batch continues. Session authority is narrower than Worker registry
    /// authority: an expired or terminal Session realization must not fence unrelated
    /// in-flight Sessions from the same Worker incarnation.
    pub async fn renew_due_session_realizations(
        &self,
        renew_before_unix_ms: u64,
        requested_expiry_unix_ms: u64,
    ) -> Result<usize, crate::HostError> {
        let realizations = self.session_slots.realization_leases();
        if realizations.is_empty() {
            return Ok(0);
        }
        let control = self.session_control.as_ref().ok_or_else(|| {
            crate::HostError::internal(
                "active Worker Session projection has no Control renewal client",
            )
        })?;
        let mut renewed = 0;
        let mut terminal_error = None;
        for (session_id, lease) in &realizations {
            // Cause/effect decision table: C1 the aggregate has no terminal
            // fence, C2 it is Fenced, C3 it has missing exact commands, C4 all
            // completions are already recorded, and C5 Control is temporarily
            // unavailable. Effects: E1 ordinary renewal; E2 retain the slot
            // without teardown; E3 attempt every command and record each exact
            // receipt; E4 let Control complete and then retire the now-empty
            // projection; E5 retain recoverable local state. No Worker-local
            // cleanup queue or completion registry participates.
            //
            // | Rule | Control projection | Effect |
            // | R1 | None | E1 when due |
            // | R2 | Some([]) | E2 |
            // | R3 | Some(commands) | E3, retry failures cold |
            // | R4 | NotFound after settlement | E4 |
            // | R5 | Unavailable | E5 |
            match self
                .reconcile_terminal_cleanup_for_lease(control.as_ref(), session_id, lease)
                .await
            {
                Ok(true) => {
                    // Even an empty batch is a durable terminal fence. Never
                    // reinterpret retired Work as ordinary projection revocation
                    // while cleanup is waiting or retrying.
                    continue;
                }
                Ok(false) => {}
                Err(error) => {
                    terminal_error.get_or_insert(error);
                    continue;
                }
            }
            if lease.expires_at_unix_ms > renew_before_unix_ms {
                continue;
            }
            let renewal = async {
                let directive = match control
                    .begin_session_realization(awaken_session_contract::BeginSessionRealization {
                        session_id: session_id.clone(),
                        target: awaken_session_contract::SessionRealizationTarget {
                            owner: lease.owner.clone(),
                            runtime_incarnation: lease.runtime_incarnation.clone(),
                            lease_expires_at_unix_ms: requested_expiry_unix_ms,
                            renew_existing_lease: true,
                            reassign_existing_lease: false,
                        },
                    })
                    .await
                {
                    Ok(directive) => directive,
                    Err(error)
                        if realization_renewal_failure_disposition(&error)
                            == RenewalFailureDisposition::Retire =>
                    {
                        return Ok(false);
                    }
                    Err(error) => return Err(crate::HostError::internal(error.to_string())),
                };
                let realization = self.session_slots.realization_lock(session_id);
                let Ok(_realization) = realization.try_lock() else {
                    // Control has extended only the same owner/incarnation/epoch.
                    // Preserve that authority locally, but never start a second
                    // effect driver. The active driver compares exact generation
                    // fences at Activate/Acknowledge and catches up before it can
                    // report completion.
                    self.install_session_realization_lease(session_id, directive.lease.clone());
                    return Ok::<bool, crate::HostError>(true);
                };
                crate::host::HostWorkerResolver::drive_session_realization(
                    self,
                    control.as_ref(),
                    session_id,
                    directive,
                    None,
                    None,
                    false,
                )
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
                Ok::<bool, crate::HostError>(true)
            }
            .await;
            if matches!(renewal, Ok(true)) {
                renewed += 1;
                continue;
            }
            if let Err(error) = &renewal {
                eprintln!(
                    "Session realization renewal lost authority for `{session_id}`; revoking only that Session: {error}"
                );
            }
            // Every failed renewal has one cleanup path. Expected terminal
            // retirement differs only in observability, never in side effects.
            let _ = self.interrupt(session_id).await;
            self.revoke_session_realization(session_id).await;
        }
        match terminal_error {
            Some(error) => Err(error),
            None => Ok(renewed),
        }
    }

    /// Remove one process-local realization after its continuing authority is
    /// no longer provable. This is not a terminal Session edge: preserve the
    /// durable Sandbox so the next authorized Worker can adopt it, while local
    /// processes, routes, credentials, and runtime references are discarded.
    async fn revoke_session_realization(&self, session_id: &str) -> bool {
        let Some(lifecycle) = self
            .session_slots
            .read(session_id, |slot| slot.lifecycle.clone())
        else {
            return false;
        };
        let _lifecycle = lifecycle.lock().await;
        self.stop_session_mcp_processes(session_id).await;
        let Some(slot) = self.session_slots.remove(session_id) else {
            return false;
        };
        if let Some(environment) = slot
            .environment
            .or_else(|| slot.runtime.and_then(|runtime| runtime.env.clone()))
        {
            environment.stop_bound_processes().await;
        }
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_routes(session_id);
        }
        true
    }

    /// Revoke every process-local Session projection after Worker authority is
    /// no longer provable. Durable Session environments remain available for
    /// adoption; only terminal Session lifecycle commands may dispose them.
    pub async fn revoke_all_session_realizations(&self) -> Result<usize, crate::HostError> {
        let session_ids = self.session_slots.session_ids();
        let mut revoked = 0;
        for session_id in session_ids {
            if self.revoke_session_realization(&session_id).await {
                revoked += 1;
            }
        }
        Ok(revoked)
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
            if self.interrupt(&session_id).await.is_ok() {
                interrupted += 1;
            }
        }
        interrupted
    }

    /// Install the protocol-neutral Runtime coordinates derived from one frozen
    /// Session projection. Managed local preparation and claimed Worker replay
    /// share this single lowering path; neither may independently reconstruct a
    /// different workspace, Agent, model, backend, Environment, or tool surface.
    pub(crate) fn project_session_init(
        &self,
        thread: &str,
        init: &awaken_session_contract::SessionInit,
    ) -> Result<(), crate::HostError> {
        // Validate the only fallible coordinate before publishing the remaining
        // fields, so a conflicting Environment cannot leave a partial update.
        self.install_environment_projection(thread, &init.environment)?;
        self.register_thread_workspace(thread, &init.workspace_id);
        self.register_thread_agent_projection(thread, &init.agent_id);
        if let Some(model) = &init.model {
            self.register_thread_model(thread, model);
        }
        if let Some(backend_ref) = &init.runtime {
            self.register_thread_backend_projection(thread, backend_ref);
        }
        self.register_thread_delegates(thread, init.delegate_ids.clone());
        self.session_slots.update(thread, |slot| {
            slot.agent_id = Some(init.agent_id.clone());
            slot.tools = init.tools.clone();
        });
        Ok(())
    }

    pub(crate) async fn install_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
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
        let expected_environment_binding = projection.environment.binding().map(str::to_owned);
        let baseline = baseline_projection(&projection.baseline);
        let init = projection.session_init();

        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.baseline.clone())
            .flatten()
        {
            if existing.fingerprint != baseline.fingerprint {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Session baseline"
                )));
            }
            self.install_expected_environment_binding(
                thread,
                expected_environment_binding.clone(),
            )?;
            // The baseline is immutable, but a remote Resource verification is
            // authorized by the current dispatch claim. Re-stage the exact
            // manifest on every claimed replay so Repository checks never retain
            // a prior lease epoch. `install_dispatched_resources` owns generation
            // equality/fencing and does not realize an existing environment.
            if synchronize_resources && projection.resource_revision > 0 {
                let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
                    projection.workspace_id.clone(),
                    projection.resource_revision,
                    projection.resources.clone(),
                );
                self.install_dispatched_resources(thread, &manifest, claim)
                    .await
                    .map_err(|error| crate::HostError::internal(error.to_string()))?;
            }
            self.project_session_init(thread, &init)?;
            self.install_session_request_context(thread, projection.request_context.clone());
            self.session_slots.update(thread, |slot| {
                slot.has_mcp_projection = has_mcp_projection;
            });
            return Ok(());
        }

        let occupied = self.session_slots.read(thread, |slot| {
            (
                slot.runtime.is_some() || slot.environment.is_some(),
                slot.resources.mounts.clone(),
            )
        });
        let (is_realized, built_in_mounts) = occupied.unwrap_or_else(|| (false, Vec::new()));
        if is_realized {
            return Err(crate::HostError::internal(format!(
                "thread {thread} was realized before its frozen Session baseline"
            )));
        }

        validate_baseline_projection(&baseline, &built_in_mounts)?;
        self.install_expected_environment_binding(thread, expected_environment_binding)?;
        self.project_session_init(thread, &init)?;
        self.install_session_request_context(thread, projection.request_context.clone());
        if !synchronize_resources && projection.resource_revision > 0 {
            return Err(crate::HostError::internal(format!(
                "thread {thread} cannot cold-materialize frozen Session Resources during lease-only renewal"
            )));
        }
        if synchronize_resources && projection.resource_revision > 0 {
            let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
                projection.workspace_id.clone(),
                projection.resource_revision,
                projection.resources,
            );
            self.install_dispatched_resources(thread, &manifest, claim)
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
        }
        self.session_slots.update(thread, |slot| {
            slot.baseline = Some(baseline);
            slot.has_mcp_projection = has_mcp_projection;
        });
        Ok(())
    }

    /// Install the immutable baseline for the co-located realization path.
    /// Claimed Workers use `install_frozen_session_projection`; local Native
    /// execution reaches this method through the protocol-neutral SessionRuntime
    /// port before `prepare_session`, so both paths expose identical mounts,
    /// environment values, and prompts.
    pub(crate) fn install_frozen_session_baseline(
        &self,
        thread: &str,
        baseline: &awaken_session_contract::SessionBaseline,
    ) -> Result<(), crate::HostError> {
        if baseline.fingerprint.0.trim().is_empty() {
            return Err(crate::HostError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        let baseline = baseline_projection(baseline);
        let current = self.session_slots.read(thread, |slot| {
            (
                slot.baseline.clone(),
                slot.environment.is_some(),
                slot.resources.mounts.clone(),
            )
        });
        let (existing, environment_realized, resource_mounts) =
            current.unwrap_or_else(|| (None, false, Vec::new()));
        if let Some(existing) = existing {
            if existing.fingerprint != baseline.fingerprint {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Session baseline"
                )));
            }
            return Ok(());
        }
        if environment_realized {
            return Err(crate::HostError::internal(format!(
                "thread {thread} was realized before its frozen Session baseline"
            )));
        }
        validate_baseline_projection(&baseline, &resource_mounts)?;
        self.session_slots
            .update(thread, |slot| slot.baseline = Some(baseline));
        Ok(())
    }

    pub(crate) fn install_environment_projection(
        &self,
        thread: &str,
        environment: &awaken_session_contract::EnvironmentSnapshot,
    ) -> Result<(), crate::HostError> {
        let projection = crate::provisioning::project_environment(environment);
        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.environment_projection.clone())
            .flatten()
        {
            if existing.fingerprint != projection.fingerprint {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Environment"
                )));
            }
            self.session_slots.update(thread, |slot| {
                slot.environment_snapshot = Some(environment.clone())
            });
            return Ok(());
        }
        self.session_slots.update(thread, |slot| {
            slot.environment_projection = Some(projection);
            slot.environment_snapshot = Some(environment.clone());
        });
        Ok(())
    }

    pub(crate) fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::MountRequirement> {
        self.session_slots
            .read(thread, |slot| {
                let mut mounts = slot.resources.mounts.clone();
                if let Some(baseline) = &slot.baseline {
                    mounts.extend(baseline.mounts.clone());
                }
                mounts
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

    #[cfg(test)]
    pub(crate) fn thread_session_prompts(&self, thread: &str) -> Vec<String> {
        self.session_slots.prompts(thread)
    }
}

fn baseline_projection(
    baseline: &awaken_session_contract::SessionBaseline,
) -> crate::session_slot::FrozenBaselineRuntimeProjection {
    crate::session_slot::FrozenBaselineRuntimeProjection {
        fingerprint: baseline.fingerprint.clone(),
        agent_id: baseline.agent_id.clone(),
        agent_revision: baseline.agent_revision,
        model_override: baseline.model_override.clone(),
        mounts: baseline.mounts.clone(),
        env: baseline.env.clone(),
        prompts: baseline.prompts.clone(),
    }
}

fn validate_baseline_projection(
    baseline: &crate::session_slot::FrozenBaselineRuntimeProjection,
    built_in_mounts: &[awaken_provisioning_contract::MountRequirement],
) -> Result<(), crate::HostError> {
    let mut mount_ids: HashSet<&str> = built_in_mounts
        .iter()
        .map(|mount| mount.mount_id.as_str())
        .collect();
    let mut mount_paths: HashSet<&str> = built_in_mounts
        .iter()
        .map(|mount| mount.mount_path.as_str())
        .collect();
    for mount in &baseline.mounts {
        if mount.mount_id.trim().is_empty()
            || mount.mount_path.trim().is_empty()
            || !mount_ids.insert(&mount.mount_id)
            || !mount_paths.insert(&mount.mount_path)
        {
            return Err(crate::HostError::internal(
                "frozen Session baseline has an empty or conflicting mount",
            ));
        }
    }

    let mut env_names = HashSet::new();
    for env in &baseline.env {
        if env.name.trim().is_empty() || !env_names.insert(env.name.as_str()) {
            return Err(crate::HostError::internal(
                "frozen Session baseline has an empty or duplicate environment variable",
            ));
        }
    }

    Ok(())
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
