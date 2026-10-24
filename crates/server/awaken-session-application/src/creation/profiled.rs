// @formal entrypoint agent_profile.requested_revision_never_follows_mutable_current
// @formal entrypoint session_creation.network_intersection_is_canonical_and_non_widening
// @formal entrypoint session_creation.selected_environment_is_frozen_in_baseline
//! Profiled Session creation: a Session frozen from one published Agent
//! profile, its Repository participants and its network restriction.

use std::collections::BTreeMap;

use awaken_session_configuration_contract::mcp::McpAttachmentOrigin;
use awaken_session_configuration_contract::{
    ApplicationOriginBinding, ControlSessionCreationInputs, SessionContextSeed,
    SessionCreationIntent, SessionMcpAuthoringContext, SessionNetworkPolicy,
    SessionSystemPromptSelection, SessionToolConfiguration, SessionTrigger,
};
use awaken_session_types::RunError;

use super::SessionCreationCompletion;
use crate::run_admission::preparation_error;
use crate::{
    ConfiguredSessionRepository, CreateSessionCommand, McpAttachmentCandidate,
    McpAttachmentCandidateTarget, SessionCreationCommands, SessionCreationError,
    SessionRepositoryOwner, SessionRepositoryResourceInput,
};

/// Protocol-independent request to create a Session from one published Agent
/// profile. The Session application resolves Environment, MCP, Resources,
/// credentials, tools, and immutable execution identity exactly once.
pub struct CreateProfiledSessionCommand {
    pub owner_scope: String,
    pub session_id: String,
    pub mutation_policy: crate::session_resource::SessionMutationPolicy,
    pub application_origin: Option<ApplicationOriginBinding>,
    pub agent_id: String,
    /// Exact executable publication source revision. `None` selects the current
    /// publication for ordinary interactive authoring.
    pub source_revision: Option<u64>,
    /// Exact Environment selected by the product at Session creation. The
    /// Session application resolves and freezes it without mutating the
    /// published Agent's authored defaults.
    pub environment_id: Option<String>,
    pub model: Option<String>,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
    /// Session-local typed attachments supplied by the product. They join the
    /// published Agent defaults inside this sole composer and therefore freeze
    /// in the original root rather than through a post-create Manifest write.
    pub resource_inputs: Vec<crate::session_resource::SessionInputAttachment>,
    /// Explicit Session candidates supplied by the product adapter. Published
    /// Agent candidates are joined and normalized inside the sole composer.
    pub mcp_candidates: Vec<McpAttachmentCandidate>,
    pub repositories: Vec<ProfiledSessionRepositoryInput>,
    pub network_restriction: Option<SessionNetworkPolicy>,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub tools: Option<SessionToolConfiguration>,
    pub idempotency: Option<awaken_session_state_contract::IdempotencyRecord>,
}

/// One product-authored Repository binding joined into the original profiled
/// Session root. `binding_id` is the caller's stable correlation identity;
/// `repository.id` remains Open's Session-owned Registry identity.
pub struct ProfiledSessionRepositoryInput {
    pub binding_id: awaken_resource_contract::BindingId,
    pub repository: SessionRepositoryResourceInput,
}

fn profiled_repository_attachment(
    repository_id: awaken_resource_contract::RepositoryId,
    mount_path: String,
    binding_id: awaken_resource_contract::BindingId,
) -> crate::session_resource::SessionInputAttachment {
    crate::session_resource::SessionInputAttachment {
        binding: awaken_resource_contract::InputBinding {
            binding_id,
            target: awaken_resource_contract::InputResourceId::Repository(repository_id),
            mount_path,
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        },
        replaces: None,
    }
}

