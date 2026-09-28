// @formal surface exclude=application_adapter
// @formal owner crates/server/awaken-session-application/src/creation.rs
// @formal reason: Orchestrates one protocol-neutral Session create request through existing application
// @formal reason: steps (profile, model, Environment, inputs, MCP, skills, tools, budget, Repository
// @formal reason: participants) in the fixed validation order, then hands the complete command to the
// @formal reason: one durable creation driver. It owns no root CAS, realization or retry of its own.
//! One Session create request, from protocol-neutral inputs to a durable root.
//!
//! A protocol lowers its wire request into a [`SessionCreateRequest`] and
//! supplies the few rules that stay protocol-owned (its public id spelling,
//! its initial Event compilation, its model admission policy, how served
//! capabilities read as tools, and its fallible projection) through
//! [`SessionCreateProtocol`]. Everything else — the order of every check, which
//! Agent publication and model are frozen, the input plan, Repository
//! participants and their compensation, budget pricing, and whether the root
//! is accepted or awaited — is decided here once for every protocol.

use awaken_lease_contract::WallClockExt as _;
use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_agent_contract::AgentSkillBinding;
use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_runtime_contract::agent_bindings::InferenceOptions;
use awaken_session_configuration_contract::{AgentCapabilities, SessionToolConfiguration};
use awaken_session_error_contract::RunError;
use awaken_session_state_contract::PersistedSession;

use super::session_input::{self, SessionInputRequest};
use super::{
    CreateSessionCommand, McpAttachmentCandidate, McpAttachmentCandidateTarget, SessionApplication,
    SessionCreationError, SessionPreparationError,
};

/// A model the request names in place of the Agent publication's.
#[derive(Clone, Debug)]
pub struct SessionModelRequest {
    pub model_id: String,
    pub inference: InferenceOptions,
}

/// Where the frozen model choice came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionModelSource {
    /// The request's own override.
    Override,
    /// The Agent publication's model and inference controls.
    Publication,
}

/// The model a new Session freezes, when one was selected.
#[derive(Clone, Debug)]
pub struct SessionModelChoice {
    pub model_id: String,
    pub inference: InferenceOptions,
    pub source: SessionModelSource,
}

/// A request-local replacement of the inherited tool configuration.
#[derive(Clone, Debug)]
pub struct SessionToolsOverride {
    pub configuration: SessionToolConfiguration,
    /// The request authored an empty tool list, clearing every tool.
    pub clears_all: bool,
}

/// Everything a protocol lowers from one wire Session create request.
///
/// Consumer: [`SessionApplication::create_requested_session`].
pub struct SessionCreateRequest {
    pub owner_scope: String,
    /// A caller-chosen id (deterministic replay); otherwise one is minted.
    pub explicit_id: Option<String>,
    pub idempotency: Option<awaken_session_state_contract::IdempotencyRecord>,
    pub agent_id: String,
    /// The exact Agent publication revision the request pins.
    pub agent_version: Option<u64>,
    pub model_override: Option<SessionModelRequest>,
    /// `Some(None)` clears the system prompt; `Some(Some(_))` replaces it.
    pub system_override: Option<Option<String>>,
    pub tools_override: Option<SessionToolsOverride>,
    pub skills_override: Option<Vec<AgentSkillBinding>>,
    /// MCP servers authored in place of the publication's.
    pub mcp_override: Option<Vec<McpAttachmentCandidate>>,
    pub environment_id: String,
    pub inputs: Vec<SessionInputRequest>,
    pub vault_ids: Vec<String>,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub budget: Option<SessionBudgetRequest>,
}

/// An authored Session budget.
#[derive(Clone, Debug)]
pub enum SessionBudgetRequest {
    /// A list-cost limit in minor currency units.
    MaxListCostMinor(u64),
    /// A malformed limit, rejected at the budget stage of creation after
    /// every earlier admission check.
    Invalid(String),
}

/// The rules of Session creation a protocol keeps.
///
/// Consumer: [`SessionApplication::create_requested_session`], which calls
/// each at its one fixed point in the creation order.
#[async_trait]
pub trait SessionCreateProtocol: Send + Sync {
    /// The protocol's projection of the new Session, finished before the
    /// durable root insert so a projection failure strands nothing.
    type Projection: Send;

