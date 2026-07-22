//! Configuration publication preparation and values shared by the service and HTTP edge.

use awaken_config_resolver::InferenceAccessPublisher;
use awaken_config_store::{AgentConfig, AgentConfigRevision, ModelSelection};
use awaken_runtime_contract::{InferenceAccess, ResolutionManifest};

use crate::binding_resolver::ModelResolver;
use crate::compaction::apply_compaction;

/// A publish failure, split so the edge can map it to an HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("reserved configuration publication requires an explicit execution Workspace")]
    ExecutionWorkspaceRequired,
    #[error("no config stored for agent `{0}`")]
    NotStored(String),
    #[error("agent `{0}` is archived")]
    Archived(String),
    #[error("cannot resolve an auto model binding: {0}")]
    Unresolvable(String),
    #[error("config changed while it was being published (current revision: {0:?})")]
    StaleRevision(Option<u64>),
    #[error("{0}")]
    Compile(String),
    #[error("{0}")]
    Store(String),
}

/// A compile problem projected onto the authored config field that caused it.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

/// Transient, secret-free result of reading every configuration source once.
#[derive(Debug)]
pub(crate) struct AgentPublicationDraft {
    pub(crate) source: awaken_runtime_contract::AgentConfigRevisionRef,
    pub(crate) config: AgentConfig,
    pub(crate) manifest: ResolutionManifest,
    pub(crate) inference_access: Option<InferenceAccess>,
}

/// Read every authored configuration input once and prepare one publication.
/// Runtime and worker paths receive only the resulting immutable snapshot.
pub(crate) fn prepare_agent_publication(
    model_resolver: Option<&dyn ModelResolver>,
    source: AgentConfigRevision,
) -> Result<AgentPublicationDraft, PublishError> {
    let source_revision = source.revision;
    let mut config = source.config;
    if crate::binding_resolver::needs_resolution(&config.model_binding) {
        let resolver = model_resolver
            .ok_or_else(|| PublishError::Unresolvable("no model resolver wired".into()))?;
        let resolved = resolver
            .resolve_auto()
            .map_err(PublishError::Unresolvable)?;
        config.model_binding = ModelSelection::Pinned(resolved.primary);
        config.model_candidates = resolved.candidates;
    }
    if let Some(resolver) = model_resolver
        && let Some(model_id) = config.model_binding.resolved().map(|b| b.model_ref.clone())
    {
        let strategy = config.compaction.clone().unwrap_or_default();
        apply_compaction(
            &mut config.plugin_config,
            &strategy,
            resolver.context_window(&model_id),
            resolver.max_output_tokens(&model_id),
        );
    }
    let model_bytes = serde_json::to_vec(&(
        config.model_binding.resolved(),
        &config.model_candidates,
        &config.compaction,
    ))
    .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
    let manifest = ResolutionManifest::new([
        awaken_runtime_contract::ResolvedInputRef {
            kind: "agent_config".into(),
            id: config.id.clone(),
            version: awaken_runtime_contract::ResolvedInputVersion::Revision(source_revision),
        },
        awaken_runtime_contract::ResolvedInputRef {
            kind: "model_binding".into(),
            id: config
                .model_binding
                .resolved()
                .map(|binding| binding.model_ref.clone())
                .unwrap_or_default(),
            version: awaken_runtime_contract::ResolvedInputVersion::ContentHash(
                awaken_runtime_contract::content_fingerprint(&model_bytes)
                    .map_err(|error| PublishError::Unresolvable(error.to_string()))?,
            ),
        },
    ])
    .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
    Ok(AgentPublicationDraft {
        source: awaken_runtime_contract::AgentConfigRevisionRef {
            agent_id: awaken_runtime_contract::snapshot::AgentId(config.id.clone()),
            revision: source_revision,
        },
        config,
        manifest,
        inference_access: None,
    })
}

/// Select provider/gateway access or the explicit local-host access exactly once.
pub(crate) async fn pin_inference_access(
    publisher: Option<&dyn InferenceAccessPublisher>,
    workspace: &str,
    draft: &AgentPublicationDraft,
) -> Result<InferenceAccess, PublishError> {
    let models = draft
        .config
        .model_binding
        .resolved()
        .into_iter()
        .chain(draft.config.model_candidates.iter())
        .cloned()
        .collect::<Vec<_>>();
    if let Some(publisher) = publisher {
        return publisher
            .resolve_access(workspace, &models)
            .await
            .map_err(PublishError::Unresolvable);
    }
    InferenceAccess::candidate_set(models.iter().map(|model| {
        (
            model.model_ref.clone(),
            InferenceAccess::host_executor(&model.model_ref),
        )
    }))
    .ok_or_else(|| {
        PublishError::Unresolvable(
            "published agent has no resolved model for inference access".into(),
        )
    })
}

pub(crate) fn snapshot_metadata(
    resolved: &AgentPublicationDraft,
) -> awaken_runtime_contract::AgentSnapshotMetadata {
    awaken_runtime_contract::AgentSnapshotMetadata {
        source: resolved.source.clone(),
        publication_version: awaken_runtime_contract::AgentPublicationVersion(String::new()),
        resolution: resolved.manifest.clone(),
        fingerprint: awaken_runtime_contract::AgentSnapshotFingerprint(String::new()),
        inference_access: resolved.inference_access.clone(),
    }
}