fn preflight_profiled_resources(
    owner_scope: &str,
    session_id: &str,
    mutation_policy: crate::session_resource::SessionMutationPolicy,
    resource_inputs: &[crate::session_resource::SessionInputAttachment],
    repositories: &[ProfiledSessionRepositoryInput],
) -> Result<Vec<crate::session_resource::SessionInputAttachment>, RunError> {
    let repository_owner = SessionRepositoryOwner::profiled(session_id);
    for repository in repositories {
        if repository.binding_id.as_str().trim().is_empty()
            || repository.repository.workspace_id != owner_scope
            || !repository_owner.owns_repository_id(&repository.repository.id)
        {
            return Err(RunError::bad_request(
                "Profiled Session Repository binding is empty or outside its Session or Workspace",
            ));
        }
        if !mutation_policy.admits_repository_credential_mutation()
            && repository.repository.credential_material.is_some()
        {
            return Err(RunError::bad_request(
                "profiled Session Repository credentials must be pre-existing Vault references",
            ));
        }
    }

    let mut collision_preflight = resource_inputs.to_vec();
    collision_preflight.extend(repositories.iter().map(|repository| {
        profiled_repository_attachment(
            awaken_resource_contract::RepositoryId::from(repository.repository.id.clone()),
            repository.repository.mount_path.clone(),
            repository.binding_id.clone(),
        )
    }));
    crate::session_resource::SessionInputResolver::effective_bindings(&[], &collision_preflight)
        .map_err(|error| RunError::bad_request(error.to_string()))?;
    Ok(collision_preflight)
}

impl SessionCreationCommands {
    pub async fn create_profiled_session(
        &self,
        command: CreateProfiledSessionCommand,
    ) -> Result<awaken_session_state_contract::PersistedSession, SessionCreationError> {
        // Match the canonical Session-create boundary: the profiled authoring
        // graph is intentionally deep, so keep its state machine behind one
        // heap indirection instead of embedding it in every caller's Tokio
        // worker stack under the full workspace feature graph.
        Box::pin(self.create_profiled_session_with_completion(
            command,
            SessionCreationCompletion::AwaitRealization,
        ))
        .await
    }

    /// Accept one product-authored Session at the durable-root boundary. The
    /// original root remains the operation and lifecycle authority; the sole
    /// supervisor realizes it asynchronously and replays never repeat effects.
    pub async fn accept_profiled_session(
        &self,
        command: CreateProfiledSessionCommand,
    ) -> Result<awaken_session_state_contract::PersistedSession, SessionCreationError> {
        Box::pin(self.create_profiled_session_with_completion(
            command,
            SessionCreationCompletion::AcceptDurableRoot,
        ))
        .await
    }

