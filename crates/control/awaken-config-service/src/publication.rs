//! Configuration publication preparation and values shared by the service and HTTP edge.

use awaken_agent_config::{AgentConfig, AgentConfigRevision, ModelSelection, StoredPublication};
use awaken_executable_agent_contract::{
    ExecutableAgentRegistration, ExecutableAgentRegistrationError,
};
use awaken_runtime_contract::ResolutionManifest;

use crate::binding_resolver::{
    ModelPublicationResolver, PublicationResolutionError, ResolvedPublicationModels,
};
use crate::compaction::apply_compaction;

/// One prepared publication and the exact executable registration derived from
/// it. Keeping the pair in the publication module prevents the Config service
/// from growing a second transient publication representation.
pub(crate) struct PreparedPublication {
    pub(crate) publication: StoredPublication,
    pub(crate) registration: ExecutableAgentRegistration,
}

/// A publish failure, split so the edge can map it to an HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("reserved configuration publication requires an explicit execution Workspace")]
    ExecutionWorkspaceRequired,
    #[error("no config stored for agent `{0}`")]
    NotStored(String),
    #[error("agent `{0}` is disabled or archived")]
    Unavailable(String),
    #[error("cannot resolve model publication: {0}")]
    Unresolvable(String),
    #[error("config changed while it was being published (current revision: {0:?})")]
    StaleRevision(Option<u64>),
    #[error("resource bindings changed before publication (current revision: {0})")]
    StaleResourceRevision(i64),
    #[error("{0}")]
    Compile(String),
    #[error("{0}")]
    Store(String),
    #[error("publication persisted but executable registration failed: {0}")]
    Registration(ExecutableAgentRegistrationError),
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
    /// The public authoring intent that produced this publication. Resolution
    /// replaces `config.model_binding` with the immutable runtime pin, so this
    /// value must travel separately to keep protocol projections exact.
    pub(crate) authored_model_selection: ModelSelection,
    pub(crate) config: AgentConfig,
    pub(crate) manifest: ResolutionManifest,
    pub(crate) models: ResolvedPublicationModels,
    pub(crate) advisor: Option<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
}

fn validate_candidate_scope_and_proof(
    workspace: &awaken_tenancy::ScopeId,
    candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
) -> Result<(), PublishError> {
    let candidate_scope = match candidate.provisioning() {
        awaken_runtime_contract::resolved::ModelProvisioning::Provider { scope_id, .. }
        | awaken_runtime_contract::resolved::ModelProvisioning::Remote { scope_id, .. } => {
            Some(scope_id)
        }
        _ => None,
    };
    if let Some(scope_id) = candidate_scope
        && scope_id != workspace
    {
        return Err(PublishError::Unresolvable(
            PublicationResolutionError::CandidateUnavailable {
                binding: candidate.binding().clone(),
                reason: format!(
                    "resolved candidate belongs to Workspace {scope_id}, not trusted execution Workspace {workspace}"
                ),
            }
            .to_string(),
        ));
    }
    if matches!(
        candidate.provisioning(),
        awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
            acp,
            ..
        } if acp.capability_fingerprint.trim().is_empty()
            || acp.capability_adapter_version.trim().is_empty()
    ) {
        return Err(PublishError::Unresolvable(
            "backend-owned publication requires a fresh exact ACP capability pin".into(),
        ));
    }
    if matches!(
        candidate.provisioning(),
        awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            security_fingerprint,
            ..
        } if security_fingerprint.trim().is_empty()
    ) {
        return Err(PublishError::Unresolvable(
            "remote publication requires an exact Agent Card security fingerprint".into(),
        ));
    }
    Ok(())
}

