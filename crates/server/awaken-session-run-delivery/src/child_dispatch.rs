// @formal entrypoint delegation.independent_child_scheduling_lease
//! Durable dispatch construction for a child Run.
//!
//! A delegated child is admitted to the same queue as a root Run. This module
//! owns the one place that freezes a child's publication closure, resource
//! manifest, and backend placement into that durable request.

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::RunDispatch;
use awaken_runtime_contract::activation::RunActivation;

/// Why a child Run could not be frozen into its durable request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ChildDispatchRefusal(String);

pub fn child_dispatch_request(
    activation: RunActivation,
    parent_thread_id: ThreadId,
    session_resources: Option<
        awaken_session_configuration_contract::resource::SessionResourceManifest,
    >,
    publication_source: Option<&dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
) -> Result<RunDispatch, ChildDispatchRefusal> {
    // Delegated publications are frozen FROM a Workspace, so this coordinate
    // selects whose published Agent a child may inherit. A compiled-in default
    // would let a child freeze another tenant's publication, so an unknown
    // Workspace is refused rather than guessed. A parent that delegates to
    // nothing never consults it, and keeps working without a resource manifest.
    let workspace = match session_resources.as_ref() {
        Some(resources) => resources.workspace_id.as_str(),
        None if activation
            .snapshot
            .resolved_spec
            .plugin_config
            .agent
            .delegates
            .is_empty() =>
        {
            ""
        }
        None => {
            return Err(ChildDispatchRefusal(
                "child dispatch cannot freeze delegated publications without the \
                 session's Workspace"
                    .to_string(),
            ));
        }
    };
    let child_publications = crate::RUN_PUBLICATIONS
        .freeze(&activation.snapshot, publication_source, workspace)
        .map_err(|error| ChildDispatchRefusal(error.to_string()))?;
    let inference_holder =
        awaken_dispatch_execution::placement::self_hosted_inference_holder(&activation)
            .map_err(|error| ChildDispatchRefusal(error.to_string()))?;
    let mut placement = awaken_dispatch_execution::placement::remote_worker_placement(
        &activation.snapshot.resolved_spec,
        None,
        session_resources.as_ref(),
        false,
    );
    if activation.inherited_toolsets().is_some() {
        placement
            .required_capabilities
            .insert(awaken_runtime_contract::activation::INHERITED_TOOLSETS_CAPABILITY.to_string());
    }
    let mut request = RunDispatch::new(activation)
        .for_session(parent_thread_id)
        .with_traceparent(awaken_observability::current_traceparent())
        .with_agent_publications(child_publications)
        .with_placement(placement);
    if let Some(holder) = inference_holder {
        request = request.with_inference_plaintext_holder(holder);
    }
    if let Some(resources) = session_resources {
        let envelope = awaken_dispatch_execution::session_envelope::resource_envelope(&resources)
            .map_err(|error| {
            ChildDispatchRefusal(format!(
                "serialize child Session resource manifest: {error}"
            ))
        })?;
        request = request
            .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
                awaken_agent_contract::ScopeId::from(resources.workspace_id.clone()),
            ))
            .with_session_resources(envelope);
    }
    Ok(request)
}
