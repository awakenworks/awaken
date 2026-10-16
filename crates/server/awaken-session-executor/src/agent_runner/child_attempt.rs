// @formal entrypoint delegation.independent_child_scheduling_lease
//! Execution-edge materialization for a child attempt.
//!
//! A delegated child selects its execution edge only through its immutable
//! publication. This module owns the host-installed adapters behind that
//! selection: the Native plugin composition, the ACP/remote attempt router, and
//! the credential realization bound to a directly executed child.

use awaken_credential_contract as credential;
use std::sync::Arc;

use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use super::AgentRunError;

/// One pre-effect check shared by direct creation and claimed recovery.
pub(crate) fn validate_child_tool_configuration(
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
    inherited: Option<&awaken_runtime_contract::activation::InheritedToolsets>,
) -> Result<(), AgentRunError> {
    if inherited.is_none()
        && snapshot.resolved_spec.plugin_ids.iter().any(|id| {
            id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID
                || id == awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID
        })
    {
        return Err(AgentRunError::Configuration(
            "delegated Web execution requires frozen caller toolsets".into(),
        ));
    }
    Ok(())
}

/// Install the selected configured Web plugins into a child Native Runtime.
/// Direct and recovered children call this same owner so replacement dispatch
/// cannot silently lose the immutable publication's tool selection or policy.
pub(crate) fn configure_child_native_runtime(
    mut runtime: awaken_runtime::Runtime,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
    inherited: Option<&awaken_runtime_contract::activation::InheritedToolsets>,
    adapters: &ChildExecutionAdapters,
) -> Result<awaken_runtime::Runtime, AgentRunError> {
    validate_child_tool_configuration(snapshot, inherited)?;
    let selected = |plugin_id: &str| {
        snapshot
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == plugin_id)
    };
    if selected(awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID) {
        let plugin = adapters.web_search.as_ref().ok_or_else(|| {
            AgentRunError::Configuration(
                "a child publication selects WebSearch but the Host has no WebSearch adapter"
                    .to_string(),
            )
        })?;
        runtime = runtime.with_plugin(Arc::new(
            (**plugin)
                .clone()
                .with_inherited_execution_configuration(
                    awaken_ext_builtin_tools::web_search_execution_configuration(
                        &snapshot.resolved_spec.plugin_config.agent.toolsets,
                    )
                    .map_err(AgentRunError::Configuration)?,
                    inherited.expect("selected WebSearch requires frozen callers"),
                )
                .map_err(AgentRunError::Configuration)?,
        ));
    }
    if selected(awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID) {
        let plugin = adapters.web_fetch.as_ref().ok_or_else(|| {
            AgentRunError::Configuration(
                "a child publication selects WebFetch but the Host has no WebFetch adapter"
                    .to_string(),
            )
        })?;
        runtime = runtime.with_plugin(Arc::new(
            (**plugin)
                .clone()
                .with_tool_source_catalogue(&snapshot.resolved_spec.tool_descriptors)
                .with_inherited_execution_configuration(
                    awaken_ext_builtin_tools::web_fetch_execution_configuration(
                        &snapshot.resolved_spec.plugin_config.agent.toolsets,
                    )
                    .map_err(AgentRunError::Configuration)?,
                    inherited.expect("selected WebFetch requires frozen callers"),
                )
                .map_err(AgentRunError::Configuration)?,
        ));
    }
    Ok(runtime)
}

/// Execution-edge adapters already installed by the host process startup.
///
/// A delegated child selects only through its immutable `backend_ref`; this
/// value carries implementations and credential-realization evidence, never a
/// second target directory or backend-selection rule.
#[derive(Clone, Default)]
pub(crate) struct ChildExecutionAdapters {
    pub(crate) acp: Option<ChildAcpExecutorFactory>,
    pub(crate) remote: Option<Arc<dyn RunAttemptExecutor>>,
    pub(crate) credential_capabilities: credential::CredentialRealizationCapabilities,
    /// The same host-configured plugin instance shape used by a root Native Run.
    /// The child snapshot still decides whether the plugin is selected.
    pub(crate) web_search: Option<Arc<awaken_ext_builtin_tools::WebSearchPlugin>>,
    /// The same host-configured plugin instance shape used by a root Native Run.
    /// The child snapshot still decides whether the plugin is selected.
    pub(crate) web_fetch: Option<Arc<awaken_ext_builtin_tools::WebFetchPlugin>>,
    /// The parent Host's process clock, shared by Native approval/resume and
    /// directly delegated remote credential bindings. Native child composition
    /// requires this existing clock before any Fresh provider effect; absence
    /// is a typed configuration refusal, never a process-clock fallback.
    pub(crate) clock: Option<Arc<dyn awaken_time::WallClock>>,
    /// The Host-owned slot registry also installed on the root Native Runtime.
    pub(crate) background_supervisor:
        Option<Arc<awaken_ext_background_task::BackgroundTaskSupervisor>>,
}