/// Read every authored configuration input once and prepare one publication.
/// Runtime and worker paths receive only the resulting immutable snapshot.
pub(crate) async fn prepare_agent_publication(
    model_resolver: &dyn ModelPublicationResolver,
    workspace: &awaken_tenancy::ScopeId,
    source: AgentConfigRevision,
) -> Result<AgentPublicationDraft, PublishError> {
    let source_revision = source.revision;
    let mut config = source.config;
    let authored_model_selection = config.model_binding.clone();
    let models = model_resolver
        .resolve_models(workspace, &config.model_binding, &config.model_fallbacks)
        .await
        .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
    let mut bindings = std::collections::BTreeSet::new();
    for candidate in std::iter::once(&models.primary).chain(models.candidates.iter()) {
        validate_candidate_scope_and_proof(workspace, candidate)?;
        if !bindings.insert(candidate.binding().clone()) {
            return Err(PublishError::Unresolvable(
                PublicationResolutionError::DuplicateBinding(candidate.binding().clone())
                    .to_string(),
            ));
        }
    }
    let advisor_model = config.multiagent.as_ref().and_then(|multiagent| {
        multiagent
            .agents
            .iter()
            .find_map(awaken_agent_config::MultiagentTarget::advisor_model)
    });
    let advisor = match advisor_model {
        Some(model) => {
            let selection = crate::parse_managed_model_id(model)
                .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
            let resolved = model_resolver
                .resolve_models(workspace, &selection, &[])
                .await
                .map_err(|error| PublishError::Unresolvable(error.to_string()))?;
            if !resolved.candidates.is_empty() {
                return Err(PublishError::Unresolvable(
                    "advisor resolution must return exactly one model candidate".into(),
                ));
            }
            validate_candidate_scope_and_proof(workspace, &resolved.primary)?;
            Some(resolved.primary)
        }
        None => None,
    };
    if let Some(backend_ref) = config.model_binding.backend_default_ref() {
        let valid = !backend_ref.trim().is_empty()
            && models.primary.binding().backend_ref == backend_ref
            && models.primary.binding().model_ref.is_empty()
            && matches!(
                models.primary.provisioning(),
                awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
                    model_selection:
                        awaken_runtime_contract::resolved::BackendModelSelection::Default,
                    ..
                }
            );
        if !valid {
            return Err(PublishError::Unresolvable(
                "backend-default resolution must preserve the exact backend, default-model policy, and Worker-local ownership"
                    .into(),
            ));
        }
    }
    if let Some((backend_ref, model_ref)) = config.model_binding.backend_exact() {
        let valid = models.primary.binding().backend_ref == backend_ref
            && models.primary.binding().model_ref == model_ref
            && matches!(
                models.primary.provisioning(),
                awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
                    model_selection:
                        awaken_runtime_contract::resolved::BackendModelSelection::Exact,
                    ..
                }
            );
        if !valid {
            return Err(PublishError::Unresolvable(
                "backend-exact resolution must preserve the exact backend, model, and Worker-local ownership"
                    .into(),
            ));
        }
    }
    if let Some(authored) = config.model_binding.resolved()
        && !resolved_binding_matches_authored(models.primary.binding(), authored)
    {
        return Err(PublishError::Unresolvable(
            "resolved primary candidate does not match the pinned authoring binding".into(),
        ));
    }
    if (config.model_binding.resolved().is_some()
        || config.model_binding.backend_default_ref().is_some()
        || config.model_binding.backend_exact().is_some())
        && (models.candidates.len() != config.model_fallbacks.len()
            || models
                .candidates
                .iter()
                .zip(&config.model_fallbacks)
                .any(|(resolved, authored)| {
                    !resolved_binding_matches_authored(resolved.binding(), authored)
                }))
    {
        return Err(PublishError::Unresolvable(
            "resolved fallback candidates do not match the pinned authoring order".into(),
        ));
    }
    config.model_binding = ModelSelection::Pinned(models.primary.binding().clone());
    config.model_fallbacks = models
        .candidates
        .iter()
        .map(|candidate| candidate.binding().clone())
        .collect();
    if config.plugin_ids.iter().any(|id| id == "compact") {
        config
            .plugin_config
            .entry("compact".into())
            .or_insert_with(|| serde_json::json!({}));
    }
    if config
        .model_binding
        .resolved()
        .is_some_and(|binding| binding.backend_ref.starts_with("acp:"))
    {
        config
            .plugin_config
            .entry("acp".into())
            .or_insert_with(|| serde_json::json!({}));
    }
    let strategy = config.compaction.clone().unwrap_or_default();
    apply_compaction(
        &mut config.plugin_config,
        &strategy,
        models.context_window,
        models.max_output_tokens,
    );
    let model_bytes = serde_json::to_vec(&(
        config.model_binding.resolved(),
        &config.model_fallbacks,
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
        authored_model_selection,
        config,
        manifest,
        models,
        advisor,
    })
}

