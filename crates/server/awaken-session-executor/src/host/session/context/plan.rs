// @formal evidence session_run.reservation_never_executes
//! Resolve and validate frozen construction inputs before physical acquisition.
//!
//! Legacy Skill catalog refresh remains here because its support-file evidence
//! must precede Environment deferral. This stage never creates an Environment.

use crate::host::{ClaimedRuntimeInput, RuntimePublicationIdentity};
use crate::{HostError, SharedHost};
use awaken_runtime_contract::ExecutableAgentSnapshot;
use std::sync::Arc;

pub(super) struct ContextPlan {
    pub(super) workspace: String,
    pub(super) selected_agent: String,
    pub(super) installed: Option<ExecutableAgentSnapshot>,
    pub(super) model_candidate: Option<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
    pub(super) fallback_model_ref: String,
    pub(super) publication_source: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    pub(super) unclaimed_runtime_publications: Option<Vec<ExecutableAgentSnapshot>>,
    pub(super) session_dispatch: bool,
    pub(super) frozen_skill_versions: Option<Vec<awaken_resource_contract::SkillVersion>>,
    pub(super) deferred_skill_bindings: Option<Vec<crate::session_resource::ResolvedSkillBinding>>,
    pub(super) execution_backend: awaken_runtime_contract::resolved::Backend,
    pub(super) background_tasks_enabled: bool,
    pub(super) background_attention: Option<
        std::sync::Weak<dyn awaken_session_application_contract::SessionRunBackgroundApplication>,
    >,
    pub(super) a2a_only: bool,
    pub(super) claimed_runtime_identity: Option<RuntimePublicationIdentity>,
}

