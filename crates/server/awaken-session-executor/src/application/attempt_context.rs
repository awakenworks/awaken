// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: Projects one frozen Session into the existing complete attempt route; derived request context is never another persisted authority.
//! Session and ACP attempt-context projection, including its behavior tests.

use super::agent_filesystem_context::consistent_agent_filesystem_context;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{ExecutorCapabilities, RunAttemptExecutor};
use std::sync::Arc;

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
    slots: Arc<crate::installed_session::InstalledSessions>,
    session_id: String,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl SessionContextAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        slots: Arc<crate::installed_session::InstalledSessions>,
        session_id: impl Into<String>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            inner,
            slots,
            session_id: session_id.into(),
            clock,
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
                slot.inputs.tools.as_ref().map(|tools| {
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
        let seed = self
            .slots
            .read(&self.session_id, |slot| {
                slot.inputs.baseline.as_ref().map(|baseline| {
                    awaken_session_configuration_contract::SessionContextSeed::from_prompts(
                        &baseline.prompts,
                    )
                })
            })
            .flatten()
            .unwrap_or_else(|| {
                awaken_session_configuration_contract::SessionContextSeed::from_prompts(&[])
            });
        {
            let seed = seed.map_err(|error| {
                awaken_runtime_contract::execution::Error::Resolution(error.to_string())
            })?;
            context.request_timezone = Some(seed.timezone().to_owned());
            let prefix = format!("session-environment:{}:", self.session_id);
            let id = MessageId(format!(
                "{prefix}{}",
                awaken_agent_contract::stable_fingerprint(&seed)
            ));
            context
                .request_context
                .retain(|message| !message.id.0.starts_with(&prefix) || message.id == id);
            if !context
                .request_context
                .iter()
                .any(|message| message.id == id)
            {
                let prompt = seed
                    .render_at(self.clock.now().as_epoch_ms())
                    .map_err(|error| {
                        awaken_runtime_contract::execution::Error::Resolution(error.to_string())
                    })?;
                context
                    .request_context
                    .push(Message::text(id, Role::System, prompt));
            }
        }
        let prompts = self
            .slots
            .realizations()
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
                            awaken_agent_contract::stable_fingerprint(&(
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
            .identity()
            .2
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

    #[test]
    fn frozen_environment_context_is_shared_request_only_and_stable_per_attempt() {
        // Causes: C1 Native/ACP; C2 frozen timezone/source; C3 clock crosses
        // local midnight; C4 same/new attempt; C5 corrupt seed. Effects:
        // E1 identical task facts; E2 no seed/durable-input leakage; E3 same
        // attempt byte-stable; E4 fresh attempt updates date; E5 reject C5.
        // Rules A1 C1+C2=>E1+E2; A2 C3+same=>E3; A3 C3+new=>E4; A4 C5=>E5.
        use awaken_session_configuration_contract as sc;
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(
            awaken_session_application_contract::rfc3339_to_epoch_millis("2026-09-29T15:59:59Z")
                .unwrap(),
        ));
        let seed = sc::SessionContextSeed::from_metadata(
            sc::SessionTrigger::Interactive {},
            &std::collections::BTreeMap::from([(
                "awaken.timezone".into(),
                "Asia/Singapore".into(),
            )]),
            None,
        )
        .unwrap();
        let baseline = sc::SessionBaseline::compile(sc::SessionBaselineInputs {
            environment: sc::EnvironmentSnapshot {
                environment_id: "environment".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: true,
                config_fingerprint: sc::EnvironmentFingerprint("fp".into()),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: sc::SessionNetworkPolicy::Unrestricted,
                credential_realization:
                    awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
            },
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model: "model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: vec![seed.encode(), "product context".into()],
            transcript_prefix: None,
        });
        let slots = Arc::new(crate::installed_session::InstalledSessions::default());
        let executor = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
            clock.clone(),
        );
        // A5 a baseline arrives after construction/legacy projection: replace
        // the owned environment message, never pair its old UTC text with new TZ.
        let mut late = awaken_runtime_contract::RuntimeRunContext::new();
        executor.project(activation("genai"), &mut late).unwrap();
        assert_eq!(late.request_timezone.as_deref(), Some("UTC"));
        slots.replace_frozen_with("session-1", |next| next.baseline = Some(baseline.clone()));
        executor.project(activation("genai"), &mut late).unwrap();
        assert_eq!(
            late.request_context
                .iter()
                .filter(|message| message.id.0.starts_with("session-environment:"))
                .count(),
            1,
            "A5 one owned projection"
        );
        assert!(
            late.request_context
                .iter()
                .any(|message| message.text_content().contains("timezone=Asia/Singapore\n")),
            "A5 coherent new seed"
        );
        let mut observed = Vec::new();
        for backend in ["genai", "acp:claude"] {
            let input = activation(backend);
            let mut context = awaken_runtime_contract::RuntimeRunContext::new();
            let projected = executor.project(input.clone(), &mut context).unwrap();
            assert_eq!(projected.input, input.input, "E2 durable input");
            assert_eq!(context.request_timezone.as_deref(), Some("Asia/Singapore"));
            let texts = context
                .request_context
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>();
            assert_eq!(texts.len(), 2, "one environment + existing product context");
            assert!(texts[0].contains("current_date=2026-09-29\n"));
            assert!(
                texts
                    .iter()
                    .all(|text| !text.contains("AWAKEN_SESSION_CONTEXT_")),
                "E2 consumed seed"
            );
            observed.push(texts);
        }
        assert_eq!(observed[0], observed[1], "E1 Native/ACP");
        let mut context = awaken_runtime_contract::RuntimeRunContext::new();
        executor.project(activation("genai"), &mut context).unwrap();
        let before = context.request_context.clone();
        clock.advance(1_000);
        executor.project(activation("genai"), &mut context).unwrap();
        assert_eq!(
            context.request_context, before,
            "E3 provider/reprojection retry"
        );
        let mut fresh = awaken_runtime_contract::RuntimeRunContext::new();
        executor
            .project(activation("acp:claude"), &mut fresh)
            .unwrap();
        assert!(
            fresh.request_context[0]
                .text_content()
                .contains("current_date=2026-09-30\n"),
            "E4 restart/resume"
        );
        assert_eq!(
            slots
                .read("session-1", |slot| slot.inputs.baseline.clone())
                .unwrap(),
            Some(baseline),
            "frozen truth unchanged"
        );
        slots.update("session-1", |slot| {
            let mut baseline = slot.inputs.baseline.clone().unwrap();
            baseline.prompts.push(seed.encode());
            slot.replace_frozen_with(|next| next.baseline = Some(baseline))
        });
        assert!(
            executor.project(activation("genai"), &mut fresh).is_err(),
            "E5 before inference"
        );
    }

    /// Session context FMECA cause/effect graph: C1=frozen prompts exist,
    /// C2=durable User input exists, C3=the exact prompt is already in this
    /// attempt context. Effects: E1=append one request-only System message;
    /// E2=leave activation input byte-stable; E3=do not duplicate a replay.
    /// Rules: C1+C2+!C3 -> E1+E2; C1+*+C3 -> E2+E3; !C1 -> E2.
    #[test]
    fn session_context_is_request_only_and_attempt_idempotent() {
        let slots = Arc::new(crate::installed_session::InstalledSessions::default());
        let executor = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
            awaken_time::system_clock(),
        );
        // Construction happens before the claim-fenced frozen projection is
        // accepted; realization installs the one authoritative slot later.
        slots.realizations().update_realization("session-1", |r| {
            r.staging.resources.prompts = vec!["frozen session prompt".into()];
        });
        let durable = activation("genai");
        let mut context = awaken_runtime_contract::RuntimeRunContext::default();
        let projected = executor.project(durable.clone(), &mut context).unwrap();
        assert_eq!(projected.input, durable.input, "E2");
        assert_eq!(
            context.request_context.len(),
            2,
            "environment + C1+C2+!C3 -> E1"
        );
        assert_eq!(context.request_context[0].role, Role::System, "E1");
        assert_eq!(
            context.request_context[1].text_content(),
            "frozen session prompt",
            "E1"
        );
        let replayed = executor.project(projected, &mut context).unwrap();
        assert_eq!(context.request_context.len(), 2, "C1+C3 -> E3");
        assert_eq!(replayed.input, durable.input, "E2");

        let empty = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            Arc::new(crate::installed_session::InstalledSessions::default()),
            "session-1",
            awaken_time::system_clock(),
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
        assert_eq!(
            empty_context.request_context.len(),
            1,
            "!C1 still supplies task time"
        );
        assert!(
            empty_context.request_context[0]
                .text_content()
                .contains("timezone=UTC\n")
        );
    }

    #[test]
    fn resource_revision_replacement_exposes_only_current_context() {
        // Cause/effect table: C1 revision R1 owns prompt P1 -> attempt A1 sees
        // only P1; C2 the same slot atomically advances to R2/P2 -> fresh A2
        // sees only P2; C3 A1 already captured P1 -> it remains immutable but
        // cannot contaminate A2. Every activation input stays byte-identical.
        let slots = Arc::new(crate::installed_session::InstalledSessions::default());
        let executor = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
            awaken_time::system_clock(),
        );
        slots.realizations().update_realization("session-1", |r| {
            r.staging.resources.prompts = vec!["resource revision one".into()]
        });
        let durable = activation("genai");
        let mut first = awaken_runtime_contract::RuntimeRunContext::default();
        let projected = executor.project(durable.clone(), &mut first).unwrap();
        assert_eq!(projected.input, durable.input, "C1");
        assert_eq!(
            first.request_context[1].text_content(),
            "resource revision one",
            "C1"
        );

        slots.realizations().update_realization("session-1", |r| {
            r.staging.resources.prompts = vec!["resource revision two".into()]
        });
        let mut second = awaken_runtime_contract::RuntimeRunContext::default();
        let projected = executor.project(durable.clone(), &mut second).unwrap();
        assert_eq!(projected.input, durable.input, "C2");
        assert_eq!(
            second.request_context.len(),
            2,
            "C2 environment + current resource"
        );
        assert_eq!(
            second.request_context[1].text_content(),
            "resource revision two",
            "C2"
        );
        assert_eq!(
            first.request_context[1].text_content(),
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
        let slots = Arc::new(crate::installed_session::InstalledSessions::default());
        slots.realizations().update_realization("session-1", |r| {
            r.staging.resources.prompts = vec!["r1-a".into(), "r1-b".into()]
        });
        let executor = Arc::new(SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
            awaken_time::system_clock(),
        ));
        let release = Arc::new(std::sync::Barrier::new(2));
        let (writer_locked_tx, writer_locked_rx) = std::sync::mpsc::channel();
        let writer_slots = slots.clone();
        let writer_release = release.clone();
        let writer = std::thread::spawn(move || {
            writer_slots.update("session-1", |slot| {
                writer_locked_tx.send(()).unwrap();
                writer_release.wait();
                slot.realization.staging.resources.prompts = vec!["r2-a".into(), "r2-b".into()];
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
                .filter(|message| message.id.0.starts_with("session-context:"))
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
        let slots = Arc::new(crate::installed_session::InstalledSessions::default());
        let executor = SessionContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            slots.clone(),
            "session-1",
            awaken_time::system_clock(),
        );
        let publication = activation("genai");
        let retained = publication.snapshot.clone();
        let flow_policy =
            |enabled| awaken_session_configuration_contract::SessionToolConfiguration {
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
        slots.replace_frozen_with("session-1", |next| next.tools = Some(flow_policy(true)));

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

        slots.replace_frozen_with("session-1", |next| next.tools = Some(flow_policy(false)));
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
            Arc::new(crate::installed_session::InstalledSessions::default()),
            "session-1",
            awaken_time::system_clock(),
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