    /// One fresh candidate in the protocol's public Session id spelling. The
    /// application probes each candidate against durable truth.
    fn session_id_candidate(&self) -> String;

    /// Compile the request's initial Events for the minted Session id.
    fn initial_event_plan(
        &self,
        session_id: &str,
    ) -> Result<Option<awaken_session_event_contract::SessionInitialEventPlan>, RunError>;

    /// Admit the selected model under the protocol's placement policy.
    async fn admit_model(
        &self,
        owner_scope: &str,
        model: Option<&SessionModelChoice>,
    ) -> Result<(), RunError>;

    /// Validate the served capabilities against the protocol's tool rules and
    /// read them as the tool configuration a Session without a published
    /// toolset inherits.
    fn capability_tools(
        &self,
        capabilities: &AgentCapabilities,
    ) -> Result<SessionToolConfiguration, RunError>;

    /// Finish every fallible protocol projection of the Session.
    fn project_before_insert(
        &self,
        owner_scope: &str,
        profile: Option<&ExecutableAgentSessionProfile>,
        capabilities: &AgentCapabilities,
        tools: &SessionToolConfiguration,
    ) -> Result<Self::Projection, RunError>;
}

/// A durably created Session and what its protocol echoes.
pub struct CreatedSession<P> {
    pub session: PersistedSession,
    pub session_id: String,
    /// The Agent publication frozen into the Session, if one exists.
    pub profile: Option<ExecutableAgentSessionProfile>,
    /// The Agent version the Session answers to (at least 1).
    pub agent_version: u64,
    pub model: Option<SessionModelChoice>,
    /// The Runtime's default model, frozen when nothing selected one.
    pub default_model: String,
    pub environment_id: String,
    /// The effective skill bindings, when the request or publication names any.
    pub skills: Option<Vec<AgentSkillBinding>>,
    pub capabilities: AgentCapabilities,
    pub tools: SessionToolConfiguration,
    pub projection: P,
}