pub(super) async fn prepare(
    host: &SharedHost,
    thread: &str,
    agent: Option<&str>,
    published_snapshot: Option<ExecutableAgentSnapshot>,
    claimed_attempt: Option<&ClaimedRuntimeInput>,
    force_defer_environment: bool,
) -> Result<ContextPlan, HostError> {
    let realizations = &host.realizer.environments.sessions;
    // Resolve the immutable publication before selecting an environment.
    // Environment identity is a consequence of provisioning, not of the ACP
    // executor or a process-wide sandbox default.
    let publication = &host.realizer.publication;
    let (workspace, selected_agent, installed, model_candidate) =
        publication.resolve_session_publication(thread, agent, published_snapshot)?;
    publication.retain_session_publication(thread, installed.as_ref())?;
    let fallback_model_ref = claimed_attempt.as_ref().map_or_else(
        || {
            let frozen_model_ref = installed
                .as_ref()
                .map_or(host.agent.model_ref.as_str(), |root| {
                    root.resolved_spec.model_binding.identity().1
                });
            host.agent
                .inference_routing
                .model_ref(thread, frozen_model_ref)
        },
        |attempt| attempt.effective_model_ref.clone(),
    );
    let (publication_source, unclaimed_runtime_publications): (
        Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
        Option<Vec<awaken_runtime_contract::ExecutableAgentSnapshot>>,
    ) = if let Some(attempt) = &claimed_attempt {
        (attempt.publications.clone(), None)
    } else {
        let frozen = installed
            .as_ref()
            .map(|root| {
                awaken_dispatch_execution::run_publications::freeze_run_publications(
                    root,
                    host.agent.agent_publications.as_deref(),
                    &workspace,
                )
            })
            .transpose()
            .map_err(HostError::bad_request)?
            .unwrap_or_default();
        let source = Arc::new(
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(frozen.clone())
                .map_err(|error| {
                    HostError::bad_request(format!("invalid Agent publication closure: {error}"))
                })?,
        );
        (source, Some(frozen))
    };
    // Managed dispatch is the profile boundary for every Skill source, not
    // merely for its eventual presentation. Read it before any catalog I/O
    // so an empty Managed projection never depends on or reloads the live
    // direct-compatibility catalog.
    let session_dispatch = host
        .installed_sessions
        .read_run(thread, |run| run.session_dispatch)
        .unwrap_or(false);
    // Direct protocol Sessions have no aggregate Resource projection, but
    // Environment realization still consumes the same exact transition as
    // Managed and claimed execution. Project the direct empty generation
    // once instead of teaching the provider a second baseline-only create
    // path. A Managed/claimed caller must already carry its aggregate-owned
    // transition and therefore remains fail-closed when that fact is absent.
    if claimed_attempt.is_none() && !session_dispatch {
        let empty = crate::session_resource::SessionResourceManifest::at_revision(
            workspace.clone(),
            0,
            crate::session_resource::ResolvedSessionResources::default(),
        );
        let transition =
            crate::session_resource::SessionResourceTransition::new(empty.clone(), empty)
                .map_err(|error| HostError::internal(error.to_string()))?;
        realizations.update_realization(thread, |r| {
            if r.staging.transition.is_none() {
                r.staging.transition = Some(transition);
            }
        });
    }
    // Legacy direct Session manifests carry no frozen Skill list. Refresh
    // their canonical delivered catalog before deciding whether
    // `on_tool_use` may defer the Environment; doing this later in Skill
    // wiring can classify a cold cache as instruction-only and then discover
    // support files after the sandbox-free decision has already been made.
    let frozen_skill_versions = realizations
        .read_realization(thread, |r| r.staging.skills.clone())
        .flatten();
    // Reservation is metadata-only on a cold Coordinator. The aggregate
    // Resource transition already owns the exact immutable Skill pins,
    // while the claim-owning Worker remains the sole reader/validator of
    // bundle bytes after Environment adoption.
    let deferred_skill_bindings = (force_defer_environment && frozen_skill_versions.is_none())
        .then(|| {
            realizations
                .read_realization(thread, |r| {
                    let transition = r.staging.transition.as_ref()?;
                    Some(transition.desired().resources.skills().to_vec())
                })
                .flatten()
                .unwrap_or_default()
        });
    if !session_dispatch && frozen_skill_versions.is_none() {
        host.tools
            .skills
            .reload_cache_in(&workspace)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
    }
    let published_backend_ref = model_candidate
        .as_ref()
        .map(|candidate| candidate.binding().identity().2.to_owned());
    let execution_backend = published_backend_ref
        .as_deref()
        .map(awaken_runtime_contract::resolved::Backend::from_ref)
        .unwrap_or(awaken_runtime_contract::resolved::Backend::Native);
    if let Some(snapshot) = &installed {
        host.validate_mcp_input_placement(
            snapshot.resolved_spec.plugin_config.plugins(),
            execution_backend.is_acp(),
        )?;
    }
    // BackgroundTask completion is a process-local fenced projection. Reject
    // unsupported execution placement immediately after resolving the frozen
    // publication and before Environment, MCP, tool, or commit realization.
    // A late rejection would be semantically correct but would still leak a
    // physical Session side effect for work that can never be admitted.
    let plugin_ids = installed
        .as_ref()
        .map_or(host.tools.plugin_ids.as_slice(), |snapshot| {
            snapshot.resolved_spec.plugin_ids.as_slice()
        });
    let background_tasks_enabled = plugin_ids
        .iter()
        .any(|id| id == awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID);
    if background_tasks_enabled && execution_backend.is_acp() {
        return Err(HostError::bad_request(
            "background_task requires the Native backend so the canonical tool executor remains process-addressable",
        ));
    }
    if background_tasks_enabled && !host.authority.hosts_session_application() {
        return Err(HostError::bad_request(
            "background_task requires a co-located Session application until completion attention has a claim-fenced Worker transport",
        ));
    }
    let background_attention = if background_tasks_enabled {
        host.session_application
            .background_run_submission()
            .cloned()
            .filter(|application| application.strong_count() > 0)
    } else {
        None
    };
    if background_tasks_enabled && session_dispatch && background_attention.is_none() {
        return Err(HostError::internal(
            "Managed BackgroundTask Session has no background Run application authority",
        ));
    }
    let a2a_only = installed.as_ref().is_some_and(|snapshot| {
        !awaken_session_realization_contract::requires_local_environment(&snapshot.resolved_spec)
    });
    if a2a_only && host.session_has_local_environment_inputs(thread, installed.as_ref()) {
        return Err(HostError::bad_request(
            "remote A2A execution cannot consume local Session Environment inputs",
        ));
    }
    let claimed_runtime_identity = claimed_attempt.map(|attempt| attempt.identity.clone());
    Ok(ContextPlan {
        workspace,
        selected_agent,
        installed,
        model_candidate,
        fallback_model_ref,
        publication_source,
        unclaimed_runtime_publications,
        session_dispatch,
        frozen_skill_versions,
        deferred_skill_bindings,
        execution_backend,
        background_tasks_enabled,
        background_attention,
        a2a_only,
        claimed_runtime_identity,
    })
}
