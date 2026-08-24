//! Constructing and running an Agent through the ordinary Run lifecycle.
//!
//! Native delegation (`agent_run`) and skill forks use this
//! substrate. A delegated Agent receives a first-class child Run identity, the
//! same durable context and capabilities as a directly initiated Agent, and the
//! same delegation executor, so nested delegation is not a special execution
//! path. Auxiliary Agents may deliberately request transient identity and an
//! isolated context because their work is outside the user-visible Run tree.
//!
//! [`root_execution::run_configured_agent`] is the parameterized form: it resolves the Agent's
//! own `ExecutableAgentSnapshot` (instructions, model, tools) from an
//! [`AgentCatalog`](crate::agent_catalog::AgentCatalog) by id. [`run_agent`] is the thin wrapper that runs the default
//! `assistant` config with a plain string prompt.

#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use awaken_agent_contract::agent::delegation::DelegationOrigin;
#[cfg(test)]
use awaken_agent_contract::agent::run::Record as RunRecord;
#[cfg(test)]
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
#[cfg(test)]
use awaken_agent_contract::agent::thread::Id as ThreadId;
#[cfg(test)]
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
#[cfg(test)]
use awaken_run_ingress::AnyDispatchStore;
#[cfg(test)]
use awaken_runtime::RunInput;
#[cfg(test)]
use awaken_runtime_contract::CancellationToken;
#[cfg(test)]
use awaken_runtime_contract::activation::RunActivation;
#[cfg(test)]
use awaken_runtime_contract::execution::RunAttemptExecutor;
#[cfg(test)]
use awaken_runtime_contract::llm::LlmExecutor;
#[cfg(test)]
use awaken_runtime_contract::resolved::Backend;
#[cfg(test)]
use awaken_runtime_contract::resume::ResumeResult;
#[cfg(test)]
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
#[cfg(test)]
use awaken_runtime_contract::tool::{RawToolRegistry, ToolExecutor};
#[cfg(test)]
use awaken_sandbox_local::LocalProvider;

#[cfg(test)]
use crate::agent_catalog::AgentCatalog;
#[cfg(test)]
use crate::config::{effective_tool_authorization, server_config};