/// Why a Session create request was not created.
#[derive(Debug, thiserror::Error)]
pub enum SessionCreateRequestError {
    #[error(transparent)]
    Rejected(#[from] RunError),
    /// A referenced vault does not exist.
    #[error("vault `{0}` does not exist")]
    VaultNotFound(String),
    #[error(transparent)]
    Repository(#[from] awaken_session_state_contract::SessionRepositoryError),
    #[error(transparent)]
    Preparation(#[from] SessionPreparationError),
    #[error(transparent)]
    Creation(#[from] SessionCreationError),
}

/// Preserve every create-time authoring candidate and its actual source. The
/// Session aggregate, not array order or credential presence, resolves logical
/// name precedence and target conflicts after canonical normalization.
fn initial_mcp_candidates(
    profile: Option<&ExecutableAgentSessionProfile>,
    authored: Option<Vec<McpAttachmentCandidate>>,
) -> Vec<McpAttachmentCandidate> {
    match (authored, profile) {
        (Some(authored), _) => authored,
        (None, Some(profile)) => profile
            .mcp_servers
            .iter()
            .map(|server| McpAttachmentCandidate {
                name: server.name.clone(),
                target: McpAttachmentCandidateTarget::Normalized(server.target.clone()),
                prompts_as_skills: server.prompts_as_skills,
                published_credential: server
                    .credential_source_id
                    .clone()
                    .zip(server.credential_revision),
                origin: awaken_session_mcp_contract::McpAttachmentOrigin::Agent,
            })
            .collect(),
        (None, None) => Vec::new(),
    }
}

impl SessionApplication {
    /// Retire the Repository participants a failed request configured; a
    /// compensation that cannot finish is left to terminal reconciliation.
    async fn compensate_request_repositories(
        &self,
        session_id: &str,
        configurations: &[super::ConfiguredSessionRepository],
        stage: &str,
    ) {
        if !self
            .abort_unadopted_session_repositories(configurations)
            .await
        {
            tracing::warn!(
                session = %session_id,
                "Managed Session Repository compensation remains pending after {stage}"
            );
        }
    }

    /// Create one Session from a protocol-neutral request.
    ///
    /// Input admission is the first domain operation: the complete explicit
    /// input set is validated before any catalog is refreshed or any model,
    /// Environment, MCP, Skill, File, Vault, Registry, Runtime or Git port is
    /// entered. Repository participants configured before a later failure are
    /// compensated. A coordinator-realized placement accepts the root as soon
    /// as it is durable; every other placement awaits realization.
    #[allow(clippy::too_many_lines)]
    pub async fn create_requested_session<P: SessionCreateProtocol + ?Sized>(
        &self,
        request: SessionCreateRequest,
        protocol: &P,
    ) -> Result<CreatedSession<P::Projection>, SessionCreateRequestError> {
        let SessionCreateRequest {
            owner_scope,
            explicit_id,
            idempotency,
            agent_id,
            agent_version: requested_agent_version,
            model_override: requested_model,
            system_override,
            tools_override,
            skills_override,
            mcp_override,
            environment_id: requested_environment_id,
            inputs,
            vault_ids,
            title,
            metadata,
            budget: budget_request,
        } = request;
        session_input::admit_memory_inputs(&inputs)?;
        session_input::preflight_repository_mount_paths(&inputs)?;
        session_input::admit_file_count(&inputs)?;
        let admission = self
            .refresh_executable_projections()
            .await
            .map_err(|error| {
                RunError::unavailable_classified(
                    "executable_projection_refresh_failed",
                    format!("Executable projections could not be refreshed: {error}"),
                )
            })?;
        // Probe every minted candidate against both durable roots and Runtime
        // truth. The repository insert remains the final collision fence.
        let session_id = admission
            .enter(|| async {
                match explicit_id {
                    Some(id) => Ok(id),
                    None => loop {
                        let candidate = protocol.session_id_candidate();
                        let repository_absent = match self
                            .session(&awaken_session_types::SessionId::from(&candidate))
                            .await
                        {
                            Err(
                                awaken_session_state_contract::SessionRepositoryError::NotFound,
                            ) => true,
                            Ok(_) => false,
                            Err(error) => return Err(SessionCreateRequestError::from(error)),
                        };
                        if !self.runtime_owns_thread(&candidate).await? && repository_absent {
                            break Ok(candidate);
                        }
                    },
                }
            })
            .await?;
        let initial_events = protocol.initial_event_plan(&session_id)?;
        let profile = requested_agent_version
            .and_then(|version| self.session_profile_at_revision(&owner_scope, &agent_id, version))
            .or_else(|| {
                requested_agent_version
                    .is_none()
                    .then(|| self.session_profile(&owner_scope, &agent_id))
                    .flatten()
            });
        if let Some(version) = requested_agent_version
            && profile.is_none()
            && self.has_agent_profile_source()
        {
            return Err(RunError::bad_request(format!(
                "agent_version_unavailable: Agent `{agent_id}` version {version} is unavailable"
            ))
            .into());
        }
        if let Some(version) = requested_agent_version
            && profile.is_none()
        {
            return Err(RunError::bad_request(format!(
                "agent_version_unavailable: agent `{agent_id}` has no executable publication at version {version}"
            ))
            .into());
        }
        if profile.is_none()
            && requested_agent_version.is_none()
            && self.agent_unavailable(&owner_scope, &agent_id)
        {
            return Err(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            ))
            .into());
        }
        let agent_defaults = profile
            .as_ref()
            .map(|profile| profile.resources.as_slice())
            .unwrap_or_default();
        let prepared_inputs = self
            .prepare_session_inputs(&session_id, &owner_scope, &inputs, agent_defaults)
            .await?;
        // Cause/effect decision table for Session model authority: R1 absent ->
        // inherit the Agent publication; R2 equal override -> reuse its route
        // and replace inference controls; R3 different override + resolver ->
        // freeze the complete newly resolved publication; R4 different override
        // + unavailable/invalid resolver -> fail before Session persistence. A
        // public id is never stitched onto the Agent's old backend, endpoint, or
        // credential provisioning.
        let published_model = profile
            .as_ref()
            .and_then(|profile| profile.model.clone())
            .unwrap_or_else(|| self.model());
        let (model, model_override) = match requested_model {
            Some(requested) => {
                let model_override = self
                    .resolve_session_model_override(
                        &owner_scope,
                        &requested.model_id,
                        &published_model,
                        requested.inference.clone(),
                    )
                    .await?;
                (
                    Some(SessionModelChoice {
                        model_id: requested.model_id,
                        inference: requested.inference,
                        source: SessionModelSource::Override,
                    }),
                    Some(model_override),
                )
            }
            None => (
                profile.as_ref().and_then(|profile| {
                    profile.model.clone().map(|model_id| SessionModelChoice {
                        model_id,
                        inference: profile.inference.clone(),
                        source: SessionModelSource::Publication,
                    })
                }),
                None,
            ),
        };
        protocol.admit_model(&owner_scope, model.as_ref()).await?;
        let agent_version = requested_agent_version
            .or_else(|| profile.as_ref().map(|profile| profile.source_revision))
            .unwrap_or(1)
            .max(1);
        let delegate_ids = profile
            .as_ref()
            .map(|profile| {
                profile
                    .delegates
                    .iter()
                    .map(|delegate| delegate.agent_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let selected_geo = model
            .as_ref()
            .and_then(|model| model.inference.inference_geo);
        for delegate_ref in profile
            .as_ref()
            .into_iter()
            .flat_map(|profile| profile.delegates.iter())
        {
            let delegate_id = &delegate_ref.agent_id;
            let delegate = delegate_ref
                .source_revision
                .and_then(|revision| {
                    self.session_profile_at_revision(&owner_scope, delegate_id, revision)
                })
                .or_else(|| self.session_profile(&owner_scope, delegate_id))
                .ok_or_else(|| {
                    RunError::bad_request(format!(
                        "multiagent_unavailable: Agent `{delegate_id}` has no executable profile"
                    ))
                })?;
            if delegate.inference.inference_geo != selected_geo {
                return Err(RunError::bad_request(format!(
                    "multiagent_inference_geo_mismatch: coordinator is {:?}, Agent `{delegate_id}` is {:?}",
                    selected_geo, delegate.inference.inference_geo
                ))
                .into());
            }
        }
        // Freeze the root execution route before any Resource or Session write.
        // For budgeted Sessions, the single roster compiler also validates that
        // the root, exact ordinary roster revisions, and the Advisor route all
        // execute inside the Native per-request gate.
        let default_model = self.model();
        let resolved_model_id = model
            .as_ref()
            .map_or_else(|| default_model.clone(), |model| model.model_id.clone());
        let execution_model_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().identity().1.to_owned())
            .or_else(|| {
                profile
                    .as_ref()
                    .and_then(|profile| profile.execution_model_ref.clone())
            })
            .unwrap_or_else(|| resolved_model_id.clone());
        let published_backend_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().identity().2.to_owned())
            .or_else(|| {
                profile
                    .as_ref()
                    .map(|profile| profile.backend_ref.clone())
                    .filter(|backend_ref| !backend_ref.trim().is_empty())
            });
        let budget_model_refs = budget_request
            .as_ref()
            .map(|_| {
                self.managed_session_model_refs(
                    &owner_scope,
                    &agent_id,
                    profile.as_ref(),
                    &execution_model_ref,
                    published_backend_ref.as_deref(),
                    model_override
                        .as_ref()
                        .and_then(|model_override| model_override.publication.as_deref()),
                )
            })
            .transpose()?;
        let (mcp_candidates, mcp_targets) = Self::normalize_mcp_candidate_targets(
            initial_mcp_candidates(profile.as_ref(), mcp_override),
        )?;
        // Resolve the Environment and networking policy once, for both the
        // creation intent and the protocol's echo. No local-environment
        // fallback is invoked on behalf of an explicit selection.
        let environment = self
            .resolve_session_environment(
                Some(&requested_environment_id),
                profile
                    .as_ref()
                    .and_then(|profile| profile.environment.as_ref()),
                published_backend_ref.as_deref(),
                &mcp_targets,
            )
            .await?
            .snapshot;
        let environment_id = environment.environment_id.clone();
        let coordinator_realized = awaken_session_state_contract::SessionPlacementClass::new(
            self.runtime_placement(),
            environment.self_hosted,
        )
        .realized_by_coordinator();
        // Cause/effect layout gate: C1 the complete effective typed bindings,
        // frozen Environment, and exact publication-selected provider produce a
        // non-overlapping final Sandbox layout -> continue; C2 any projected
        // File/Memory/Repository, baseline, output, HOME/XDG, or provider mount
        // overlaps -> reject before Vault checks, Skill/File materialization,
        // Repository configuration, root CAS, cache prewarm, or provider I/O.
        self.validate_session_sandbox_layout(
            &session_id,
            &awaken_session_configuration_contract::SessionSandboxLayout {
                workspace_id: owner_scope.clone(),
                agent_id: agent_id.clone(),
                agent_revision: profile.as_ref().and_then(|profile| {
                    (profile.source_revision > 0).then_some(profile.source_revision)
                }),
                runtime_placement: self.runtime_placement(),
                model_override: model_override.clone(),
                runtime: published_backend_ref.clone(),
                mounts: Vec::new(),
                env: Vec::new(),
                environment: environment.clone(),
                resources: prepared_inputs.effective_bindings().to_vec(),
            },
        )?;
        if let Some(vault_id) = self.missing_vault(&owner_scope, &vault_ids).await? {
            return Err(SessionCreateRequestError::VaultNotFound(vault_id));
        }
        let mcp_drafts = self
            .normalize_mcp_drafts(
                &owner_scope,
                mcp_candidates,
                &vault_ids,
                &environment.credential_realization.mcp_holder,
            )
            .await?;
        let skills =
            skills_override.or_else(|| profile.as_ref().map(|profile| profile.skills.clone()));
        self.validate_session_skill_total(
            &owner_scope,
            &agent_id,
            profile.as_ref(),
            skills.as_deref().unwrap_or_default(),
        )?;
        let resolved_skills = match &skills {
            Some(skills) => Some(self.resolve_session_skills(&owner_scope, skills).await?),
            None => None,
        };
        // Validate the advertised tool surface before persisting an activation
        // or touching a Host. A definition error cannot strand Prepared inputs.
        let capabilities = self.capabilities_for(&session_id);
        let capability_tools = protocol.capability_tools(&capabilities)?;
        let inherited_tools = profile.as_ref().map_or_else(
            || capability_tools.clone(),
            |profile| SessionToolConfiguration {
                toolsets: if profile.toolsets.is_empty() {
                    capability_tools.toolsets.clone()
                } else {
                    profile.toolsets.clone()
                },
                client_tools: profile.client_tools.clone(),
            },
        );
        let clears_all_tools = tools_override
            .as_ref()
            .is_some_and(|tools| tools.clears_all);
        let tools = tools_override
            .map(|tools| tools.configuration)
            .unwrap_or(inherited_tools);
        let skills_configured = skills
            .as_ref()
            .map_or(!capabilities.skills.is_empty(), |skills| !skills.is_empty());
        if clears_all_tools && skills_configured {
            return Err(
                RunError::bad_request("cannot clear tools while skills are configured").into(),
            );
        }
        // Validate after replacement/inheritance has produced the effective
        // configuration: checking only authored overrides admitted a partial
        // override whose inherited half named a different server.
        let effective_server_names = mcp_drafts
            .iter()
            .map(|draft| draft.name.clone())
            .collect::<Vec<_>>();
        let effective_toolset_names = tools
            .toolsets
            .iter()
            .filter_map(|toolset| match &toolset.source {
                awaken_runtime_contract::agent_bindings::ToolsetSource::Mcp { server_name } => {
                    Some(server_name.clone())
                }
                awaken_runtime_contract::agent_bindings::ToolsetSource::Agent => None,
            })
            .collect::<Vec<_>>();
        awaken_session_configuration_contract::validate_mcp_toolset_pairing(
            &effective_server_names,
            &effective_toolset_names,
        )
        .map_err(RunError::bad_request)?;
        let budget = match budget_request {
            Some(budget_request) => {
                let max_list_cost_minor = match budget_request {
                    SessionBudgetRequest::MaxListCostMinor(minor) => minor,
                    SessionBudgetRequest::Invalid(message) => {
                        return Err(RunError::bad_request(message).into());
                    }
                };
                let model_refs =
                    budget_model_refs.expect("a budgeted Session compiled its exact model roster");
                let snapshot = self
                    .resolve_managed_list_price_snapshot(
                        awaken_session_budget_contract::ManagedListPriceRequest {
                            occurred_at_unix_ms: self.clock.now_ms(),
                            model_refs,
                        },
                    )
                    .await
                    .map_err(|error| match error {
                        awaken_session_budget_contract::ManagedListPriceError::Unavailable(_) => {
                            RunError::unavailable(error.to_string())
                        }
                        _ => RunError::bad_request(error.to_string()),
                    })?;
                awaken_session_budget_contract::SessionBudgetState::active(
                    max_list_cost_minor,
                    snapshot,
                )
            }
            None => awaken_session_budget_contract::SessionBudgetState::Absent,
        };
        // Cause/effect rules for Session-owned Repository participants:
        // R1 all pure Environment/Skill/tool/budget validation succeeds ->
        // enter Registry/Vault; R2 lowering fails -> compensate only prior
        // Applied participants; R3 later resolve/pin fails -> compensate every
        // Applied participant; R4 root Applied/Replayed -> creation owns the
        // participants and terminal reconciliation becomes the only retire path.
        let (attachments, repository_configurations) = self
            .lower_prepared_session_inputs(&session_id, &owner_scope, &prepared_inputs)
            .await?;
        // Sole Session input resolution point. Environment selection has
        // already produced one exact snapshot above; the creation intent is the
        // only value that combines and freezes both families.
        let mut resolved_inputs = match self
            .resolve_session_inputs(&owner_scope, agent_defaults, &attachments)
            .await
        {
            Ok(inputs) => inputs,
            Err(first) => {
                self.compensate_request_repositories(
                    &session_id,
                    &repository_configurations,
                    "input resolution",
                )
                .await;
                return Err(first.into());
            }
        };
        if let Some(resolved_skills) = resolved_skills {
            resolved_inputs = match resolved_inputs.with_skills(resolved_skills) {
                Ok(inputs) => inputs,
                Err(error) => {
                    let first = RunError::bad_request(error.to_string());
                    self.compensate_request_repositories(
                        &session_id,
                        &repository_configurations,
                        "Skill binding",
                    )
                    .await;
                    return Err(first.into());
                }
            };
        }
        if let Err(error) = self
            .pin_repository_credentials(
                &owner_scope,
                &environment.credential_realization.resource_holder,
                &mut resolved_inputs,
            )
            .await
        {
            self.compensate_request_repositories(
                &session_id,
                &repository_configurations,
                "credential pinning",
            )
            .await;
            return Err(error.into());
        }
        let agent_revision = profile
            .as_ref()
            .and_then(|profile| (profile.source_revision > 0).then_some(profile.source_revision));
        let intent = awaken_session_configuration_contract::SessionCreationIntent {
            control: awaken_session_configuration_contract::ControlSessionCreationInputs {
                mutation_policy: awaken_session_resource_contract::SessionMutationPolicy::Managed,
                environment,
                runtime_placement: self.runtime_placement(),
                mcp_authoring: awaken_session_configuration_contract::SessionMcpAuthoringContext {
                    ordered_vault_ids: vault_ids,
                },
                agent_id: agent_id.clone(),
                agent_revision,
                model: resolved_model_id,
                execution_model_ref,
                model_override,
                system_prompt: match system_override {
                    None => {
                        awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit
                    }
                    Some(None) => {
                        awaken_session_configuration_contract::SessionSystemPromptSelection::Clear
                    }
                    Some(Some(value)) => {
                        awaken_session_configuration_contract::SessionSystemPromptSelection::Replace(
                            value,
                        )
                    }
                },
                runtime: published_backend_ref,
                delegate_ids,
                toolsets: tools.toolsets.clone(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
                resources: resolved_inputs,
                initial_mcp: mcp_drafts,
            },
        };
        // Finish every fallible protocol projection before the durable root
        // insert: after the root exists the protocol only caches it.
        let projection = protocol.project_before_insert(
            &owner_scope,
            profile.as_ref(),
            &capabilities,
            &tools,
        )?;
        let command = CreateSessionCommand {
            owner_scope,
            session_id: session_id.clone(),
            intent,
            title,
            metadata,
            tools: tools.clone(),
            budget,
            repository_configurations,
            idempotency,
            initial_events,
        };
        let session = if coordinator_realized {
            Box::pin(self.accept_session(command)).await
        } else {
            Box::pin(self.create_session(command)).await
        }?;
        Ok(CreatedSession {
            session,
            session_id,
            profile,
            agent_version,
            model,
            default_model,
            environment_id,
            skills,
            capabilities,
            tools,
            projection,
        })
    }
}
