// @formal evidence background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
//! Runtime plugins, frozen configuration, attempt context, and ingress assembly.
//!
//! No Session Runtime cache is published from this stage. Export leases are
//! owned by the completed context, or dropped if construction fails/cancels.

use super::{PreparedContext, execution, extensions, resources::ContextResources};
use crate::SessionExecutionError;
use crate::config::{build_runtime_with_authorization, server_config};
use crate::host::session::{SessionToolsetProjection, project_session_tool_override};
use crate::host::session_ctx::RuntimeContextPurpose;
use crate::host::{RuntimePublicationIdentity, SessionCtx};
use std::collections::HashSet;
use std::sync::Arc;

pub(super) async fn assemble(
    host: &crate::RunAssembly,
    thread: &str,
    mut resources: ContextResources,
    requested_purpose: RuntimeContextPurpose,
) -> Result<PreparedContext, SessionExecutionError> {
    let env = &resources.env;
    let authorization = &resources.authorization;
    let has_published_multiagent = resources.has_published_multiagent;
    let has_published_advisor = resources.has_published_advisor;
    let managed_coordination = resources.managed_coordination;
    let coordination_endpoint = &resources.coordination_endpoint;
    let commit = &resources.commit;
    let installed = &resources.plan.installed;
    // This marker is set by the Session application for its canonical root
    // thread. Child workers supply their separate original Session coordinate.
    let file_reader_session_id = resources.plan.session_dispatch.then(|| thread.to_owned());
    let publication_source = &resources.plan.publication_source;
    let toolsets = &resources.toolsets;
    // R1/R2: the runtime is built with the host default executor; each run then
    // resolves its *effective* model (its `model_ref_override`, else its snapshot
    // binding) to an executor at the resolve seam and sets it on the run context.
    // Resolving per Run — not once at Session build — means a per-Run model
    // switch needs no session rebuild, and a database-less worker runs the
    // configured model without a session-level registry.
    let tool_source: &dyn crate::config::RuntimeToolSource = match env.as_ref() {
        Some(environment) => environment.as_ref(),
        None => &crate::config::DeferredHandToolSource,
    };
    let mut runtime = build_runtime_with_authorization(
        host.capabilities.agent.llm.clone(),
        host.clock.clone(),
        tool_source,
        authorization,
        Some(host.capabilities.background_supervisor.clone()),
        &host.capabilities.tools.admin_tools,
    );
    // Delegation is a runtime concern: inject the executor so the kernel runs
    // `agent_run` as a sub-agent (native or remote), not the tool registry.
    if has_published_multiagent
        && !managed_coordination
        && env.is_none()
        && host.runs_local_dispatch_pool()
    {
        return Err(SessionExecutionError::internal(
            "remote-only execution cannot host local delegate targets",
        ));
    }
    if managed_coordination {
        let coordinator = coordination_endpoint
            .clone()
            .map(|endpoint| Arc::new(crate::coordination::HostAgentCoordinator::new(endpoint)));
        if let Some(coordinator) = coordinator {
            for tool in awaken_ext_builtin_tools::coordination_tools(coordinator) {
                runtime = runtime.with_tool(tool);
            }
        }
    }
    // One service assembly: Managed admission narrows this to the published
    // Advisor. Installing it cannot restore a removed AgentDelegation descriptor.
    if (!managed_coordination || has_published_advisor)
        && let Some(env) = env.clone()
        && let Some(service) = host.child_runs.delegation_service(
            thread,
            env,
            commit.clone(),
            installed.as_ref(),
            publication_source.clone(),
            crate::delegate::CallerToolConfiguration {
                toolsets: toolsets.clone(),
                inherited: None,
            },
        )?
    {
        runtime = runtime.with_run_delegation(service);
    }
    let extensions::SkillWiring {
        runtime,
        skill_descriptors,
        semantic_skill_tools,
        skill_registry,
        mut session_content_plugins,
        generated_skill_bindings,
    } = extensions::skills(host, thread, &mut resources, runtime).await?;
    let is_acp = resources.plan.execution_backend.is_acp();
    let (mut runtime, semantic_memory_tools) = extensions::memory_tools(
        host,
        thread,
        &resources,
        runtime,
        &mut session_content_plugins,
    )?;
    let memory_descriptors = semantic_memory_tools
        .as_ref()
        .map(|tools| tools.descriptors.clone())
        .unwrap_or_default();
    // This is the sole final gate replacement point. Skill wiring may decorate
    // the ordinary base gate above; an explicit host override intentionally
    // replaces that complete default chain (the scheduled-action scenario).
    if let Some(gate) = &host.capabilities.tools.gate_override {
        runtime = runtime.with_gate(gate.clone());
    }
    let extensions::MemoryWiring {
        mut runtime,
        plugin_ids,
        acp_memory_recall,
        context_policy,
    } = extensions::memory(host, thread, &resources, runtime).await?;
    let installed = &resources.plan.installed;
    let selected_agent = &resources.plan.selected_agent;
    let toolsets = &resources.toolsets;
    let session_tools = &resources.session_tools;
    let background_tasks_enabled = resources.plan.background_tasks_enabled;
    // Authored/default Session configuration still advertises Skill tools.
    // MCP tools are live Session plugins and therefore do not rewrite either
    // this generated snapshot or an immutable published snapshot.
    // A direct embedded Session may author its generated snapshot locally.
    // A published snapshot is immutable, so its Resource-derived Skill
    // descriptors enter through `session_skill_plugin` instead.
    let dynamic_descriptors = if installed.is_some() {
        Vec::new()
    } else {
        skill_descriptors
            .into_iter()
            .chain(memory_descriptors)
            .collect()
    };
    let session_delegates: HashSet<String> = installed
        .as_ref()
        .map(|snapshot| {
            snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegate_ids()
                .map(|id| id.0.clone())
                .collect()
        })
        .unwrap_or_default();
    // The durable Session configuration is a complete execution overlay,
    // applied to this per-Session clone, never to the retained immutable Agent
    // publication. A publication takes the one activation projection the
    // Coordinator also admits Runs with; a generated snapshot embeds the
    // Session toolsets and its generated Skill bindings.
    let config = match installed.clone() {
        Some(installed) => awaken_session_run_delivery::session_activation_snapshot(
            installed,
            session_tools.as_ref(),
            resources.plan.session_dispatch,
        )?,
        None => {
            let model_ref = host
                .capabilities
                .agent
                .inference_routing
                .model_ref(thread, &host.capabilities.agent.model_ref)?;
            let mut config = server_config(
                selected_agent,
                &model_ref,
                &host.capabilities.tools.client_tools,
                &session_delegates,
                &plugin_ids,
                &host.capabilities.tools.plugin_config,
                &dynamic_descriptors,
                context_policy,
            )
            .map_err(|error| {
                SessionExecutionError::bad_request(format!(
                    "host default model reference `{model_ref}` for Agent \
                     `{selected_agent}` is not an executable binding: {error}"
                ))
            })?;
            let mut config_changed = false;
            if let Some(skills) = generated_skill_bindings {
                config.resolved_spec.plugin_config.agent.skills = skills;
                config_changed = true;
            }
            if let Some(session_tools) = &session_tools {
                config_changed |= project_session_tool_override(
                    &mut config,
                    session_tools,
                    SessionToolsetProjection::ProjectIntoSnapshot,
                );
            }
            if config_changed {
                config.recompute_fingerprint().map_err(|error| {
                    SessionExecutionError::internal(format!(
                        "fingerprint Session tool projection: {error}"
                    ))
                })?;
            }
            config
        }
    };
    debug_assert_eq!(
        background_tasks_enabled,
        config
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID),
        "Session projections must not rewrite BackgroundTask activation",
    );
    let (web_runtime, web_search, web_fetch) = host.capabilities.configure_web_tools_for_session(
        thread,
        &config.resolved_spec,
        toolsets,
        is_acp,
        runtime,
    )?;
    runtime = web_runtime;
    let execution::ExecutionWiring {
        runtime,
        acp_executor,
        acp_tool_exports,
        acp_provider_server_tools,
    } = execution::prepare(
        host,
        &resources,
        requested_purpose,
        &config,
        runtime,
        execution::AcpTools {
            skills: semantic_skill_tools.as_ref(),
            memory: semantic_memory_tools.as_ref(),
            search: web_search,
            fetch: web_fetch,
        },
    )
    .await?;
    let ContextResources {
        plan,
        env,
        thread_id,
        commit,
        stream_checkpoint,
        active_mcp,
        mcp,
        coordination_endpoint,
        content_delivery,
        ..
    } = resources;
    let super::plan::ContextPlan {
        fallback_model_ref,
        publication_source,
        session_dispatch,
        background_tasks_enabled,
        background_attention,
        a2a_only,
        frozen_runtime_identity,
        unclaimed_runtime_publications,
        ..
    } = plan;
    let mut attempt_specs = vec![&config.resolved_spec];
    attempt_specs.extend(
        host.capabilities
            .agent
            .judge_snapshot
            .as_ref()
            .map(|snapshot| &snapshot.resolved_spec),
    );
    let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
        Arc::new(crate::run_exec::session_attempt_executor(
            runtime.clone(),
            acp_executor,
            host.capabilities.agent.remote_attempt_executor.clone(),
            &attempt_specs,
        ));
    let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
        Arc::new(
            crate::application::attempt_context::AcpContextAttemptExecutor::new(
                attempt_executor,
                acp_memory_recall,
                acp_provider_server_tools,
            ),
        );
    let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
        Arc::new(
            crate::application::attempt_context::SessionContextAttemptExecutor::new(
                attempt_executor,
                &host.installed_sessions,
                thread,
                host.clock.clone(),
            ),
        );
    let attempt_executor = host
        .capabilities
        .exec
        .attempt_decorator
        .as_ref()
        .map_or(attempt_executor.clone(), |decorate| {
            decorate(attempt_executor)
        });
    // Privacy attribution is an attempt concern, not a delivery-topology
    // concern. This one decorator therefore wraps the final executor used by
    // both direct and claimed durable attempts (including recovered runs).
    let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
        Arc::new(crate::run_exec::CaptureContextAttemptExecutor::new(
            attempt_executor,
            &host.capture,
            host.capabilities.thread_workspace(thread)?,
        ));
    // This is the one post-attempt output edge. Because it wraps the final
    // executor shared by direct delivery and the durable Worker, claimed
    // recovery cannot bypass artifact publication.
    let attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor> =
        Arc::new(crate::run_exec::ArtifactHarvestAttemptExecutor::new(
            attempt_executor,
            host.realization_executor.effects().artifacts(),
        ));
    // The foreground delivery seam (slice C/D): a Run's execution goes through
    // the selected delivery path rather than calling the Runtime loop directly. Direct
    // ingress runs inline on the same `runtime`; durable ingress queues the run
    // through a dispatch store first. Both share this thread's `runtime`/`commit`.
    // A database-less Worker sends its terminal commit to the Coordinator.
    // The authenticated dispatch-settlement observer owns remote post-commit
    // Memory observation, after Coordinator commit and before settlement, so
    // the durable intent lands beside authoritative Session truth. Local
    // execution observes here because its commit authority is in this Host.
    let mut terminal_observers: Vec<_> = if !host
        .authority
        .committed_truth()
        .observes_terminal_memory_locally()
    {
        Vec::new()
    } else {
        host.resources
            .memory_terminal_observers
            .for_published_run(
                thread,
                &config,
                &fallback_model_ref,
                None,
                publication_source.as_ref(),
                commit.clone(),
            )
            .await?
            .into_iter()
            .collect()
    };
    let tool_executor = if a2a_only {
        None
    } else if content_delivery == crate::sandbox_layout::ManagedContentDelivery::SemanticTools {
        let mut tools = Vec::new();
        if let Some((_, skill_tools)) = semantic_skill_tools.as_ref() {
            tools.extend(skill_tools.iter().cloned());
        }
        if let Some(memory) = semantic_memory_tools.as_ref() {
            tools.extend(memory.executors.iter().cloned());
        }
        Some(Arc::new(
            crate::config::FilesystemFreeAgentToolExecutor::try_new(tools)
                .map_err(SessionExecutionError::internal)?,
        )
            as Arc<dyn awaken_tool_contract::tool::ToolExecutor>)
    } else if let Some(environment) = env.as_ref() {
        // Every realized tier owns the Hand for its Session. Container uses
        // the channel-backed process; Workdir/Namespace use their rooted
        // implementations behind the same ToolExecutor port.
        Some(environment.agent_tools().tool_executor())
    } else {
        host.installed_sessions
            .read_run(thread, |run| run.deferred_executor.clone())?
            .flatten()
    };
    // Cause/effect rules: terminal observers and Session plugins
    // are additive; an Environment/placement hand overrides only the tool
    // executor; one canonical RuntimeRunContext crosses the ingress boundary.
    let workspace_id = host.capabilities.thread_workspace(thread)?;
    let (environment_generation, resource_origin) = match env.as_ref() {
        Some(environment) => host
            .realization_executor
            .environments()
            .resident_environment_activity_identity(thread, environment)?,
        None => ("brain".to_string(), None),
    };
    let dispatch_claim = host
        .installed_sessions
        .read_run(thread, |run| run.dispatch_claim.clone())?
        .flatten();
    let mut run_context = awaken_runtime_contract::RuntimeRunContext::new()
        .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
            awaken_agent_contract::ScopeId::from(workspace_id.as_str()),
        ))
        .with_tool_execution_admission(
            host.resources
                .memory
                .background()
                .tool_execution_admission(thread, &environment_generation)?,
        )
        .with_model_content_materializer(Arc::new(
            crate::model_content_materializer::ResourceModelContentMaterializer::new(
                host.resources.file_content_source.clone(),
                workspace_id,
                thread,
                dispatch_claim,
            )
            .with_reader_session(file_reader_session_id),
        ));
    run_context.resource_origin = resource_origin;
    run_context.request_context = host
        .installed_sessions
        .read(thread, |installed| installed.inputs.request_context.clone())?
        .unwrap_or_default();
    let run_context = mcp
        .plugins
        .iter()
        .cloned()
        .chain(session_content_plugins)
        .fold(
            run_context,
            awaken_runtime_contract::RuntimeRunContext::with_session_plugin,
        );
    let run_context = match (session_dispatch, coordination_endpoint.clone()) {
        (true, Some(endpoint)) => run_context.with_model_request_gate(Arc::new(
            crate::coordination::HostModelRequestGate::new(endpoint, thread),
        )),
        (false, _) => run_context,
        (true, None) => unreachable!("Session application authority was checked above"),
    };
    let run_context = match tool_executor.as_ref() {
        Some(executor) => run_context.with_tool_executor(executor.clone()),
        None => run_context,
    };
    let run_context = match env.as_ref() {
        Some(environment) => run_context.with_tool_output_spiller(Arc::new(
            crate::tool_output_spill::SandboxToolOutputSpiller::new(environment.clone()),
        )),
        None => run_context,
    };
    let claimed_terminal_reads = awaken_dispatch_execution::ClaimedTerminalReads::default();
    if background_tasks_enabled {
        let background_commit = crate::background_task::TaskCommittedReads::of(
            &commit,
            &claimed_terminal_reads,
            host.capabilities.worker.task_control.as_ref(),
            thread,
        )
        .map_err(SessionExecutionError::internal)?;
        terminal_observers.push(Arc::new(
            crate::background_task::BackgroundTaskTerminalObserver::new(
                runtime.clone(),
                config.clone(),
                run_context.clone(),
                background_commit,
                host.resources.memory.background(),
                host.capabilities.background_supervisor.clone(),
                thread.to_string(),
                environment_generation,
                background_attention.clone(),
            ),
        ));
    }
    let run_context = terminal_observers.iter().cloned().fold(
        run_context,
        awaken_runtime_contract::RuntimeRunContext::with_terminal_observer,
    );
    let claimed_model_gate = run_context.model_request_gate.as_ref().and_then(|_| {
        coordination_endpoint.clone().map(|endpoint| {
            crate::coordination::claimed_model_request_gate_factory(endpoint, thread)
        })
    });
    let (delivery, claimed_worker) = host
        .build_dispatch_worker(
            claimed_model_gate,
            runtime.clone(),
            attempt_executor,
            commit.clone(),
            stream_checkpoint.clone(),
            run_context.clone(),
            claimed_terminal_reads,
        )
        .await?;
    // No per-session dispatch daemon: the process-level `DispatchPool` (spawned
    // once by `mount`) is the sole claimer of the shared queue and drives this
    // session's runs by routing claimed work back to its worker (O2).
    let runtime_publication_identity = frozen_runtime_identity.as_ref().cloned().or_else(|| {
        unclaimed_runtime_publications
            .as_deref()
            .map(|publications| {
                RuntimePublicationIdentity::from_publications(
                    &config,
                    publications,
                    fallback_model_ref.as_str(),
                )
            })
    });
    let ctx = Arc::new(SessionCtx {
        runtime,
        delivery,
        claimed_worker,
        runtime_publication_identity,
        purpose: requested_purpose,
        config,
        commit,
        attempt_context: run_context,
        terminal_observers,
        stream_checkpoint,
        _acp_tool_exports: acp_tool_exports,
        thread_id,
        env,
        skill_registry,
        foreground: Arc::default(),
        projection: tokio::sync::Mutex::new(()),
        outcome: tokio::sync::Mutex::new(()),
        command: tokio::sync::Mutex::new(()),
    });
    Ok(PreparedContext { ctx, active_mcp })
}