mod child_execution;
mod root_execution;
pub(crate) use child_execution::{
    AgentRunBoundary, ChildAcpExecutorFactory, ChildExecutionAdapters, ChildRunRequest,
    RunScheduler, child_attempt_executor, child_dispatch_request, configure_child_native_runtime,
    run_configured_agent_until_boundary,
};
#[cfg(test)]
use child_execution::{
    await_committed_child_boundary, isolated_child_recovery_projection, settled_agent_boundary,
};
use root_execution::usage_from_committed;
pub(crate) use root_execution::{
    AgentExecution, AgentRunError, AgentRunSandbox, run_agent, run_configured_agent_with_id,
};
#[cfg(test)]
use root_execution::{run_agent_until_boundary, run_configured_agent};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_agent_run_uses_its_frozen_toolset_authorization() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 a frozen Agent publication has an Agent
        // toolset; C2 web_search is enabled+always_allow while web_fetch is
        // enabled+always_ask. E1 search executes without HITL; E2 fetch awaits;
        // C3 no authored policy/toolset retains the strict runtime baseline.
        //
        // Decision table:
        // | C1 policy                         | effective gate    |
        // | web_search enabled/always_allow  | allow (R1/E1)     |
        // | web_fetch enabled/always_ask     | confirm (R2/E2)   |
        // | no explicit policy               | no override (R3)  |
        // FMECA: a root/child/recovery-specific compiler can strand an otherwise
        // autonomous Run at Awaiting or let an initiator silently broaden another
        // Agent. The same effective authorization compiler owns every Run shape.
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };
        use awaken_runtime_contract::permission::{GateOutcome, ToolCall};

        let mut child = agent("research-child", "research");
        child.resolved_spec.plugin_config.agent.toolsets = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: Default::default(),
            overrides: vec![
                ToolPolicyOverride::new(
                    "web_search",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                ),
                ToolPolicyOverride::new(
                    "web_fetch",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                ),
            ],
        }];
        let authorization = effective_tool_authorization(
            &child.resolved_spec.plugin_config,
            &[],
            &child.resolved_spec.plugin_config.agent.toolsets,
        );
        let gate = authorization.gate;
        let state = awaken_agent_contract::agent::state::Store::default();
        let call = |tool_id: &str| ToolCall {
            tool_id: tool_id.into(),
            call_id: format!("call-{tool_id}"),
            arguments: serde_json::json!({}),
        };
        assert_eq!(
            gate.gate(&call("web_search"), &state).await,
            GateOutcome::Allow,
            "R1/E1"
        );
        assert!(
            matches!(
                gate.gate(&call("web_fetch"), &state).await,
                GateOutcome::RequireConfirmation { .. }
            ),
            "R2/E2"
        );
        let plain = agent("plain-child", "plain");
        let baseline = effective_tool_authorization(
            &plain.resolved_spec.plugin_config,
            &[],
            &plain.resolved_spec.plugin_config.agent.toolsets,
        );
        assert!(
            matches!(
                baseline.gate.gate(&call("web_search"), &state).await,
                GateOutcome::RequireConfirmation { .. }
            ),
            "R3"
        );
    }

    #[test]
    fn remote_child_never_reuses_the_parent_recovery_projection() {
        // Cause/effect graph and decision table:
        // C1 a local authoritative scheduler has no recovery projection -> E1
        // the child adds none; C2 a database-independent Worker has a parent
        // projection -> E2 the child gets a distinct empty projection.
        // R1=!C2=>E1; R2=C2=>E2. FMECA: sharing C2 lets the child overwrite the
        // running parent's cached claim snapshot and deterministically fences
        // the next parent or child commit as the wrong Run.
        assert!(isolated_child_recovery_projection(None).is_none(), "R1/E1");
        let parent = Arc::new(awaken_run_ingress::RecoveryProjection::new());
        let child = isolated_child_recovery_projection(Some(&parent)).expect("R2/E2");
        assert!(!Arc::ptr_eq(&parent, &child), "R2/E2");
        assert!(parent.current().is_none());
        assert!(child.current().is_none());
    }
    use awaken_store_inmem::MemoryCommitCoordinator;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::awaiting::ResumeTicket;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::EndCause;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult,
    };
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
    use awaken_runtime_contract::resume::ResumeCommand;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    #[test]
    fn child_terminal_cause_is_not_collapsed_to_success() {
        // Cause/effect graph: child boundary is Awaiting/NaturalEnd/MaxSteps/
        // Cancelled/Stopped/Error/Indeterminate. Effects are preserve Awaiting,
        // return successful child output, or surface an AgentRunError.
        // Decision table: R1 Awaiting -> awaiting; R2 NaturalEnd|MaxSteps ->
        // success; R3 every other EndCause -> error. This closes the former
        // `Ended(_)` wildcard that reclassified remote failures as empty success.
        let reader = MemoryCommitCoordinator::new();
        let thread = ThreadId("child-terminal".into());
        assert!(matches!(
            settled_agent_boundary(&reader, &thread, RunState::Awaiting),
            Ok(AgentRunBoundary::Awaiting)
        ));
        for cause in [EndCause::NaturalEnd, EndCause::MaxSteps] {
            assert!(matches!(
                settled_agent_boundary(&reader, &thread, RunState::Ended(cause)),
                Ok(AgentRunBoundary::Ended { .. })
            ));
        }
        for cause in [
            EndCause::Cancelled,
            EndCause::Stopped("budget".into()),
            EndCause::Indeterminate,
            EndCause::Error(awaken_agent_contract::agent::run::Failure::Inference {
                code: "a2a_task_failed".into(),
                message: "remote failed".into(),
            }),
        ] {
            assert!(
                settled_agent_boundary(&reader, &thread, RunState::Ended(cause)).is_err(),
                "R3"
            );
        }
    }

    fn published_model(
        model: &str,
        credential: &str,
        provider: &str,
        route: &str,
    ) -> ResolvedModelCandidate {
        ResolvedModelCandidate::try_provider(
            ModelBinding::new(provider, model, "native"),
            provider,
            route,
            "workspace-a",
            Some(awaken_runtime_contract::CredentialAccess::new(
                awaken_runtime_contract::CredentialRef {
                    id: credential.into(),
                    revision: 0,
                },
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
            )),
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "test".into(),
                api_dialect: "test".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: model.into(),
                processing_placement: None,
            },
        )
        .expect("coherent published test model")
    }

    fn memory_context() -> RuntimeRunContext {
        let commit = Arc::new(MemoryCommitCoordinator::new());
        RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit)
    }

    /// A model that replies with the leading system instruction it was given, so a
    /// test can prove the sub-run resolved that agent's own config.
    struct InstructionEchoModel;

    struct EventuallySettledReader {
        reads: AtomicUsize,
    }

    impl CommittedThreadView for EventuallySettledReader {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
            None
        }

        fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
            None
        }

        fn run_state(&self, _run_id: &RunId) -> Option<RunState> {
            if self.reads.fetch_add(1, Ordering::SeqCst) < 2 {
                Some(RunState::Running)
            } else {
                Some(RunState::Awaiting)
            }
        }
    }

    #[tokio::test]
    async fn losing_a_child_claim_waits_for_the_winners_committed_boundary() {
        let reader = EventuallySettledReader {
            reads: AtomicUsize::new(0),
        };
        let state = await_committed_child_boundary(&reader, &RunId("child-race".to_string()), None)
            .await
            .expect("the competing worker settles the child");

        assert_eq!(state, RunState::Awaiting);
        assert!(reader.reads.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn waiting_for_a_competing_child_claim_observes_parent_cancellation() {
        struct RunningReader;

        impl CommittedThreadView for RunningReader {
            fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
                Vec::new()
            }

            fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
                None
            }

            fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
                None
            }

            fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
                None
            }

            fn run_state(&self, _run_id: &RunId) -> Option<RunState> {
                Some(RunState::Running)
            }
        }

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = await_committed_child_boundary(
            &RunningReader,
            &RunId("child-cancelled".to_string()),
            Some(&cancellation),
        )
        .await
        .expect_err("a cancelled parent stops waiting for its child");

        assert!(error.to_string().contains("was cancelled"));
    }

    #[async_trait::async_trait]
    impl LlmExecutor for InstructionEchoModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let system = request
                .messages
                .iter()
                .find(|m| m.role == Role::System)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            Ok(ChatResponse {
                output: AssistantOutput::text(system),
                usage: None,
                stop_reason: None,
            })
        }
    }

    fn agent(id: &str, instructions: &str) -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot::builder(id)
            .instructions(instructions)
            .model(ModelBinding::new("default", "stub", "default"))
            .max_steps(4)
            .build()
    }

    fn user(text: &str) -> Message {
        Message {
            id: MessageId("u1".into()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    struct AcpChildRecorder {
        executions: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl awaken_runtime_contract::execution::RunExecutor for AcpChildRecorder {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> awaken_runtime_contract::execution::Result<RunState> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(RunState::Ended(EndCause::Stopped("acp-child".into())))
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for AcpChildRecorder {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> awaken_runtime_contract::execution::Result<RunState> {
            unreachable!("this test exercises a fresh delegated attempt")
        }

        async fn cancel(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> awaken_runtime_contract::execution::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn delegated_attempt_uses_the_canonical_backend_router_for_acp() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause graph: C1=the frozen child publication names ACP; C2=the Host
        // installed an ACP materializer. C1+C2 causes E1=materialize the exact
        // selected backend, E2=route the child attempt to ACP, and E3=bind the
        // child's own frozen permission policy. C1+!C2 causes E4=fail closed
        // before execution. !C1 causes E5=do not consult ACP. C1 plus a selected
        // Web plugin causes E6=fail closed until the child ACP boundary can
        // retain the canonical exported-tool lease; it never exports a broken
        // placeholder or silently drops the selected tool.
        //
        // | Rule | C1 ACP | C2 adapter | Effect |
        // | A1 | yes | yes | E1 + E2 + E3 |
        // | A2 | yes | no | E4 |
        // | A3 | no | either | E5 |
        // | A4 | yes + Web plugin | yes | E6 before adapter materialization |
        // FMECA: capturing the coordinator's policy in the ACP factory lets a
        // parent silently broaden or narrow its referenced Agent, unlike the
        // Managed Agents version-pinned thread model.
        let mut acp_snapshot = agent("acp-child", "child");
        let mut binding = acp_snapshot.resolved_spec.model_binding.binding().clone();
        binding.backend_ref = "acp:claude".to_string();
        acp_snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);
        acp_snapshot.resolved_spec.plugin_config.agent.toolsets = vec![
            awaken_runtime_contract::agent_bindings::ToolsetPolicy {
                source: awaken_runtime_contract::agent_bindings::ToolsetSource::Agent,
                default: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy {
                    enabled: true,
                    permission:
                        awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
                },
                overrides: vec![awaken_runtime_contract::agent_bindings::ToolPolicyOverride::new(
                    "bash",
                    awaken_runtime_contract::agent_bindings::ToolExecutionPolicy {
                        enabled: true,
                        permission:
                            awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAllow,
                    },
                )],
            },
        ];
        let materializations = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let received_permission = Arc::new(std::sync::Mutex::new(None));
        let adapters = ChildExecutionAdapters {
            acp: Some({
                let materializations = materializations.clone();
                let executions = executions.clone();
                let received_permission = received_permission.clone();
                Arc::new(move |backend, permission| {
                    assert_eq!(backend, Backend::from_ref("acp:claude"));
                    materializations.fetch_add(1, Ordering::SeqCst);
                    *received_permission.lock().unwrap() = Some(permission);
                    Ok(Arc::new(AcpChildRecorder {
                        executions: executions.clone(),
                    }) as Arc<dyn RunAttemptExecutor>)
                })
            }),
            ..Default::default()
        };
        let runtime = Arc::new(awaken_runtime::Runtime::new());
        let router = child_attempt_executor(
            runtime.clone(),
            &acp_snapshot,
            &adapters,
            effective_tool_authorization(
                &acp_snapshot.resolved_spec.plugin_config,
                &[],
                &acp_snapshot.resolved_spec.plugin_config.agent.toolsets,
            )
            .policy,
        )
        .expect("A1 installs the selected ACP adapter");
        let permission = received_permission
            .lock()
            .unwrap()
            .clone()
            .expect("A1/E3 passes the child policy to ACP");
        assert_eq!(
            permission
                .evaluate(&awaken_runtime_contract::permission::ToolCall {
                    tool_id: "bash".into(),
                    call_id: "child-bash".into(),
                    arguments: serde_json::json!({}),
                })
                .await,
            awaken_runtime_contract::permission::ToolPermissionVerdict::Allow,
            "A1/E3"
        );
        let (_, activation) = runtime.prepare(
            &acp_snapshot,
            "acp-child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );
        assert_eq!(
            router
                .execute(activation, RuntimeRunContext::new())
                .await
                .unwrap(),
            RunState::Ended(EndCause::Stopped("acp-child".into()))
        );
        assert_eq!(materializations.load(Ordering::SeqCst), 1, "A1/E1");
        assert_eq!(executions.load(Ordering::SeqCst), 1, "A1/E2");

        let error = match child_attempt_executor(
            Arc::new(awaken_runtime::Runtime::new()),
            &acp_snapshot,
            &ChildExecutionAdapters::default(),
            effective_tool_authorization(
                &acp_snapshot.resolved_spec.plugin_config,
                &[],
                &acp_snapshot.resolved_spec.plugin_config.agent.toolsets,
            )
            .policy,
        ) {
            Ok(_) => panic!("A2 missing ACP adapter must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("ACP backend"), "A2/E3");

        let native_snapshot = agent("native-child", "child");
        child_attempt_executor(
            runtime,
            &native_snapshot,
            &adapters,
            effective_tool_authorization(
                &native_snapshot.resolved_spec.plugin_config,
                &[],
                &native_snapshot.resolved_spec.plugin_config.agent.toolsets,
            )
            .policy,
        )
        .expect("A3 native remains available");
        assert_eq!(materializations.load(Ordering::SeqCst), 1, "A3/E4");

        let mut acp_web_snapshot = acp_snapshot;
        acp_web_snapshot
            .resolved_spec
            .plugin_ids
            .push(awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.into());
        let error = match child_attempt_executor(
            Arc::new(awaken_runtime::Runtime::new()),
            &acp_web_snapshot,
            &adapters,
            effective_tool_authorization(
                &acp_web_snapshot.resolved_spec.plugin_config,
                &[],
                &acp_web_snapshot.resolved_spec.plugin_config.agent.toolsets,
            )
            .policy,
        ) {
            Ok(_) => panic!("A4 ACP child Web export must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("ACP child"), "A4/E6: {error}");
        assert_eq!(materializations.load(Ordering::SeqCst), 1, "A4/E6");
    }

    #[test]
    fn child_dispatch_reuses_publication_pinned_model_candidates() {
        // Cause/effect graph and decision table:
        // C1 the child publication carries credentialed Native primary and
        // fallback candidates; C2 it inherits a frozen Session resource
        // manifest. E1 both exact candidates remain frozen; E2 the manifest is
        // carried in the parent's execution scope; E3 credential admission is
        // pinned to the canonical Worker plaintext holder.
        // R1=C1+C2 => E1+E2+E3 is covered here. The credential-free remote rule
        // is covered below. FMECA: omitting E3 lets the child reach the queue but
        // makes every credential-aware claim fail closed; the holder assertion
        // detects that parent/child dispatch-contract divergence.
        let mut config = agent("child", "child");
        let primary = published_model("primary", "cred-a", "provider-a@1", "route-a@1");
        let fallback = published_model("fallback", "cred-b", "provider-b@2", "route-b@3");
        config.resolved_spec.model_binding = primary.clone();
        config.resolved_spec.model_candidates = vec![fallback.clone()];
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );

        let manifest = awaken_session_contract::SessionResourceManifest::new(
            "workspace-a",
            awaken_session_contract::ResolvedSessionResources::try_new(
                Vec::new(),
                vec![awaken_session_contract::ResolvedSkillBinding {
                    kind: awaken_agent_contract::AgentSkillKind::Custom,
                    skill_id: "skill-a".into(),
                    version: 3,
                    bundle_sha256: "sha256:skill-a-v3".into(),
                }],
            )
            .unwrap(),
        );
        let request = child_dispatch_request(
            activation,
            ThreadId("parent-thread".to_string()),
            Some(manifest.clone()),
            None,
        )
        .expect("build child dispatch");

        assert_eq!(
            request.activation.snapshot.resolved_spec.model_binding,
            primary
        );
        assert_eq!(
            request.activation.snapshot.resolved_spec.model_candidates,
            vec![fallback]
        );
        assert_eq!(request.session_thread_id.unwrap().0, "parent-thread");
        let carried = request
            .session_resources
            .as_ref()
            .expect("resource envelope")
            .decode_manifest()
            .expect("decode resource envelope");
        assert_eq!(carried, manifest);
        assert!(
            request
                .placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY)
        );
        assert_eq!(
            request.inference_plaintext_holder,
            Some(
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
                    .inference_holder
            ),
            "R1/E3"
        );
    }

    #[test]
    fn child_dispatch_freezes_nested_and_auxiliary_targets_from_one_source() {
        // Cause/effect graph: C1 a child publication delegates to a nested Agent;
        // C2 the same child selects a custom Memory auxiliary; C3 both targets
        // exist/one is absent in the inherited attempt source. Effects: E1 one
        // child dispatch freezes both exact non-root targets; E2 an incomplete
        // source fails before queue admission. Rules P1=C1+C2+complete=>E1 and
        // P2=C1+C2+missing=>E2. No child execution consumer may reopen Control.
        use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};

        let nested = agent("nested-child", "nested");
        let auxiliary = awaken_ext_memory::memory_agent(
            "child-memory-extractor",
            nested.resolved_spec.model_binding.clone(),
            awaken_ext_memory::DEFAULT_MEMORY_INSTRUCTIONS,
        );
        let config = awaken_runtime_contract::ExecutableAgentSnapshot::builder("child-owner")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "agent_id": "child-memory-extractor",
                    "recall_enabled": false
                }),
            )])
            .agent_bindings(AgentBindings {
                delegates: vec![AgentDelegateBinding {
                    agent_id: nested.root_agent_id.clone(),
                    source_revision: None,
                    recursive_self: false,
                }],
                ..Default::default()
            })
            .model(ModelBinding::new("default", "stub", "native"))
            .build();
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "child-owner-thread".into(),
            RunInput::from(vec![user("go")]),
        );
        let complete = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([
            nested.clone(),
            auxiliary.clone(),
        ])
        .expect("P1 complete attempt source");
        let request = child_dispatch_request(
            activation.clone(),
            ThreadId("parent-thread".into()),
            None,
            Some(&complete),
        )
        .expect("P1 child admission");
        assert_eq!(
            request
                .agent_publications
                .iter()
                .map(|snapshot| snapshot.root_agent_id.0.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            [
                nested.root_agent_id.0.as_str(),
                auxiliary.root_agent_id.0.as_str()
            ]
            .into_iter()
            .collect(),
            "P1/E1"
        );

        let incomplete = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([nested])
            .expect("P2 incomplete attempt source shape is valid");
        assert!(
            child_dispatch_request(
                activation,
                ThreadId("parent-thread".into()),
                None,
                Some(&incomplete),
            )
            .is_err(),
            "P2/E2"
        );
    }

    #[test]
    fn child_dispatch_always_carries_backend_placement_without_resources() {
        // Cause/effect graph:
        // C1 the child publication pins a Remote backend; C2 the child has no
        // Session resources; C3 the child is remote-preferred rather than
        // remote-required. E1 admission still requires the A2A executor
        // capability; E2 no resource capability is invented; E3 the canonical
        // current dispatch/runtime protocol is frozen instead of legacy v0.
        //
        // Decision rule P1: C1+C2+C3 => E1+E2+E3. The resource-present/provider case
        // is rule P2 in `child_dispatch_reuses_publication_pinned_model_candidates`.
        // Together they prevent optional resource staging from controlling
        // backend or credential placement. FMECA: a newly-authored v0 child is
        // durably pending but incompatible with every current Worker; the
        // explicit version assertions detect that silent delegation stall.
        let mut config = agent("remote-child", "remote child");
        config.resolved_spec.model_binding = ResolvedModelCandidate::try_remote(
            ModelBinding::new("remote", "", "a2a:https://agent.example"),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "sha256:card",
        )
        .expect("coherent remote child candidate");
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "remote-child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );

        let request = child_dispatch_request(
            activation,
            ThreadId("parent-thread".to_string()),
            None,
            None,
        )
        .expect("build remote child dispatch");

        assert!(
            request
                .placement
                .required_capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY)
        );
        assert!(
            !request
                .placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY)
        );
        assert_eq!(request.placement.contract_version, 1, "P1/E3");
        assert_eq!(request.placement.dispatch_contract_version, 1, "P1/E3");
        assert_eq!(request.placement.runtime_protocol_version, 1, "P1/E3");
    }

    #[tokio::test]
    async fn resolves_agent_config_by_id() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new()
            .with_agent(agent("memory-extractor", "MEMORY INSTRUCTIONS"))
            .with_agent(agent("judge", "JUDGE INSTRUCTIONS"));
        let llm = Arc::new(InstructionEchoModel);

        let (mem, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "memory-extractor",
            "t-mem",
            vec![user("go")],
            Vec::new(),
            None,
            Some(memory_context()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(mem, "MEMORY INSTRUCTIONS");

        let (judge, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "judge",
            "t-judge",
            vec![user("go")],
            Vec::new(),
            None,
            Some(memory_context()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(judge, "JUDGE INSTRUCTIONS");
    }

    /// A model that reports fixed token usage per inference, so a test can prove the
    /// sub-run's usage is read back out of its isolated commit store (rather than
    /// discarded with it).
    struct UsageModel;

    #[async_trait::async_trait]
    impl LlmExecutor for UsageModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: Some(awaken_runtime_contract::llm::TokenUsage {
                    prompt_tokens: 13,
                    completion_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
                stop_reason: None,
            })
        }
    }

    struct CountingModel(AtomicUsize);

    #[async_trait::async_trait]
    impl LlmExecutor for CountingModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                output: AssistantOutput::text("stable"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    struct RejectedModel;

    #[async_trait::async_trait]
    impl LlmExecutor for RejectedModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Err(awaken_runtime_contract::llm::Error::Binding(
                "candidate is outside the frozen set".into(),
            ))
        }
    }

    #[tokio::test]
    async fn auxiliary_terminal_failure_is_not_projected_as_empty_success() {
        // Cause graph / decision table:
        // C1=terminal cause is NaturalEnd/MaxSteps; C2=terminal cause is a fault.
        // | Rule | C1 | C2 | result                                      |
        // | R1   | T  | F  | return reply/usage                          |
        // | R2   | F  | T  | return error; caller must retry/fail closed |
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let context = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit);

        let error = run_configured_agent_with_id(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(RejectedModel),
            "worker",
            "aux/rejected",
            RunId("aux/rejected/run".into()),
            vec![user("go")],
            Vec::new(),
            context,
        )
        .await
        .expect_err("R2: a terminal inference fault is not an empty successful result");

        assert!(error.to_string().contains("ended unsuccessfully"));
        assert!(error.to_string().contains("binding_rejected"));
    }

    #[tokio::test]
    async fn stable_auxiliary_run_reuses_committed_terminal_truth_without_reinference() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));
        let model = Arc::new(CountingModel(AtomicUsize::new(0)));
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let context = || {
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone())
        };
        let run_id = RunId("aux/stable/run".into());

        for input in ["first", "ignored retry input"] {
            let (reply, _) = run_configured_agent_with_id(
                &catalog,
                AgentRunSandbox::Fresh(&provider),
                model.clone(),
                "worker",
                "aux/stable",
                run_id.clone(),
                vec![user(input)],
                Vec::new(),
                context(),
            )
            .await
            .unwrap();
            assert_eq!(reply, "stable");
        }

        assert_eq!(model.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            commit.run_state(&run_id),
            Some(RunState::Ended(_))
        ));
        assert_eq!(
            commit
                .committed_messages(&ThreadId("aux/stable".into()))
                .iter()
                .filter(|message| message.role == Role::Assistant)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn agent_run_returns_its_accumulated_usage() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-usage-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));

        let (_text, usage) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(UsageModel),
            "worker",
            "t-usage",
            vec![user("go")],
            Vec::new(),
            None,
            Some(memory_context()),
            None,
        )
        .await
        .unwrap();

        // The sub-run's model reported 13/5 on its one step; that must survive the
        // committed Agent Run state as the returned tally.
        let total = usage.total();
        assert_eq!(total.prompt_tokens, 13);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(
            usage
                .by_model
                .get("stub")
                .copied()
                .unwrap_or_default()
                .prompt_tokens,
            13
        );
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let base = std::env::temp_dir().join("awaken-subrun-test-unknown");
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let err = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(InstructionEchoModel),
            "nope",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown agent"), "got: {err}");
    }

    #[tokio::test]
    async fn known_agent_without_commit_history_authority_fails_closed() {
        // Cause/effect graph:
        // C1 agent resolves; C2 commit authority supplied; C3 history reader supplied.
        // E1 execute on caller authority; E2 configuration error; E3 no volatile store.
        // Decision table: R1 C1/T,C2/T,C3/T -> E1; R2 C1/T,C2/F,C3/F -> E2+E3.
        // This case owns R2; ordinary successful Agent tests above own R1.
        let provider = LocalProvider::new(std::env::temp_dir().join("awaken-no-implicit-store"));
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let error = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(InstructionEchoModel),
            "assistant",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .expect_err("missing commit/history wiring must fail closed");
        assert!(
            error
                .to_string()
                .contains("explicitly owned commit/history context"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn child_runs_bind_the_current_sandbox_and_selected_web_plugin() {
        // Causes: C1 execution enters the boundary or auxiliary child path; C2
        // placement is Shared, SharedLocal, or Fresh; C3 the child WebFetch
        // configuration is absent, valid, malformed, or incompatible with a
        // provider-server realization; C4 the parent context carries an executor
        // for a different sandbox; C5 the frozen child selects the WebFetch
        // plugin; C6 the Host supplies its configured WebFetch adapter. Effects:
        // E1 a successful child writes only through
        // its current placement executor; E2 the parent's executor is not
        // inherited; E3 selected malformed child configuration
        // fails before model/tool effects; E4 a selected WebFetch plugin resolves
        // from the Host adapter; E5 a missing selected adapter fails before
        // inference; E6 an incompatible provider-server policy fails through the
        // same configured plugin before inference.
        // Constraints: K1 the current sandbox is the sole child Hand authority;
        // K2 WebFetch policy belongs to the configured plugin, never the Hand;
        // K3 direct and recovered Native children use one plugin composition owner.
        //
        // | Rule | Child path | Placement | Fetch selected/adapter | Effect |
        // | R1 | boundary | Shared | no/either | E1+E2 current Shared executor |
        // | R2 | auxiliary | SharedLocal | no/either, inert policy | E1+E2 current executor |
        // | R3 | auxiliary | Fresh | no/either | E1+E2 fresh executor/root |
        // | R4 | boundary | SharedLocal | yes/yes, malformed policy | E3 fail closed |
        // | R5 | boundary | Shared | yes/yes | E1+E4 configured plugin |
        // | R6 | boundary | Shared | yes/no | E5 fail closed |
        // | R7 | boundary | Shared | provider-server/restrictive policy | E6 capability-bound before inference |
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };

        struct SandboxWriteModel {
            file_name: String,
            inferences: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl LlmExecutor for SandboxWriteModel {
            async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
                self.inferences.fetch_add(1, Ordering::SeqCst);
                let output = if request
                    .messages
                    .iter()
                    .any(|message| message.role == Role::Tool)
                {
                    AssistantOutput::text("child done")
                } else {
                    AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                        call_id: format!("write-{}", self.file_name),
                        tool_id: "write".into(),
                        arguments: serde_json::json!({
                            "file_path": self.file_name,
                            "content": "current sandbox"
                        }),
                    }])
                };
                Ok(ChatResponse {
                    output,
                    usage: None,
                    stop_reason: None,
                })
            }
        }

        fn child_config(configuration: Option<serde_json::Value>) -> ExecutableAgentSnapshot {
            let allow = ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAllow,
            };
            let mut overrides = vec![ToolPolicyOverride::new("write", allow)];
            if let Some(configuration) = configuration {
                overrides.push(ToolPolicyOverride::with_optional_configuration(
                    "web_fetch",
                    allow,
                    Some(configuration),
                ));
            }
            let selected_web_fetch = overrides.iter().any(|item| item.name == "web_fetch");
            let plugin_ids = if selected_web_fetch {
                vec![awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string()]
            } else {
                Vec::new()
            };
            let plugin_config = if selected_web_fetch {
                std::collections::BTreeMap::from([(
                    awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string(),
                    awaken_ext_builtin_tools::WebFetchPlugin::default_config(),
                )])
            } else {
                std::collections::BTreeMap::new()
            };
            let mut config = server_config(
                "assistant",
                "stub",
                &HashSet::new(),
                &HashSet::new(),
                &plugin_ids,
                &plugin_config,
                &[],
                awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
            );
            config.resolved_spec.plugin_config.agent.toolsets = vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides,
            }];
            config
                .recompute_fingerprint()
                .expect("test child configuration remains coherent");
            config
        }

        fn select_web_fetch(mut config: ExecutableAgentSnapshot) -> ExecutableAgentSnapshot {
            config
                .resolved_spec
                .plugin_ids
                .push(awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string());
            config
                .recompute_fingerprint()
                .expect("selected child WebFetch configuration remains coherent");
            config
        }

        let tmp = tempfile::tempdir().unwrap();
        let parent = LocalProvider::new(tmp.path().join("parent"))
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("policy-owner"))
            .await
            .unwrap();
        let parent_path = parent.workspace_path().to_path_buf();
        let parent_executor: Arc<dyn ToolExecutor> =
            Arc::new(RawToolRegistry::new(parent.rooted_tools()));
        let context = || {
            let commit = Arc::new(MemoryCommitCoordinator::new());
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit)
                .with_tool_executor(parent_executor.clone())
        };
        let model = |file_name: &str, inferences: Arc<AtomicUsize>| {
            Arc::new(SandboxWriteModel {
                file_name: file_name.to_string(),
                inferences,
            }) as Arc<dyn LlmExecutor>
        };
        let web_adapters = || ChildExecutionAdapters {
            web_fetch: Some(Arc::new(awaken_ext_builtin_tools::WebFetchPlugin::new(
                awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                None,
            ))),
            ..Default::default()
        };

        let shared_sandbox = LocalProvider::new(tmp.path().join("shared"))
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("current"))
            .await
            .unwrap();
        let shared_path = shared_sandbox.workspace_path().to_path_buf();
        let shared_environment =
            crate::session_environment::SessionEnvironment::workdir(shared_sandbox);
        let shared_inferences = Arc::new(AtomicUsize::new(0));
        let shared_config = child_config(None);
        let shared = run_configured_agent_until_boundary(
            &shared_config,
            AgentRunSandbox::Shared(&shared_environment),
            model("shared.txt", shared_inferences.clone()),
            RunId("shared-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-shared".into()),
                "call-shared",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters::default(),
        )
        .await
        .expect("R1 child reaches a terminal boundary");
        assert!(matches!(shared, AgentRunBoundary::Ended { .. }), "R1/E1");
        assert!(shared_path.join("shared.txt").is_file(), "R1/E1");
        assert_eq!(shared_inferences.load(Ordering::SeqCst), 2, "R1/E1");

        let shared_local = LocalProvider::new(tmp.path().join("shared-local"))
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("current"))
            .await
            .unwrap();
        let shared_local_path = shared_local.workspace_path().to_path_buf();
        let valid = serde_json::json!({"type":"web_fetch", "max_content_tokens":3});
        let shared_local_config = child_config(Some(valid));
        let shared_local_inferences = Arc::new(AtomicUsize::new(0));
        run_configured_agent_until_boundary(
            &shared_local_config,
            AgentRunSandbox::SharedLocal(&shared_local),
            model("shared-local.txt", shared_local_inferences.clone()),
            RunId("shared-local-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-shared-local".into()),
                "call-shared-local",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            web_adapters(),
        )
        .await
        .expect("R2 child uses its configured plugin and current Hand executor");
        assert!(
            shared_local_path.join("shared-local.txt").is_file(),
            "R2/E1"
        );
        assert_eq!(shared_local_inferences.load(Ordering::SeqCst), 2, "R2/E1");

        let fresh_base = tmp.path().join("fresh");
        let fresh_provider = LocalProvider::new(&fresh_base);
        let plain_catalog = AgentCatalog::new().with_agent(child_config(None));
        let fresh_inferences = Arc::new(AtomicUsize::new(0));
        run_configured_agent(
            &plain_catalog,
            AgentRunSandbox::Fresh(&fresh_provider),
            model("fresh.txt", fresh_inferences.clone()),
            "assistant",
            "fresh-current",
            vec![user("write")],
            Vec::new(),
            None,
            Some(context()),
            None,
        )
        .await
        .expect("R3 child creates and uses its fresh executor");
        assert!(
            fresh_base.join("fresh-current/fresh.txt").is_file(),
            "R3/E1"
        );
        assert_eq!(fresh_inferences.load(Ordering::SeqCst), 2, "R3/E1");

        let selected_config = select_web_fetch(child_config(None));
        let selected_inferences = Arc::new(AtomicUsize::new(0));
        let selected = run_configured_agent_until_boundary(
            &selected_config,
            AgentRunSandbox::Shared(&shared_environment),
            model("selected-web-fetch.txt", selected_inferences.clone()),
            RunId("selected-web-fetch-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-selected-web-fetch".into()),
                "call-selected-web-fetch",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters {
                web_fetch: Some(Arc::new(awaken_ext_builtin_tools::WebFetchPlugin::new(
                    awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                    None,
                ))),
                ..Default::default()
            },
        )
        .await
        .expect("R5 selected WebFetch resolves through the Host adapter");
        assert!(matches!(selected, AgentRunBoundary::Ended { .. }), "R5/E4");
        assert!(
            shared_path.join("selected-web-fetch.txt").is_file(),
            "R5/E1"
        );
        assert_eq!(selected_inferences.load(Ordering::SeqCst), 2, "R5/E4");

        let missing_inferences = Arc::new(AtomicUsize::new(0));
        let missing = match run_configured_agent_until_boundary(
            &selected_config,
            AgentRunSandbox::Shared(&shared_environment),
            model("missing-web-fetch.txt", missing_inferences.clone()),
            RunId("missing-web-fetch-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-missing-web-fetch".into()),
                "call-missing-web-fetch",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters::default(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("R6 selected WebFetch without its adapter must fail closed"),
        };
        assert!(
            missing.to_string().contains("no WebFetch adapter"),
            "R6/E5: {missing}"
        );
        assert_eq!(missing_inferences.load(Ordering::SeqCst), 0, "R6/E5");
        assert!(!shared_path.join("missing-web-fetch.txt").exists(), "R6/E5");

        let mut provider_server_config = select_web_fetch(child_config(Some(
            serde_json::json!({"type": "web_fetch", "max_content_tokens": 3}),
        )));
        provider_server_config.resolved_spec.plugin_config.insert(
            awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string(),
            serde_json::json!({"provider_id": "openrouter", "options": {}}),
        );
        provider_server_config
            .recompute_fingerprint()
            .expect("provider-server child configuration remains coherent");
        let provider_server_inferences = Arc::new(AtomicUsize::new(0));
        let provider_server = match run_configured_agent_until_boundary(
            &provider_server_config,
            AgentRunSandbox::Shared(&shared_environment),
            model(
                "provider-server-web-fetch.txt",
                provider_server_inferences.clone(),
            ),
            RunId("provider-server-web-fetch-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-provider-server-web-fetch".into()),
                "call-provider-server-web-fetch",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters {
                web_fetch: Some(Arc::new(awaken_ext_builtin_tools::WebFetchPlugin::new(
                    awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                    None,
                ))),
                ..Default::default()
            },
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("R7 provider-server child policy must fail closed"),
        };
        assert!(
            provider_server
                .to_string()
                .contains("Error(CapabilityBound)"),
            "R7/E6: {provider_server}"
        );
        assert_eq!(
            provider_server_inferences.load(Ordering::SeqCst),
            0,
            "R7/E6"
        );
        assert!(
            !shared_path.join("provider-server-web-fetch.txt").exists(),
            "R7/E6"
        );

        let malformed_sandbox = LocalProvider::new(tmp.path().join("malformed"))
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("current"))
            .await
            .unwrap();
        let malformed_path = malformed_sandbox.workspace_path().to_path_buf();
        let malformed_inferences = Arc::new(AtomicUsize::new(0));
        let malformed_config = select_web_fetch(child_config(Some(serde_json::json!({
            "type": "web_fetch",
            "unexpected": true
        }))));
        let error = match run_configured_agent_until_boundary(
            &malformed_config,
            AgentRunSandbox::SharedLocal(&malformed_sandbox),
            model("malformed.txt", malformed_inferences.clone()),
            RunId("malformed-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-malformed".into()),
                "call-malformed",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            web_adapters(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("R4 malformed child configuration must fail closed"),
        };
        assert!(
            error.to_string().contains("invalid web_fetch"),
            "R4/E3: {error}"
        );
        assert_eq!(malformed_inferences.load(Ordering::SeqCst), 0, "R4/E3");
        assert!(!malformed_path.join("malformed.txt").exists(), "R4/E3");

        for file in [
            "shared.txt",
            "shared-local.txt",
            "fresh.txt",
            "selected-web-fetch.txt",
            "missing-web-fetch.txt",
            "provider-server-web-fetch.txt",
            "malformed.txt",
        ] {
            assert!(
                !parent_path.join(file).exists(),
                "E2 parent leakage: {file}"
            );
        }
    }

    /// A delegated child reaches the same typed HITL boundary as a root Run. The
    /// parent-facing coordinator resumes it with a `ResumeResult`; no child-only
    /// continuation language or automatic permission bypass exists.
    #[tokio::test]
    async fn durable_child_permission_recovers_and_duplicate_resume_is_idempotent() {
        // Causes: C1 a local durable child has a Session-wide committed reader;
        // C2 its inherited parent reader contains no child state; C3 the child is
        // new, Awaiting, resumed, or terminal on replay. Effects: E1 every child
        // state/ticket comes from C1; E2 duplicate start reconnects without a
        // second seed; E3 exact resume ends once; E4 terminal replay is a stutter.
        // Constraints: K1 HostCommit is the sole local committed authority; K2 a
        // parent claim projection cannot own a child Thread; K3 the queue/claim
        // remains the sole execution fence; K4 no second read model is introduced.
        // Decision table: R1=C1+C2+new=>Awaiting; R2=R1+duplicate=>E2;
        // R3=R1+resume=>E3; R4=E3+replay=>E4.
        struct PermissionModel;

        #[async_trait::async_trait]
        impl LlmExecutor for PermissionModel {
            async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
                let output = if request.messages.iter().any(|m| m.role == Role::Tool) {
                    AssistantOutput::text("child done")
                } else {
                    AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                        call_id: "child-write".into(),
                        tool_id: "write".into(),
                        arguments: serde_json::json!({
                            "path": "child.txt",
                            "content": "from child"
                        }),
                    }])
                };
                Ok(ChatResponse {
                    output,
                    usage: None,
                    stop_reason: None,
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let sandbox = provider
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("parent"))
            .await
            .unwrap();
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let dispatch_path = tmp.path().join("child-dispatch.db");
        let dispatch_path = dispatch_path.to_string_lossy().to_string();
        let scheduler = RunScheduler {
            store: Arc::new(
                AnyDispatchStore::open_sqlite(&dispatch_path).expect("child dispatch store"),
            ),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "replacement-worker".to_string(),
            claimed_commit: None,
            recovery_projection: None,
            session_resources: None,
            publication_source: None,
        };
        let parent_reader: Arc<dyn CommittedThreadView> = Arc::new(MemoryCommitCoordinator::new());
        let context = || {
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(parent_reader.clone())
        };
        let child_run_id = RunId("child-run-1".into());
        let origin = DelegationOrigin::root_for_agent(
            RunId("parent-run-1".into()),
            "delegate-call-1",
            "parent-agent",
        );
        let no_delegates = HashSet::new();
        let execution = |scheduler: RunScheduler| AgentExecution {
            agent_id: "worker",
            model_ref: "default",
            delegates: &no_delegates,
            run_delegation: None,
            context: Some(context()),
            scheduler: Some(scheduler),
        };

        let first = run_agent_until_boundary(
            Arc::new(PermissionModel),
            execution(scheduler),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: child_run_id.clone(),
                origin: origin.clone(),
                seed: Some(vec![user("write the file")].into()),
                resume: None,
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(first, AgentRunBoundary::Awaiting));
        let ticket = commit.resume_ticket(&child_run_id).expect("child ticket");
        assert_eq!(
            ticket.reason(),
            awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission
        );
        assert_eq!(ticket.call_id(), Some("child-write"));
        assert_eq!(ticket.delegation_origin.as_ref(), Some(&origin));

        // A replacement process reopens the durable queue and rebuilds every live
        // runtime object from committed truth; no child handle crosses this seam.
        let replacement = RunScheduler {
            store: Arc::new(
                AnyDispatchStore::open_sqlite(&dispatch_path).expect("reopen child dispatch"),
            ),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "replacement-worker-2".to_string(),
            claimed_commit: None,
            recovery_projection: None,
            session_resources: None,
            publication_source: None,
        };

        let recovered_boundary = run_agent_until_boundary(
            Arc::new(PermissionModel),
            execution(replacement.clone()),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: child_run_id.clone(),
                origin: origin.clone(),
                seed: Some(vec![user("write the file")].into()),
                resume: None,
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .expect("duplicate start reconnects to the awaiting child");
        assert!(matches!(recovered_boundary, AgentRunBoundary::Awaiting));
        assert_eq!(
            commit
                .committed()
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            1,
            "recovery start does not duplicate the child seed"
        );

        let second = run_agent_until_boundary(
            Arc::new(PermissionModel),
            execution(replacement),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: child_run_id.clone(),
                origin: origin.clone(),
                seed: None,
                resume: Some(ResumeResult::allow()),
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            second,
            AgentRunBoundary::Ended { ref text, .. } if text == "child done"
        ));

        // Simulate a replacement process that did not observe the returned child
        // result before crashing. The child is already terminal and its ticket is
        // gone; replaying the same resume must return committed truth without
        // entering the model or appending a duplicate assistant message.
        let replay = run_agent_until_boundary(
            Arc::new(PermissionModel),
            execution(RunScheduler {
                store: Arc::new(
                    AnyDispatchStore::open_sqlite(&dispatch_path)
                        .expect("reopen terminal child dispatch"),
                ),
                commit: commit.clone(),
                reader: commit.clone(),
                owner: "replacement-worker-3".to_string(),
                claimed_commit: None,
                recovery_projection: None,
                session_resources: None,
                publication_source: None,
            }),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: RunId("child-run-1".into()),
                origin: DelegationOrigin::root_for_agent(
                    RunId("parent-run-1".into()),
                    "delegate-call-1",
                    "parent-agent",
                ),
                seed: None,
                resume: Some(ResumeResult::allow()),
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .expect("terminal child replay is idempotent");
        assert!(matches!(
            replay,
            AgentRunBoundary::Ended { ref text, .. } if text == "child done"
        ));
        assert_eq!(
            commit
                .committed()
                .messages
                .iter()
                .filter(|message| message.text_content() == "child done")
                .count(),
            1,
            "replacement resume does not duplicate the child result"
        );
    }
}