fn resolved_binding_matches_authored(
    resolved: &awaken_runtime_contract::resolved::ModelBinding,
    authored: &awaken_runtime_contract::resolved::ModelBinding,
) -> bool {
    resolved == authored
}

pub(crate) fn snapshot_metadata(
    resolved: &AgentPublicationDraft,
) -> awaken_runtime_contract::AgentSnapshotMetadata {
    awaken_runtime_contract::AgentSnapshotMetadata {
        source: resolved.source.clone(),
        publication_version: awaken_runtime_contract::AgentPublicationVersion(String::new()),
        resolution: resolved.manifest.clone(),
        fingerprint: awaken_runtime_contract::AgentSnapshotFingerprint(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::{
        CredentialRef, InferenceEndpoint,
        resolved::{BackendModelSelection, ModelBinding, ResolvedModelCandidate},
    };

    fn revision(selection: ModelSelection, fallbacks: Vec<ModelBinding>) -> AgentConfigRevision {
        AgentConfigRevision {
            config: AgentConfig {
                id: "agent-a".into(),
                model_binding: selection,
                model_fallbacks: fallbacks,
                ..Default::default()
            },
            revision: 7,
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
        }
    }

    struct FixedResolver {
        expected_workspace: &'static str,
        output: ResolvedPublicationModels,
    }

    #[async_trait::async_trait]
    impl ModelPublicationResolver for FixedResolver {
        async fn resolve_models(
            &self,
            workspace: &awaken_tenancy::ScopeId,
            _selection: &ModelSelection,
            _candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            if workspace.as_str() != self.expected_workspace {
                return Err(format!("unexpected Workspace {workspace}").into());
            }
            Ok(self.output.clone())
        }
    }

    #[tokio::test]
    async fn resolver_receives_execution_workspace_and_auto_output_becomes_authoring_projection() {
        let primary = ModelBinding::new("provider", "primary", "genai");
        let fallback = ModelBinding::new("provider", "fallback", "genai");
        let resolver = FixedResolver {
            expected_workspace: "workspace-real",
            output: ResolvedPublicationModels::host(
                primary.clone(),
                vec![fallback.clone()],
                None,
                None,
            ),
        };
        let draft = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-real"),
            revision(ModelSelection::Auto, Vec::new()),
        )
        .await
        .unwrap();
        assert_eq!(draft.config.model_binding.resolved(), Some(&primary));
        assert_eq!(draft.config.model_fallbacks, vec![fallback]);
    }

    #[tokio::test]
    async fn resolver_cannot_complete_an_incomplete_pinned_binding() {
        // Causes: C1 Pinned claims to be complete; C2 its provider/backend axes
        // are absent; C3 a resolver returns a completed binding. Effect: reject
        // instead of creating a second public selection path beside Target.
        let resolved = ModelBinding::new("provider", "primary", "genai");
        let resolver = FixedResolver {
            expected_workspace: "workspace-a",
            output: ResolvedPublicationModels::host(resolved.clone(), vec![], None, None),
        };
        let error = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            revision(
                ModelSelection::Pinned(ModelBinding::new("", "primary", "")),
                vec![],
            ),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("pinned authoring binding"));
    }

    #[tokio::test]
    async fn resolver_cannot_rewrite_pinned_primary_or_fallback_order() {
        let primary = ModelBinding::new("provider", "primary", "genai");
        let fallback = ModelBinding::new("provider", "fallback", "genai");
        let rewritten = ModelBinding::new("provider", "other", "genai");
        let resolver = FixedResolver {
            expected_workspace: "workspace-a",
            output: ResolvedPublicationModels::host(primary.clone(), vec![rewritten], None, None),
        };
        let error = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            revision(ModelSelection::Pinned(primary), vec![fallback]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("fallback candidates"));
    }

    #[tokio::test]
    async fn duplicate_complete_bindings_reject_the_entire_publication() {
        let binding = ModelBinding::new("provider", "model", "genai");
        let resolver = FixedResolver {
            expected_workspace: "workspace-a",
            output: ResolvedPublicationModels::host(binding.clone(), vec![binding], None, None),
        };

        let error = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            revision(ModelSelection::Auto, Vec::new()),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("duplicate model candidate"));
    }

    #[tokio::test]
    async fn publication_fences_every_scope_bearing_candidate() {
        // Cause/effect graph:
        // C1 provisioning carries a scope (Provider or Remote); C2 scope equals
        // the trusted execution Workspace. C1+!C2 -> E1 reject the complete
        // publication before compilation/persistence; C1+C2 -> E2 accept; !C1
        // is governed by the provisioning variant's own validation.
        //
        // Decision table:
        // R1 Provider + same Workspace  -> E2
        // R2 Provider + other Workspace -> E1
        // R3 Remote   + same Workspace  -> E2
        // R4 Remote   + other Workspace -> E1
        let binding = ModelBinding::new("provider-account", "model", "genai");
        let provider = |scope: &str| {
            ResolvedModelCandidate::try_provider(
                binding.clone(),
                "provider@1",
                "route@1",
                scope,
                None,
                InferenceEndpoint {
                    adapter_kind: "openai".into(),
                    api_dialect: "open_ai_responses".into(),
                    base_url: "https://gateway.internal/v1".into(),
                    upstream_model: "provider-model".into(),
                    processing_placement: None,
                },
            )
        };
        let remote_binding = ModelBinding::new("", "", "a2a:https://agent.example");
        let remote = |scope: &str| {
            ResolvedModelCandidate::try_remote(
                remote_binding.clone(),
                scope,
                None,
                "sha256:agent-card",
            )
        };

        for accepted in [provider("workspace-a"), remote("workspace-a")]
            .map(|candidate| candidate.expect("coherent scoped candidate"))
        {
            let resolver = FixedResolver {
                expected_workspace: "workspace-a",
                output: ResolvedPublicationModels {
                    primary: accepted,
                    candidates: Vec::new(),
                    context_window: None,
                    max_output_tokens: None,
                },
            };
            prepare_agent_publication(
                &resolver,
                &awaken_tenancy::ScopeId::from("workspace-a"),
                revision(ModelSelection::Auto, Vec::new()),
            )
            .await
            .expect("R1/R3: trusted scope is publishable");
        }

        for rejected in [provider("workspace-b"), remote("workspace-b")]
            .map(|candidate| candidate.expect("coherent cross-Workspace candidate"))
        {
            let resolver = FixedResolver {
                expected_workspace: "workspace-a",
                output: ResolvedPublicationModels {
                    primary: rejected,
                    candidates: Vec::new(),
                    context_window: None,
                    max_output_tokens: None,
                },
            };
            let error = prepare_agent_publication(
                &resolver,
                &awaken_tenancy::ScopeId::from("workspace-a"),
                revision(ModelSelection::Auto, Vec::new()),
            )
            .await
            .expect_err("R2/R4: cross-Workspace candidate must fail closed");
            let message = error.to_string();
            assert!(message.contains("Workspace workspace-b"));
            assert!(message.contains("Workspace workspace-a"));
        }
    }

    #[tokio::test]
    async fn backend_default_publication_preserves_backend_ownership_and_policy() {
        // Cause graph:
        // BackendDefault(acp:codex) -> resolver -> BackendOwned(Default) -> publish.
        // Any resolver rewrite of backend, ownership, or policy -> reject before
        // the immutable snapshot can erase the author's default-model intent.
        //
        // Decision table:
        // D1 exact backend + empty model + BackendOwned(Default) => accept
        // D2 HostExecutor                                      => reject
        // D3 BackendOwned(Exact)                               => reject
        // D4 BackendOwned without fresh capability pin         => reject
        let binding = ModelBinding::new("local-codex", "", "acp:codex");
        let credential = CredentialRef {
            id: "local-codex".into(),
            revision: 4,
        };
        let valid = ResolvedModelCandidate::try_backend_owned(
            binding.clone(),
            credential.clone(),
            BackendModelSelection::Default,
            "test",
            "sha256:test-capability",
            Default::default(),
        )
        .expect("coherent backend-default candidate");
        let resolver = FixedResolver {
            expected_workspace: "workspace-a",
            output: ResolvedPublicationModels {
                primary: valid,
                candidates: vec![],
                context_window: None,
                max_output_tokens: None,
            },
        };
        let draft = prepare_agent_publication(
            &resolver,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            revision(
                ModelSelection::try_backend_default("acp:codex", Default::default())
                    .expect("exact ACP backend"),
                vec![],
            ),
        )
        .await
        .unwrap();
        assert_eq!(draft.config.model_binding.resolved(), Some(&binding));

        for invalid in [
            ResolvedModelCandidate::host(binding.clone()),
            ResolvedModelCandidate::try_backend_owned(
                ModelBinding::new("local-codex", "gpt-exact", "acp:codex"),
                credential.clone(),
                BackendModelSelection::Exact,
                "test",
                "sha256:test-capability",
                Default::default(),
            )
            .expect("coherent backend-exact candidate"),
        ] {
            let resolver = FixedResolver {
                expected_workspace: "workspace-a",
                output: ResolvedPublicationModels {
                    primary: invalid,
                    candidates: vec![],
                    context_window: None,
                    max_output_tokens: None,
                },
            };
            let error = prepare_agent_publication(
                &resolver,
                &awaken_tenancy::ScopeId::from("workspace-a"),
                revision(
                    ModelSelection::try_backend_default("acp:codex", Default::default())
                        .expect("exact ACP backend"),
                    vec![],
                ),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("backend-default resolution"));
        }

        let error = ResolvedModelCandidate::try_backend_owned(
            binding,
            credential,
            BackendModelSelection::Default,
            "",
            "",
            Default::default(),
        )
        .expect_err("D4: missing capability pin is not constructible");
        assert!(error.to_string().contains("capability pin"), "D4");
    }

    #[tokio::test]
    async fn remote_publication_requires_discovery_fingerprint_evidence() {
        // Cause/effect: C1 a resolver returns Remote provisioning; C2 its card
        // security fingerprint is non-empty. C1+C2 accepts the exact candidate;
        // C1+!C2 rejects before snapshot installation. A HostExecutor bearing an
        // a2a backend is legacy test composition and is not accepted as proof.
        let binding = ModelBinding::new("", "", "a2a:https://agent.example");
        let revision = || revision(ModelSelection::Pinned(binding.clone()), vec![]);
        let output = |primary| ResolvedPublicationModels {
            primary,
            candidates: vec![],
            context_window: None,
            max_output_tokens: None,
        };
        let valid = FixedResolver {
            expected_workspace: "workspace-a",
            output: output(
                ResolvedModelCandidate::try_remote(
                    binding.clone(),
                    "workspace-a",
                    None,
                    "sha256:card",
                )
                .expect("coherent remote candidate"),
            ),
        };
        prepare_agent_publication(
            &valid,
            &awaken_tenancy::ScopeId::from("workspace-a"),
            revision(),
        )
        .await
        .expect("fresh Agent Card evidence");

        let error = ResolvedModelCandidate::try_remote(binding.clone(), "workspace-a", None, "")
            .expect_err("missing Agent Card proof is not constructible");
        assert!(error.to_string().contains("security fingerprint"));
    }
}
