//! Publication assembly owned by [`ConfigService`](super::ConfigService).
//!
//! Keeping resolution, resource freezing, compilation, and registration-shape
//! construction together makes publication preparation one reviewable boundary
//! without mixing it into the service's authoring and reconciliation methods.

use awaken_agent_config::{ConfigRegistry, StoredPublication};
use awaken_executable_agent_contract::ExecutableAgentRegistration;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

use super::ConfigService;
use crate::agent_projection::registered_session_profile;
use crate::credential_reference::validate_credential_references;
use crate::plugin_validation::resolve_plugin_configuration;
use crate::publication::{
    PreparedPublication, PublishError, prepare_agent_publication, snapshot_metadata,
};

pub(super) async fn prepare_publication(
    service: &ConfigService,
    workspace: &ScopeId,
    registry: &dyn ConfigRegistry,
    id: &str,
    catalog: &[ToolDescriptor],
    expected_source_revision: Option<u64>,
    expected_resource_revision: Option<i64>,
) -> Result<PreparedPublication, PublishError> {
    let versioned = registry
        .get_config_revision(id)
        .await
        .map_err(|error| PublishError::Store(error.to_string()))?
        .ok_or_else(|| PublishError::NotStored(id.to_string()))?;
    if expected_source_revision.is_some_and(|expected| expected != versioned.revision) {
        return Err(PublishError::StaleRevision(Some(versioned.revision)));
    }
    if versioned.config.lifecycle() != awaken_agent_config::AgentLifecycle::Published {
        return Err(PublishError::Unavailable(id.to_string()));
    }
    let source_revision = versioned.revision;
    let mut resolved = prepare_agent_publication(
        service.model_publication_resolver.as_ref(),
        workspace,
        versioned,
    )
    .await?;
    resolve_plugin_configuration(
        &service.plugin_publication_resolvers,
        workspace,
        &mut resolved.config,
    )
    .await
    .map_err(|error| PublishError::Unresolvable(format!("{}: {}", error.path, error.message)))?;
    validate_credential_references(
        service.credential_reference_validator.as_ref(),
        workspace,
        &resolved.config,
    )
    .await
    .map_err(|error| PublishError::Unresolvable(format!("{}: {}", error.path, error.message)))?;
    let mut metadata = snapshot_metadata(&resolved);
    let defaults = match service.resources.as_ref() {
        Some(store) => store
            .get_agent_inputs(workspace.as_str(), id)
            .map_err(|error| PublishError::Store(error.to_string()))?,
        None => None,
    };
    let current_resource_revision = defaults.as_ref().map_or(0, |inputs| inputs.revision);
    if expected_resource_revision.is_some_and(|expected| expected != current_resource_revision) {
        return Err(PublishError::StaleResourceRevision(
            current_resource_revision,
        ));
    }
    if let Some(defaults) = &defaults {
        let mut inputs = std::mem::take(&mut metadata.resolution.inputs);
        inputs.push(awaken_runtime_contract::ResolvedInputRef {
            kind: "agent_session_defaults".into(),
            id: id.to_string(),
            version: awaken_runtime_contract::ResolvedInputVersion::Revision(
                defaults.revision as u64,
            ),
        });
        metadata.resolution = awaken_runtime_contract::ResolutionManifest::new(inputs)
            .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
    }
    let snapshot = awaken_agent_config::compile_published(
        &resolved.config,
        catalog,
        metadata,
        resolved.models.primary,
        resolved.models.candidates,
        resolved.advisor,
    )
    .map_err(|error| PublishError::Compile(error.to_string()))?;
    let stored_inputs = defaults.clone();
    let publication = StoredPublication::published_at_revision(
        snapshot.clone(),
        id,
        source_revision,
        workspace.as_str(),
    )
    .with_agent_inputs(stored_inputs);
    let session_profile = registered_session_profile(
        &snapshot,
        &resolved.config,
        &resolved.authored_model_selection,
        defaults,
    )
    .ok_or_else(|| {
        PublishError::Registration(
            awaken_executable_agent_contract::ExecutableAgentRegistrationError::Invalid(
                "Agent Session defaults changed while the publication was compiled".into(),
            ),
        )
    })?;
    Ok(PreparedPublication {
        publication,
        registration: ExecutableAgentRegistration {
            workspace_id: workspace.as_str().to_owned(),
            agent_id: id.to_owned(),
            source_revision,
            snapshot,
            session_profile,
        },
    })
}