/// Materializes the already-selected ACP execution edge for a child snapshot.
/// Selection stays in the immutable `backend_ref`; the factory only binds that
/// exact backend to the Session-owned sandbox and permission context.
pub(crate) type ChildAcpExecutorFactory = Arc<
    dyn Fn(
            Backend,
            Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
        ) -> Result<Arc<dyn RunAttemptExecutor>, String>
        + Send
        + Sync,
>;

struct ChildCredentialRecorder {
    ownership: Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
    bindings: Vec<credential::AttemptCredentialBinding>,
}

#[async_trait::async_trait]
impl credential::CredentialRealizationRecorder for ChildCredentialRecorder {
    async fn record(
        &self,
        receipt: credential::CredentialRealizationReceipt,
    ) -> Result<(), credential::CredentialRealizationRecordError> {
        self.ownership
            .verify_current()
            .await
            .map_err(|error| credential::CredentialRealizationRecordError(error.to_string()))?;
        credential::verify_credential_realization_receipt(&self.bindings, &receipt)
            .map_err(|error| credential::CredentialRealizationRecordError(error.to_string()))
    }
}

pub(crate) fn child_attempt_executor(
    runtime: Arc<awaken_runtime::Runtime>,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
    adapters: &ChildExecutionAdapters,
    permission: Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
) -> Result<Arc<dyn RunAttemptExecutor>, AgentRunError> {
    let backends = snapshot
        .resolved_spec
        .attempt_candidates(None)
        .into_iter()
        .map(|candidate| Backend::from_ref(candidate.binding().identity().2))
        .collect::<Vec<_>>();
    let selected_web = snapshot.resolved_spec.plugin_ids.iter().any(|id| {
        id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID
            || id == awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID
    });
    let acp = backends
        .iter()
        .find(|backend| matches!(backend, Backend::Acp(_)))
        .cloned()
        .map(|backend| {
            if selected_web {
                return Err(AgentRunError::Configuration(
                    "an ACP child publication selects a Web plugin, but child ACP tool export is unavailable"
                        .to_string(),
                ));
            }
            adapters.acp.as_ref().ok_or_else(|| {
                AgentRunError::Configuration(format!(
                    "delegate publication requires unavailable ACP backend {backend:?}"
                ))
            })?(backend, permission.clone())
            .map_err(AgentRunError::Configuration)
        })
        .transpose()?;
    let remote = if backends
        .iter()
        .any(|backend| matches!(backend, Backend::Remote(_)))
    {
        if selected_web {
            return Err(AgentRunError::Configuration(
                "a remote child publication selects Web tools whose inherited policy cannot be enforced by the remote executor".into(),
            ));
        }
        Some(adapters.remote.clone().ok_or_else(|| {
            AgentRunError::Configuration(
                "delegate publication requires an unavailable remote backend".to_string(),
            )
        })?)
    } else {
        None
    };
    Ok(Arc::new(crate::run_exec::session_attempt_executor(
        runtime,
        acp,
        remote,
        &[&snapshot.resolved_spec],
    )))
}

pub(super) fn bind_direct_child_credentials(
    activation: &RunActivation,
    mut context: RuntimeRunContext,
    adapters: &ChildExecutionAdapters,
) -> Result<RuntimeRunContext, AgentRunError> {
    let candidates = activation
        .snapshot
        .resolved_spec
        .attempt_candidates(activation.model_ref_override.as_deref())
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.provisioning(),
                awaken_runtime_contract::resolved::ModelProvisioning::Remote { .. }
            )
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(context);
    }
    let ownership = context.ownership.clone().ok_or_else(|| {
        AgentRunError::Configuration(
            "direct delegated remote execution requires parent attempt ownership".to_string(),
        )
    })?;
    let epoch = LOCAL_CHILD_ATTEMPT_EPOCH
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .max(1);
    let clock = adapters.clock.as_ref().ok_or_else(|| {
        AgentRunError::Configuration(
            "direct delegated remote execution requires the parent Host clock".to_string(),
        )
    })?;
    let now_unix_ms = awaken_time::WallClockExt::now_ms(clock.as_ref());
    let holder = awaken_dispatch_execution::placement::self_hosted_inference_holder(activation)
        .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let bindings = credential::compile_candidate_credential_bindings(
        &candidates,
        holder.as_ref(),
        &adapters.credential_capabilities,
        epoch,
        now_unix_ms,
    )
    .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    if !bindings.is_empty() {
        context =
            context.with_credential_realization(credential::AttemptCredentialRealization::new(
                bindings.clone(),
                Arc::new(ChildCredentialRecorder {
                    ownership,
                    bindings,
                }),
            ));
    }
    Ok(context)
}

