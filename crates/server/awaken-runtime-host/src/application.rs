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
use futures_util::stream::{self, StreamExt};

pub(crate) const MAX_CONCURRENT_SESSION_REALIZATION_RECONCILIATIONS: usize = 8;

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
        let prompts = self.slots.prompts(&self.session_id);
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

/// Bridges the explicitly-authored automatic Memory extension into ACP's
/// request-only context. Standard Skills and MemoryStore bindings do not pass
/// through this adapter: they use the shared filesystem/semantic-tool delivery
/// selected for the Session.
pub(crate) struct AcpContextAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    memory: Option<awaken_ext_memory::MemoryRecall>,
}

impl AcpContextAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        memory: Option<awaken_ext_memory::MemoryRecall>,
    ) -> Self {
        Self { inner, memory }
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
            slot.baseline = Some(crate::session_slot::FrozenBaselineRuntimeProjection {
                fingerprint: awaken_session_contract::SessionBaselineFingerprint("baseline".into()),
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                system_prompt: awaken_session_contract::SessionSystemPromptSelection::Inherit,
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: vec!["frozen session prompt".into()],
            });
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
        );

        let acp = activation("acp:codex");
        let durable_input = acp.input.clone();
        let mut acp_context = awaken_runtime_contract::RuntimeRunContext::default();
        loader.load_context(&acp, &mut acp_context).await;
        assert_eq!(acp_context.request_context.len(), 1, "A1");
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
                let root_was_ready = commands
                    .iter()
                    .any(|command| command.thread_id == session_id);
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
                if let Some(error) = terminal_error {
                    return Err(error);
                }
                if root_was_ready {
                    return Ok(true);
                }

                // Publication is the sole root-owned effect between child
                // cleanup and root finalization. Its command and receipt remain
                // projections of the same aggregate operation; no Worker-local
                // queue or completion registry is introduced.
                match control
                    .terminal_repository_publication_command(session_id, lease)
                    .await
                {
                    Ok(Some(projection)) => {
                        if self.thread_workspace(session_id) != projection.workspace_id {
                            return Err(crate::HostError::internal(format!(
                                "Session `{session_id}` terminal Repository publication Workspace does not match its frozen Runtime projection"
                            )));
                        }
                        let receipt = self
                            .execute_dispatched_terminal_repository_publication(
                                projection.command,
                                lease,
                            )
                            .await
                            .map_err(|error| {
                                crate::HostError::internal(format!(
                                    "Session `{session_id}` terminal Repository publication remained pending: {error}"
                                ))
                            })?;
                        control
                            .record_terminal_repository_publication_receipt(
                                session_id,
                                lease,
                                receipt,
                            )
                            .await
                            .map_err(|error| {
                                crate::HostError::internal(format!(
                                    "Session `{session_id}` terminal Repository publication receipt remained pending: {error}"
                                ))
                            })?;
                    }
                    Ok(None) => {}
                    Err(awaken_session_contract::SessionRealizationControlFailure::NotFound) => {
                        return Ok(true);
                    }
                    Err(error) => {
                        return Err(crate::HostError::internal(format!(
                            "Session `{session_id}` terminal Repository publication control remained pending: {error}"
                        )));
                    }
                }

                // Re-read the aggregate after child/publication receipts. Only
                // its canonical pending-command projection may expose the root
                // finalizer that disposes the retained Environment.
                let root_commands = match control.terminal_cleanup_commands(session_id, lease).await
                {
                    Ok(Some(commands)) => commands,
                    Ok(None)
                    | Err(awaken_session_contract::SessionRealizationControlFailure::NotFound) => {
                        return Ok(true);
                    }
                    Err(error) => {
                        return Err(crate::HostError::internal(format!(
                            "Session `{session_id}` terminal root cleanup control remained pending: {error}"
                        )));
                    }
                };
                for command in root_commands {
                    let completion = self
                        .execute_dispatched_terminal_cleanup(command)
                        .await
                        .map_err(|error| {
                            crate::HostError::internal(format!(
                                "Session `{session_id}` terminal root cleanup remained pending: {error}"
                            ))
                        })?;
                    control
                        .record_terminal_cleanup_completion(lease, completion)
                        .await
                        .map_err(|error| {
                            crate::HostError::internal(format!(
                                "Session `{session_id}` terminal root cleanup receipt remained pending: {error}"
                            ))
                        })?;
                }
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(awaken_session_contract::SessionRealizationControlFailure::NotFound) => {
                let _ = self.interrupt(session_id).await;
                self.revoke_session_realization(session_id).await?;
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
                    if let Err(error) = self
                        .revoke_session_realization(&assignment.session_id)
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

    async fn reconcile_one_session_realization(
        &self,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: String,
        lease: awaken_session_contract::SessionRealizationLease,
        renew_before_unix_ms: u64,
        requested_expiry_unix_ms: u64,
    ) -> Result<bool, crate::HostError> {
        // Cause/effect decision table: C1 the aggregate has no terminal
        // fence, C2 it is Fenced, C3 child cleanup is pending, C4 exact
        // Repository publication is pending, C5 root cleanup is ready, and
        // C6 Control/effect is temporarily unavailable. Effects: E1 ordinary
        // renewal; E2 retain the slot; E3 record children before publication;
        // E4 record publication before root disposal; E5 finalize and retire;
        // E6 retain recoverable local state. No Worker-local queue or receipt
        // registry participates.
        //
        // | Rule | Control projection | Effect |
        // | R1 | None | E1 when due |
        // | R2 | Some([]) | E2 |
        // | R3 | Some(child commands) | E3, then re-poll |
        // | R4 | publication command | E4, then re-poll |
        // | R5 | Some(root command) / NotFound | E5 |
        // | R6 | Unavailable | E6 |
        match self
            .reconcile_terminal_cleanup_for_lease(control, &session_id, &lease)
            .await
        {
            Ok(true) => {
                // Even an empty batch is a durable terminal fence. Never
                // reinterpret retired Work as ordinary projection revocation
                // while cleanup is waiting or retrying.
                return Ok(false);
            }
            Ok(false) => {}
            Err(error) => return Err(error),
        }
        if lease.expires_at_unix_ms > renew_before_unix_ms {
            return Ok(false);
        }
        let renewal = async {
            let renewal_command = || awaken_session_contract::BeginSessionRealization {
                session_id: session_id.clone(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: lease.owner.clone(),
                    runtime_incarnation: lease.runtime_incarnation.clone(),
                    lease_expires_at_unix_ms: requested_expiry_unix_ms,
                    renew_existing_lease: true,
                    reassign_existing_lease: false,
                },
            };
            let mut directive = match control.begin_session_realization(renewal_command()).await {
                Ok(directive) => directive,
                Err(error)
                    if realization_renewal_failure_disposition(&error)
                        == RenewalFailureDisposition::Retire =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(crate::HostError::internal(error.to_string())),
            };
            let realization = self.session_slots.realization_lock(&session_id);
            let Ok(_realization) = realization.try_lock() else {
                // Control has extended only the same owner/incarnation/epoch.
                // Preserve that authority locally, but never start a second
                // effect driver. The active driver compares exact generation
                // fences at Activate/Acknowledge and catches up before it can
                // report completion.
                self.install_session_realization_lease(&session_id, directive.lease.clone());
                return Ok::<bool, crate::HostError>(true);
            };
            // A Session command can advance the aggregate after Begin but
            // before Activate/Acknowledge. Keep the one realization lock,
            // re-read Control, and replay the same idempotent driver. This
            // closes the renewal-versus-successor-Run race without a second
            // projection owner or a provider-specific retry path.
            const MAX_CONTROL_CONFLICT_ATTEMPTS: usize = 2;
            for attempt in 0..MAX_CONTROL_CONFLICT_ATTEMPTS {
                match crate::host::HostWorkerResolver::drive_session_realization_raw(
                    self,
                    control,
                    &session_id,
                    directive,
                    None,
                    None,
                    false,
                )
                .await
                {
                    Ok(()) => return Ok::<bool, crate::HostError>(true),
                    Err(awaken_session_contract::SessionRealizationDriveError::Control(
                        awaken_session_contract::SessionRealizationControlFailure::Conflict,
                    )) if attempt + 1 < MAX_CONTROL_CONFLICT_ATTEMPTS => {
                        directive = control
                            .begin_session_realization(renewal_command())
                            .await
                            .map_err(|error| crate::HostError::internal(error.to_string()))?;
                    }
                    Err(error) => {
                        return Err(crate::HostError::internal(error.to_string()));
                    }
                }
            }
            unreachable!("bounded realization conflict loop returns on every branch")
        }
        .await;
        if matches!(renewal, Ok(true)) {
            return Ok(true);
        }
        if let Err(error) = &renewal {
            eprintln!(
                "Session realization renewal lost authority for `{session_id}`; revoking only that Session: {error}"
            );
        }
        // Every failed renewal has one cleanup path. Expected terminal
        // retirement differs only in observability, never in side effects.
        let _ = self.interrupt(&session_id).await;
        self.revoke_session_realization(&session_id).await?;
        Ok(false)
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
    ///
    /// Each Session keeps its one realization lock and phase driver. Independent
    /// Sessions are reconciled with a fixed upper bound so one large resident set
    /// cannot serialize Control traffic past the Worker authority proof window.
    pub async fn renew_due_session_realizations(
        &self,
        renew_before_unix_ms: u64,
        requested_expiry_unix_ms: u64,
    ) -> Result<usize, crate::HostError> {
        let mut realizations = self.session_slots.realization_leases();
        if realizations.is_empty() {
            return Ok(0);
        }
        let control = self.session_control.as_ref().ok_or_else(|| {
            crate::HostError::internal(
                "active Worker Session projection has no Control renewal client",
            )
        })?;
        // Earliest deadline first prevents HashMap iteration order from starving
        // one Session during a sustained recovery wave. Session id is the stable
        // tie-breaker and carries no scheduling authority of its own.
        realizations.sort_by(|left, right| {
            left.1
                .expires_at_unix_ms
                .cmp(&right.1.expires_at_unix_ms)
                .then_with(|| left.0.cmp(&right.0))
        });
        let outcomes = stream::iter(realizations)
            .map(|(session_id, lease)| {
                let control = Arc::clone(control);
                async move {
                    self.reconcile_one_session_realization(
                        control.as_ref(),
                        session_id,
                        lease,
                        renew_before_unix_ms,
                        requested_expiry_unix_ms,
                    )
                    .await
                }
            })
            .buffer_unordered(MAX_CONCURRENT_SESSION_REALIZATION_RECONCILIATIONS)
            .collect::<Vec<_>>()
            .await;
        let mut renewed = 0;
        let mut first_error = None;
        for outcome in outcomes {
            match outcome {
                Ok(true) => renewed += 1,
                Ok(false) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(renewed),
        }
    }

    /// Remove one process-local realization after its continuing authority is
    /// no longer provable. This is not a terminal Session edge: preserve the
    /// durable Sandbox so the next authorized Worker can adopt it, while local
    /// processes, routes, credentials, and runtime references are discarded.
    async fn revoke_session_realization(&self, session_id: &str) -> Result<bool, crate::HostError> {
        if self.session_slots.read(session_id, |_| ()).is_none() {
            return Ok(false);
        }
        let realization = self.session_slots.realization_lock(session_id);
        let _realization = realization.lock().await;
        let Some(lifecycle) = self
            .session_slots
            .read(session_id, |slot| slot.lifecycle.clone())
        else {
            return Ok(false);
        };
        let _lifecycle = lifecycle.lock().await;
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
        let session_ids = self.session_slots.session_ids();
        let mut revoked = 0;
        for session_id in session_ids {
            if self.revoke_session_realization(&session_id).await? {
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
        realization_lease: Option<awaken_session_contract::SessionRealizationLease>,
    ) -> Result<(), crate::HostError> {
        self.install_frozen_session_projection_with_resource_authority(
            thread,
            projection,
            claim,
            synchronize_resources,
            false,
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
            thread, projection, None, true, true, None,
        )
        .await
    }

    async fn install_frozen_session_projection_with_resource_authority(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
        authority_amends_unattempted_resources: bool,
        realization_lease: Option<awaken_session_contract::SessionRealizationLease>,
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
        let baseline = baseline_projection(&projection.baseline);
        let init = projection.session_init();
        match awaken_session_contract::frozen_agent_publication_decision(
            &projection.baseline,
            projection.agent_publication.as_ref(),
        ) {
            awaken_session_contract::FrozenAgentPublicationDecision::Unpinned
            | awaken_session_contract::FrozenAgentPublicationDecision::OptionalMissing
            | awaken_session_contract::FrozenAgentPublicationDecision::Exact => {}
            awaken_session_contract::FrozenAgentPublicationDecision::MissingRequired => {
                return Err(crate::HostError::internal(
                    "Worker Session realization requires its exact frozen Agent publication",
                ));
            }
            awaken_session_contract::FrozenAgentPublicationDecision::Mismatch => {
                return Err(crate::HostError::internal(
                    "Session Agent publication does not match its frozen identity, revision, or runtime",
                ));
            }
        }
        if let Some(asserted) = &projection.agent_publication
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
        let environment_projection = crate::provisioning::project_environment(&init.environment);
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
            let occupied = self.session_slots.read(thread, |slot| {
                (
                    slot.runtime.is_some() || slot.environment_owner.has_local_environment(),
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
            Some(baseline)
        };

        if baseline_to_install.is_some()
            && !synchronize_resources
            && projection.resource_revision > 0
        {
            return Err(crate::HostError::internal(format!(
                "thread {thread} cannot cold-materialize frozen Session Resources during lease-only renewal"
            )));
        }
        // The baseline is immutable, but a remote Resource verification is
        // authorized by the current dispatch claim. Re-stage the exact
        // manifest on every claimed replay so Repository checks never retain a
        // prior lease epoch. Resource installation owns generation equality and
        // fencing; every cold/replay case then joins the one projection publish
        // sequence below.
        if synchronize_resources && projection.resource_revision > 0 {
            let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
                projection.workspace_id.clone(),
                projection.resource_revision,
                projection.resources.clone(),
            );
            let installed = if authority_amends_unattempted_resources {
                self.amend_unattempted_dispatched_resources(thread, &manifest)
                    .await
            } else {
                self.install_dispatched_resources(thread, &manifest, claim)
                    .await
            };
            installed.map_err(|error| crate::HostError::internal(error.to_string()))?;
        }
        self.install_session_environment_owner_projection(
            thread,
            &projection.workspace_id,
            &projection.environment,
        )?;
        self.project_session_init(thread, &init)?;
        self.install_session_request_context(thread, projection.request_context.clone());
        self.session_slots.update(thread, |slot| {
            if let Some(baseline) = baseline_to_install {
                slot.baseline = Some(baseline);
            }
            slot.has_mcp_projection = has_mcp_projection;
            if let Some(publication) = projection.agent_publication {
                slot.published_snapshot = Some(publication);
            }
        });
        if let Some(lease) = realization_lease {
            self.install_session_realization_lease(thread, lease);
        }
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
                if slot.content_delivery
                    == Some(crate::session_slot::ManagedContentDelivery::SemanticTools)
                {
                    mounts.retain(|mount| {
                        !matches!(
                            mount.source,
                            awaken_provisioning_contract::MountSource::MemoryStore { .. }
                        )
                    });
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
        system_prompt: (*baseline.system_prompt).clone(),
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
