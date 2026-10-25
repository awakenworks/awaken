// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
// @formal evidence session_run.reservation_never_executes
//! Acquire the lifecycle-owned Environment and authoritative read/commit ports.

use super::plan::ContextPlan;
use crate::SessionExecutionError;
use crate::config::effective_tool_authorization;
use crate::host::session::SkillDeliveryEvidence;
use crate::host::session::input_projection::pre_authorized_tool_ids;
use crate::host::session_ctx::{
    RuntimeContextPurpose, RuntimeEnvironmentAdmission, runtime_environment_admission,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use std::sync::Arc;

pub(super) struct ContextResources {
    pub(super) plan: ContextPlan,
    pub(super) env: Option<Arc<crate::session_environment::SessionEnvironment>>,
    pub(super) thread_id: awaken_agent_contract::agent::thread::Id,
    pub(super) commit: Arc<awaken_session_run_delivery::SessionCommit>,
    pub(super) stream_checkpoint:
        Option<Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>>,
    pub(super) active_mcp: Vec<awaken_session_mcp::McpGenerationProjection>,
    pub(super) mcp: awaken_session_mcp::McpWiring,
    pub(super) has_published_multiagent: bool,
    pub(super) has_published_advisor: bool,
    pub(super) managed_coordination: bool,
    pub(super) coordination_endpoint: Option<crate::coordination::CoordinationEndpoint>,
    pub(super) content_delivery: crate::sandbox_layout::ManagedContentDelivery,
    pub(super) session_tools:
        Option<awaken_session_configuration_contract::SessionToolConfiguration>,
    pub(super) toolsets: Vec<awaken_agent_contract::ToolsetPolicy>,
    pub(super) authored_skills_subdir: String,
    pub(super) skill_source_roots: Option<Vec<String>>,
    pub(super) authorization: crate::config::EffectiveToolAuthorization,
}

pub(super) async fn acquire(
    host: &crate::RunAssembly,
    lifecycle: crate::session_realizations::LifecycleHeld<'_>,
    thread: &str,
    plan: ContextPlan,
    requested_purpose: RuntimeContextPurpose,
    force_defer_environment: bool,
    recover_revoked_legacy: bool,
) -> Result<ContextResources, SessionExecutionError> {
    let installed = &plan.installed;
    let selected_agent = &plan.selected_agent;
    let frozen_skill_versions = &plan.frozen_skill_versions;
    let deferred_skill_bindings = &plan.deferred_skill_bindings;
    let execution_backend = &plan.execution_backend;
    let session_dispatch = plan.session_dispatch;
    let a2a_only = plan.a2a_only;
    let provisioning = plan
        .model_candidate
        .as_ref()
        .map(awaken_runtime_contract::resolved::ResolvedModelCandidate::provisioning)
        .unwrap_or(&awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor);
    // A legacy/direct Environment has no encoded durable binding to feed
    // through ordinary adoption. Claimed recovery is owned by the Worker
    // projection synchronizer; only an explicit resume may consume the
    // quiesced owner here, after frozen publication and local-placement
    // validation. Ordinary and remote-only contexts remain fail-closed.
    let realization = &host.realization_executor;
    let exec = realization.environments();
    if recover_revoked_legacy && !a2a_only {
        let provider = exec.session_environment_provider_for_thread(thread, provisioning)?;
        exec.rebuild_claimed_legacy_environment_after_revocation(
            realization.peers(),
            thread,
            provider,
        )
        .await?;
    }
    let retained = realization
        .sessions()
        .read_realization(thread, |r| r.environment().owner.resident())?
        .flatten();
    let environment_owner_is_vacant = exec.session_environment_owner_is_vacant(thread)?;
    let environment_owner_has_unpublished_candidate = realization
        .sessions()
        .read_realization(thread, |r| {
            r.environment().owner.unpublished_candidate().is_some()
        })?
        .unwrap_or(false);
    if a2a_only && !environment_owner_is_vacant {
        return Err(SessionExecutionError::bad_request(
            "remote A2A execution cannot bind a local Session Environment",
        ));
    }
    let expected_binding = exec.durable_environment_binding(thread)?;
    // Recovery cause/effect decision table: C1 durable binding exists; C2 a
    // matching lifecycle-owned environment is resident. R1 !C1 may create
    // the first environment; R2 C1+C2 reuses it; R3 C1+!C2 fails
    // closed. A failed recovery must never erase C1 by creating a substitute.
    if retained.is_none()
        && expected_binding.is_none()
        && !environment_owner_is_vacant
        && !environment_owner_has_unpublished_candidate
    {
        return Err(SessionExecutionError::internal(format!(
            "Session {thread} Environment transition must be recovered before context construction"
        )));
    }
    let has_published_delegates = installed.as_ref().is_some_and(|snapshot| {
        !snapshot
            .resolved_spec
            .plugin_config
            .agent
            .delegates
            .is_empty()
    });
    let has_published_advisor = installed
        .as_ref()
        .is_some_and(|snapshot| snapshot.resolved_spec.plugin_config.agent.advisor.is_some());
    let has_published_multiagent = has_published_delegates || has_published_advisor;
    // Managed Sessions expose Anthropic's fixed asynchronous coordination
    // tools. Ordinary/direct SDK sessions retain the existing synchronous
    // `agent_run` path. `session_dispatch` is the projection of the durable
    // Session admission fact; this is not a process-local feature mode.
    let managed_coordination = has_published_multiagent && session_dispatch;
    // `session_dispatch` is also the sole scope selector for per-request
    // budget admission. A Managed single-Agent Session needs that authority
    // even though it has no coordination tools, while direct AI-SDK/AG-UI
    // Threads must remain independent of the Managed application port.
    let coordination_endpoint = host.capabilities.coordination_endpoint();
    if session_dispatch && coordination_endpoint.is_none() {
        return Err(SessionExecutionError::internal(
            "Managed Session has no Session application authority",
        ));
    }
    let content_delivery = host.select_content_delivery(
        thread,
        installed.as_ref(),
        deferred_skill_bindings.as_deref().map_or_else(
            || SkillDeliveryEvidence::Materialized(frozen_skill_versions.as_deref()),
            SkillDeliveryEvidence::DeferredBindings,
        ),
    )?;
    let can_defer = host.can_defer_session_environment(
        thread,
        Some(selected_agent.as_str()),
        installed.as_ref(),
        has_published_multiagent,
        plan.claimed_runtime_identity.is_some(),
    )?;
    // Reservation cannot create a physical Environment before its immutable
    // dispatch is durable. It must still reject a deterministic substrate
    // mismatch before the Session activity opens; otherwise an impossible
    // mount remains `running` while the queue repeatedly relinquishes it.
    if force_defer_environment && host.runs_local_dispatch_pool() && !a2a_only && !can_defer {
        exec.validate_session_environment_substrate(realization.peers(), thread, provisioning)?;
    }
    // Reservation is a projection-producing boundary, never an
    // Environment consumer. A cold Coordinator may legitimately know
    // the durable Worker-owned binding without having adopted that
    // substrate in this process; force-defer must therefore outrank the
    // local adoption precondition. Ordinary execution retains the
    // stricter rule: policy deferral is allowed only before any binding
    // exists, and a bound-but-unadopted Environment fails closed below.
    let environment_admission = runtime_environment_admission(
        requested_purpose,
        retained.is_some(),
        a2a_only,
        expected_binding.is_some(),
        can_defer,
    );
    let env = match environment_admission {
        RuntimeEnvironmentAdmission::Omit => None,
        RuntimeEnvironmentAdmission::Reuse => {
            Some(retained.expect("Environment admission selected the retained owner"))
        }
        RuntimeEnvironmentAdmission::RejectMissingBinding => {
            return Err(SessionExecutionError::internal(format!(
                "Session {thread} durable Environment was not adopted before context construction"
            )));
        }
        RuntimeEnvironmentAdmission::Create => {
            let provider = exec.session_environment_provider_for_thread(thread, provisioning)?;
            Some(
                exec.create_reserved_session_environment_under_lifecycle(
                    lifecycle,
                    realization.peers(),
                    thread,
                    provider,
                )
                .await?,
            )
        }
    };
    let env = match env {
        Some(environment) => Some(
            exec.ensure_published_environment_reconciled_under_lifecycle(
                lifecycle,
                realization.peers(),
                thread,
                environment,
            )
            .await?,
        ),
        None => None,
    };
    let thread_id = ThreadId(thread.to_string());
    let commit = Arc::new(host.authority.open_commit(thread).await?);
    // Durable interrupted-stream checkpoints follow the commit's durability
    // (Phase 3): a mid-recovery crash resumes from the flushed partial.
    let stream_checkpoint = host.authority.stream_checkpoint(thread)?;
    // Runtime selection is known before MCP realization. Native execution
    // connects staged servers as in-process McpPlugins; ACP hands the same
    // typed server set to the CLI's own MCP client and must not open a second
    // competing host-side connection.
    let is_acp = execution_backend.is_acp();
    // This thread's staged MCP servers (ADR-0043 Phase 3), registered by the
    // Managed adapter's `prepare_session` before the first Run; the wire
    // startup (connect + discover, fail closed) lives in `crate::mcp`.
    // Read, not removed, so a retry re-attempts (and re-fails) the connect.
    let active_mcp = host.active_mcp_projections(thread)?;
    let mcp = if is_acp {
        awaken_session_mcp::McpWiring::empty()
    } else {
        let mut combined = awaken_session_mcp::McpWiring::empty();
        for projection in &active_mcp {
            let wiring = projection.native_wiring.as_ref().ok_or_else(|| {
                SessionExecutionError::internal(format!(
                    "native MCP generation {}:{} has no staged connection",
                    projection.request.generation.attachment_id().0,
                    projection.request.generation.generation().version()
                ))
            })?;
            combined.plugins.extend(wiring.plugins.clone());
            combined.tool_ids.extend(wiring.tool_ids.clone());
            combined
                .skill_registries
                .extend(wiring.skill_registries.clone());
        }
        combined
    };
    // An authored permission policy is the sole authority for MCP confirmation.
    // Without one, selecting the MCP server pre-authorizes its discovered tools;
    // with one, its rules/default decide every MCP call. Do not project a second
    // confirmation list through Session state: that path cannot survive recovery
    // without duplicating the published policy.
    let published_configuration = installed
        .as_ref()
        .map(|snapshot| snapshot.resolved_spec.plugin_config.clone())
        .unwrap_or_else(|| {
            awaken_runtime_contract::agent_bindings::ResolvedConfiguration::new(
                Default::default(),
                host.capabilities.tools.plugin_config.clone(),
            )
        });
    let published_toolsets = installed
        .as_ref()
        .map(|snapshot| snapshot.resolved_spec.plugin_config.agent.toolsets.clone())
        .unwrap_or_default();
    let session_tools = realization.sessions().frozen(thread)?.tools.clone();
    let toolsets = session_tools
        .as_ref()
        .map(|tools| tools.toolsets.clone())
        .unwrap_or(published_toolsets);
    // Management tools (ADR-0052) remain pre-authorized: they are read-only,
    // and only the reserved-scope assistant's config names them.
    let mut admin_ids: Vec<String> = host
        .capabilities
        .tools
        .admin_tools
        .iter()
        .map(|t| t.id().to_string())
        .collect();
    if managed_coordination {
        admin_ids.extend([
            awaken_ext_builtin_tools::LIST_AGENTS.to_string(),
            awaken_ext_builtin_tools::SEND_MESSAGE.to_string(),
        ]);
    }
    let pre_authorized = pre_authorized_tool_ids(&admin_ids);
    // `plugin_config.skills_dir` is the direct/non-Managed authored-workspace
    // compatibility setting. Managed repository discovery always uses the
    // provider-fixed `.claude/skills` path under each realized mount, while
    // attached Managed bundles materialize under runtime-owned `.skills`.
    let authored_skills_subdir = if session_dispatch {
        crate::skills::MANAGED_SKILLS_SUBDIR.to_string()
    } else {
        installed
            .as_ref()
            .and_then(|c| {
                c.resolved_spec
                    .plugin_config
                    .get("skills_dir")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if installed.is_some() {
                    crate::skills::MANAGED_SKILLS_SUBDIR.to_string()
                } else {
                    crate::skills::DEFAULT_SKILLS_SUBDIR.to_string()
                }
            })
    };
    let skill_source_roots =
        host.session_skill_source_roots(thread, installed.as_ref(), &authored_skills_subdir)?;
    let authorization =
        effective_tool_authorization(&published_configuration, &pre_authorized, &toolsets);
    Ok(ContextResources {
        plan,
        env,
        thread_id,
        commit,
        stream_checkpoint,
        active_mcp,
        mcp,
        has_published_multiagent,
        has_published_advisor,
        managed_coordination,
        coordination_endpoint,
        content_delivery,
        session_tools,
        toolsets,
        authored_skills_subdir,
        skill_source_roots,
        authorization,
    })
}