static LOCAL_CHILD_ATTEMPT_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::run::{EndCause, RunState};
    use awaken_runtime::RunInput;
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::execution::RunAttemptExecutor;
    use awaken_runtime_contract::resolved::Backend;
    use awaken_runtime_contract::resume::ResumeCommand;
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;

    use super::{ChildExecutionAdapters, child_attempt_executor};
    use crate::agent_runner::fixtures::{agent, user};
    use crate::config::effective_tool_authorization;

    struct AcpChildRecorder {
        executions: Arc<AtomicUsize>,
    }

    #[tokio::test]
    async fn direct_and_rebuilt_child_web_use_the_same_frozen_sources_before_network_io() {
        // Causes: N1 caller-only allow; N2 denied target; N3 contradictory
        // caller lists; N4 no inherited evidence; N5 an unseen same-host URL;
        // N6 activation crosses serde with its configured URL-source policy.
        // Effects: one real HTTP request for N1; no request for N2/N3; N4 is
        // rejected before execution; N5 refuses before I/O; N6 reconstructs the same policy through
        // the owner also used by recovered_child_worker. Loopback is an internal
        // transport fixture; public hostname admission has separate tests.
        // | Rule | Input | Materialization | Effect |
        // | N1 | allow loopback | direct | HTTP once, normal result |
        // | N2 | allow loopback, target localhost | serde rebuilt | error, no I/O |
        // | N3 | allow + block loopback | serde rebuilt | error, no I/O |
        // | N4 | missing sources | selected Web | pre-effect refusal |
        // | N5 | allowed host, URL absent from user input | serde rebuilt | no I/O |
        use awaken_agent_contract::agent::message::Role;
        use awaken_runtime_contract::activation::InheritedToolsets;
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
        };
        use awaken_runtime_contract::execution::RunExecutor as _;
        use awaken_runtime_contract::llm::{
            AssistantOutput, ChatRequest, ChatResponse, LlmExecutor,
        };
        use awaken_runtime_contract::permission::ToolCall;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        struct FetchModel {
            url: String,
        }
        #[async_trait::async_trait]
        impl LlmExecutor for FetchModel {
            async fn infer(
                &self,
                request: ChatRequest,
            ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
                let output = if request
                    .messages
                    .iter()
                    .any(|message| message.role == Role::Tool)
                {
                    AssistantOutput::text("finished")
                } else {
                    AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "inherited-http".into(),
                        tool_id: "web_fetch".into(),
                        arguments: serde_json::json!({"url":self.url}),
                    }])
                };
                Ok(ChatResponse {
                    output,
                    usage: None,
                    stop_reason: None,
                })
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(awaken_runtime_contract::CancellationToken::new());
        let server = {
            let requests = requests.clone();
            let stop = stop.clone();
            crate::agent_runner::AbortOnDropTask::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        accepted = listener.accept() => {
                            let (mut stream, _) = accepted.unwrap();
                            let mut bytes = [0; 2048];
                            let received = stream.read(&mut bytes).await.unwrap();
                            assert!(received > 0, "web request must send bytes");
                            requests.fetch_add(1, Ordering::SeqCst);
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").await.unwrap();
                        }
                    }
                }
            })
        };
        let source = |kind: &str| {
            vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::with_optional_configuration(
                    "web_fetch",
                    ToolExecutionPolicy::default(),
                    Some(
                        serde_json::json!({"type":"web_fetch","domains":{"type":kind,"domains":["127.0.0.1"]}, "url_sources":{"client_tool_results":"none","server_tool_results":"none"}}),
                    ),
                )],
            }]
        };
        let allowed = InheritedToolsets::with_caller(source("allow"), None);
        let blocked = InheritedToolsets::with_caller(source("block"), Some(&allowed));
        let mut snapshot = agent("web-child", "use web_fetch");
        snapshot.resolved_spec.plugin_ids =
            vec![awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.into()];
        snapshot.resolved_spec.plugin_config.insert(
            awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.into(),
            awaken_ext_builtin_tools::WebFetchPlugin::default_config(),
        );
        snapshot.recompute_fingerprint().unwrap();
        let adapters = ChildExecutionAdapters {
            web_fetch: Some(Arc::new(awaken_ext_builtin_tools::WebFetchPlugin::new(
                awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                None,
            ))),
            ..Default::default()
        };
        assert!(
            super::validate_child_tool_configuration(&snapshot, None).is_err(),
            "N4 missing evidence"
        );
        for (rule, host, sources, rebuilt, expected_requests) in [
            ("N1", "127.0.0.1", allowed.clone(), false, 1),
            ("N2", "localhost", allowed.clone(), true, 1),
            ("N3", "127.0.0.1", blocked, true, 1),
            ("N5", "127.0.0.1", allowed, true, 1),
        ] {
            let url = format!("http://{host}:{port}/");
            let activation = RunActivation::new(
                awaken_agent_contract::agent::run::Id(format!("http-{rule}")),
                awaken_agent_contract::agent::thread::Id(format!("http-{rule}")),
                snapshot.clone(),
                vec![user(&if rule == "N5" {
                    format!("fetch {url}unseen")
                } else {
                    format!("fetch {url}")
                })],
            )
            .with_inherited_toolsets(Some(sources));
            let activation: RunActivation = if rebuilt {
                serde_json::from_slice(&serde_json::to_vec(&activation).unwrap()).unwrap()
            } else {
                activation
            };
            let runtime = super::configure_child_native_runtime(
                awaken_runtime::Runtime::new().with_llm(Arc::new(FetchModel { url })),
                &activation.snapshot,
                activation.inherited_toolsets(),
                &adapters,
            )
            .unwrap();
            let commit = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            ));
            let outcome = runtime
                .execute(
                    activation,
                    RuntimeRunContext::new().with_commit(commit.clone()),
                )
                .await
                .unwrap();
            assert_eq!(
                outcome,
                RunState::Ended(EndCause::NaturalEnd),
                "{rule} model completed after tool result"
            );
            assert_eq!(
                requests.load(Ordering::SeqCst),
                expected_requests,
                "{rule} actual network count"
            );
            let transcript = commit.committed().messages;
            assert!(
                transcript.iter().any(|message| message.role == Role::Tool
                    && message.text_content().contains(if rule == "N1" {
                        "hello"
                    } else {
                        "url_not_allowed"
                    })),
                "{rule} semantic tool result"
            );
        }
        stop.cancel();
        server.join().await.unwrap();
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
        // | A5 | remote + Web plugin | any | E6 before remote dispatch |
        // FMECA: capturing the coordinator's policy in the ACP factory lets a
        // parent silently broaden or narrow its referenced Agent, unlike the
        // Managed Agents version-pinned thread model.
        let mut acp_snapshot = agent("acp-child", "child");
        let (provider, model, _) = acp_snapshot.resolved_spec.model_binding.identity();
        let binding =
            awaken_runtime_contract::resolved::ModelBinding::parse(provider, model, "acp:claude")
                .unwrap();
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
                .await
                .verdict,
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
        let mut remote_web_snapshot = native_snapshot;
        remote_web_snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::parse(
                    "remote",
                    "",
                    "a2a:https://agent.example",
                )
                .expect("admitted remote test binding"),
            );
        remote_web_snapshot
            .resolved_spec
            .plugin_ids
            .push(awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.into());
        let remote_error = match child_attempt_executor(
            Arc::new(awaken_runtime::Runtime::new()),
            &remote_web_snapshot,
            &adapters,
            effective_tool_authorization(
                &remote_web_snapshot.resolved_spec.plugin_config,
                &[],
                &remote_web_snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .toolsets,
            )
            .policy,
        ) {
            Ok(_) => panic!("A5 remote child Web policy must fail closed"),
            Err(error) => error,
        };
        assert!(
            remote_error
                .to_string()
                .contains("remote child publication selects Web"),
            "A5/E6: {remote_error}"
        );
        assert_eq!(
            materializations.load(Ordering::SeqCst),
            1,
            "A5/E6 no external executor materialization"
        );
    }
}