    async fn create_profiled_session_with_completion(
        &self,
        command: CreateProfiledSessionCommand,
        completion: SessionCreationCompletion,
    ) -> Result<awaken_session_state_contract::PersistedSession, SessionCreationError> {
        let queries = &self.queries;
        let CreateProfiledSessionCommand {
            owner_scope,
            session_id,
            mutation_policy,
            application_origin,
            agent_id,
            source_revision,
            environment_id: requested_environment_id,
            model: requested_model,
            mounts,
            env,
            prompts,
            resource_inputs,
            mut mcp_candidates,
            repositories,
            network_restriction,
            title,
            metadata,
            tools: requested_tools,
            idempotency,
        } = command;
        SessionContextSeed::admit_authored_prompts(&prompts)
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        let context_seed =
            SessionContextSeed::from_metadata(SessionTrigger::Interactive {}, &metadata, None)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        let mut prompts = prompts;
        prompts.push(context_seed.encode());
        // Authored Repository paths and ownership are pure command facts. Reject
        // them before refreshing catalogs or opening any model, Environment,
        // MCP, Skill, File, Vault, Registry, Runtime, or Git dependency.
        let collision_preflight = preflight_profiled_resources(
            &owner_scope,
            &session_id,
            mutation_policy,
            &resource_inputs,
            &repositories,
        )?;
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(|error| {
                RunError::unavailable_classified(
                    "executable_projection_refresh_failed",
                    format!("Executable projections could not be refreshed: {error}"),
                )
            })?;
        let profile = admission
            .enter(|| queries.selected_session_profile(&owner_scope, &agent_id, source_revision));
        if let (Some(requested), Some(resolved)) = (source_revision, profile.as_ref())
            && !awaken_executable_agent_contract::requested_profile_revision_matches(
                resolved.source_revision,
                requested,
            )
        {
            return Err(RunError::bad_request(format!(
                "agent_version_mismatch: agent `{agent_id}` returned version {} for requested version {requested}",
                resolved.source_revision
            )).into());
        }
        if let Some(revision) = source_revision
            && profile.is_none()
        {
            return Err(RunError::bad_request(format!(
                "agent_version_unavailable: agent `{agent_id}` has no executable publication at version {revision}"
            )).into());
        }
        if source_revision.is_none() && queries.agent_unavailable(&owner_scope, &agent_id) {
            return Err(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            ))
            .into());
        }
        let agent_resources = profile
            .as_ref()
            .map(|profile| profile.resources.as_slice())
            .unwrap_or_default();
        let effective_bindings = crate::session_resource::SessionInputResolver::effective_bindings(
            agent_resources,
            &collision_preflight,
        )
        .map_err(|error| RunError::bad_request(error.to_string()))?;
        // Model-override cause/effect rules: R1 absent -> inherit the Agent
        // publication; R2 equal -> reuse it; R3 different + resolvable -> freeze
        // the complete replacement route; R4 different + invalid/unavailable ->
        // fail before persistence or external realization.
        let published_model = profile
            .as_ref()
            .and_then(|profile| profile.model.clone())
            .unwrap_or_else(|| self.model());
        let model_override = match requested_model.as_deref() {
            Some(requested) => Some(
                self.resolve_session_model_override(
                    &owner_scope,
                    requested,
                    &published_model,
                    Default::default(),
                )
                .await?,
            ),
            None => None,
        };
        let model = requested_model.clone().unwrap_or(published_model);
        let execution_model_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().identity().1.to_owned())
            .or_else(|| {
                profile
                    .as_ref()
                    .and_then(|profile| profile.execution_model_ref.clone())
            })
            .unwrap_or_else(|| model.clone());
        let published_backend_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().identity().2.to_owned())
            .or_else(|| {
                profile
                    .as_ref()
                    .map(|profile| profile.backend_ref.trim())
                    .filter(|backend| !backend.is_empty())
                    .map(str::to_owned)
            });
        let capabilities = queries.capabilities_for(&session_id)?;
        let capability_tools = SessionToolConfiguration::from_capabilities(&capabilities);
        let inherited_tools =
            crate::tool_compilation::inherit_profile_tools(capability_tools, profile.as_ref());
        let tools = requested_tools.unwrap_or(inherited_tools);
        let skills = profile
            .as_ref()
            .map(|profile| profile.skills.clone())
            .unwrap_or_default();
        self.validate_session_skill_total(&owner_scope, &agent_id, profile.as_ref(), &skills)?;
        mcp_candidates.extend(
            profile
                .as_ref()
                .into_iter()
                .flat_map(|profile| &profile.mcp_servers)
                .map(|server| {
                    let published_credential = match (
                        server.credential_source_id.as_ref(),
                        server.credential_revision,
                    ) {
                        (None, None) => Ok(None),
                        (Some(id), Some(revision)) if !id.trim().is_empty() && revision > 0 => {
                            Ok(Some((id.clone(), revision)))
                        }
                        _ => Err(RunError::bad_request(
                            "published MCP credential pin is incomplete",
                        )),
                    }?;
                    Ok(McpAttachmentCandidate {
                        name: server.name.clone(),
                        target: McpAttachmentCandidateTarget::Normalized(server.target.clone()),
                        prompts_as_skills: server.prompts_as_skills,
                        published_credential,
                        origin: McpAttachmentOrigin::Agent,
                    })
                })
                .collect::<Result<Vec<_>, RunError>>()?,
        );
        let (mcp_candidates, mcp_targets) =
            crate::SessionCredentials::normalize_mcp_candidate_targets(mcp_candidates)?;
        let mut environment = self
            .resolve_session_environment(
                requested_environment_id.as_deref(),
                profile
                    .as_ref()
                    .and_then(|profile| profile.environment.as_ref()),
                published_backend_ref.as_deref(),
                &mcp_targets,
            )
            .await?
            .snapshot;
        if let Some(restriction) = network_restriction {
            environment.network = environment.network.safe_intersection(&restriction);
        }
        // The same pure provider-effective Runtime gate used by Managed create,
        // live manifests, and the final root CAS runs before Profiled
        // Repository/Vault configuration or Skill/File materialization.
        self.validate_session_sandbox_layout(
            &session_id,
            &awaken_session_configuration_contract::SessionSandboxLayout {
                workspace_id: owner_scope.clone(),
                agent_id: agent_id.clone(),
                agent_revision: profile.as_ref().map(|profile| profile.source_revision),
                runtime_placement: self.runtime_placement(),
                model_override: model_override.clone(),
                runtime: published_backend_ref.clone(),
                mounts: mounts.clone(),
                env: env.clone(),
                environment: environment.clone(),
                resources: effective_bindings,
            },
        )?;
        let initial_mcp = self
            .credentials
            .normalize_mcp_drafts(
                &owner_scope,
                mcp_candidates,
                &[],
                &environment.credential_realization.mcp_holder,
            )
            .await?;
        let resolved_skills = if skills.is_empty() {
            None
        } else {
            Some(self.resolve_session_skills(&owner_scope, &skills).await?)
        };
        let mut repository_attachments = resource_inputs;
        repository_attachments.reserve(repositories.len());
        let mut expected_repository_credentials = BTreeMap::new();
        let mut repository_configurations = Vec::<ConfiguredSessionRepository>::new();
        for repository in repositories {
            let mount_path = repository.repository.mount_path.clone();
            let expected_credential = repository.repository.credential.clone();
            let binding_id = repository.binding_id;
            let configured = match self
                .repository_participants
                .configure(repository.repository)
                .await
            {
                Ok(configured) => configured,
                Err(first) => {
                    self.repository_participants.compensate(
    &session_id,
    &repository_configurations,
    "Profiled Session Repository compensation remains pending after configuration failure",
)
.await;
                    return Err(first.into());
                }
            };
            let repository_id = configured.repository_id.clone();
            expected_repository_credentials.insert(repository_id.clone(), expected_credential);
            repository_attachments.push(profiled_repository_attachment(
                repository_id,
                mount_path,
                binding_id,
            ));
            repository_configurations.push(configured);
        }
        let resources = match self
            .finalize_creation_inputs(
                &owner_scope,
                agent_resources,
                &repository_attachments,
                resolved_skills,
                &environment.credential_realization.resource_holder,
                |error| SessionCreationError::from(preparation_error(error)),
            )
            .await
        {
            Ok(resources) => resources,
            Err((first, stage)) => {
                self.repository_participants
                    .compensate(
                        &session_id,
                        &repository_configurations,
                        &format!(
                            "Profiled Session Repository compensation remains pending after {stage}"
                        ),
                    )
                    .await;
                return Err(first);
            }
        };
        for input in resources.inputs() {
            let crate::session_resource::ResolvedInputSource::Repository {
                repository_id,
                credential,
                ..
            } = &input.source
            else {
                continue;
            };
            let Some(expected) = expected_repository_credentials.get(repository_id) else {
                continue;
            };
            if credential.as_deref().map(|pin| &pin.access.credential) != expected.as_ref() {
                let first = RunError::bad_request(format!(
                    "repository `{repository_id}` credential revision changed before Session admission"
                ));
                self.repository_participants.compensate(
    &session_id,
    &repository_configurations,
    "Profiled Session Repository compensation remains pending after credential drift",
)
.await;
                return Err(first.into());
            }
        }
        let intent = SessionCreationIntent {
            control: ControlSessionCreationInputs {
                mutation_policy,
                application_origin,
                environment,
                runtime_placement: self.runtime_placement(),
                agent_id,
                agent_revision: profile.as_ref().map(|profile| profile.source_revision),
                model,
                execution_model_ref,
                model_override,
                system_prompt: SessionSystemPromptSelection::Inherit,
                runtime: published_backend_ref,
                mcp_authoring: SessionMcpAuthoringContext::default(),
                delegate_ids: profile
                    .as_ref()
                    .map(|profile| {
                        profile
                            .delegates
                            .iter()
                            .map(|delegate| delegate.agent_id.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                toolsets: tools.toolsets.clone(),
                mounts,
                env,
                prompts,
                transcript_prefix: None,
                resources,
                initial_mcp,
            },
        };
        self.create_session_with_completion(
            CreateSessionCommand {
                initial_agent_configuration: None,
                owner_scope,
                session_id,
                intent,
                title,
                metadata,
                tools,
                budget: awaken_session_budget_contract::SessionBudgetState::Absent,
                repository_configurations,
                idempotency,
                initial_events: None,
            },
            completion,
        )
        .await
    }
}
