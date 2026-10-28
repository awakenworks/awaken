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

use crate::SessionRootReads as _;
use awaken_time::WallClockExt as _;
use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_agent_contract::AgentSkillBinding;
use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_runtime_contract::agent_bindings::InferenceOptions;
use awaken_session_configuration_contract::{
    AgentCapabilities, SessionModelChoice, SessionModelSource, SessionSystemPromptSelection,
    SessionToolConfiguration, validate_mcp_toolset_pairing,
};
use awaken_session_state_contract::PersistedSession;
use awaken_session_types::RunError;

use crate::session_input::{self, SessionInputRequest};
use crate::{
    CreateSessionCommand, McpAttachmentCandidate, McpAttachmentCandidateTarget,
    SessionCreationCommands, SessionCreationError, SessionPreparationError,
};

/// A model the request names in place of the Agent publication's.
#[derive(Clone, Debug)]
pub struct SessionModelRequest {
    pub model_id: String,
    pub inference: InferenceOptions,
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
    /// Trusted application-origin projection; public protocol fields cannot set it.
    pub context_seed: Option<awaken_session_configuration_contract::SessionContextSeed>,
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

    /// Validate the fallible protocol projection of the already selected
    /// configuration. A wire adapter cannot rewrite the accepted inputs.
    fn validate_before_insert(
        &self,
        configuration: &awaken_session_configuration_contract::SessionAgentConfiguration,
    ) -> Result<(), RunError>;
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
pub(super) fn initial_mcp_candidates(
    profile: Option<&ExecutableAgentSessionProfile>,
    authored: Option<Vec<McpAttachmentCandidate>>,
) -> Result<Vec<McpAttachmentCandidate>, RunError> {
    match (authored, profile) {
        (Some(authored), _) => Ok(authored),
        (None, Some(profile)) => profile
            .mcp_servers
            .iter()
            .map(|server| {
                let published_credential = match (
                    server.credential_source_id.as_ref(),
                    server.credential_revision,
                ) {
                    (None, None) => None,
                    (Some(id), Some(revision)) if !id.trim().is_empty() && revision > 0 => {
                        Some((id.clone(), revision))
                    }
                    _ => {
                        return Err(RunError::bad_request(
                            "published MCP credential pin is incomplete",
                        ));
                    }
                };
                Ok(McpAttachmentCandidate {
                    name: server.name.clone(),
                    target: McpAttachmentCandidateTarget::Normalized(server.target.clone()),
                    prompts_as_skills: server.prompts_as_skills,
                    published_credential,
                    origin: crate::session_mcp::McpAttachmentOrigin::Agent,
                })
            })
            .collect(),
        (None, None) => Ok(Vec::new()),
    }
}

/// Capture each original direct publication and enforce the selected geography
/// once, before any later creation effects. The caller supplies its existing
/// exact/current catalog read law; no profile is reselected by wire projection.
fn capture_accepted_delegates(
    profile: Option<&ExecutableAgentSessionProfile>,
    selected_geo: Option<awaken_runtime_contract::agent_bindings::InferenceGeography>,
    mut resolve: impl FnMut(
        &awaken_executable_agent_contract::ExecutableAgentDelegate,
    ) -> Option<ExecutableAgentSessionProfile>,
) -> Result<crate::agent_configuration::CapturedDelegates, RunError> {
    crate::agent_configuration::capture_accepted_delegates(
        profile.map_or(&[], |profile| profile.delegates.as_slice()),
        |delegate_ref| {
            let delegate_id = &delegate_ref.agent_id;
            let delegate = resolve(delegate_ref).ok_or_else(|| {
                RunError::bad_request(format!(
                    "multiagent_unavailable: Agent `{delegate_id}` has no exact executable profile"
                ))
            })?;
            if delegate.inference.inference_geo != selected_geo {
                return Err(RunError::bad_request(format!(
                    "multiagent_inference_geo_mismatch: coordinator is {:?}, Agent `{delegate_id}` is {:?}",
                    selected_geo, delegate.inference.inference_geo
                )));
            }
            Ok(delegate)
        },
    )
}

fn compile_creation_tools<P: SessionCreateProtocol + ?Sized>(
    compiler: &dyn awaken_tool_contract::ToolConfigurationCompiler,
    profile: Option<&ExecutableAgentSessionProfile>,
    capabilities: &AgentCapabilities,
    protocol: &P,
    tools_override: Option<SessionToolsOverride>,
) -> Result<(SessionToolConfiguration, bool), RunError> {
    let capability_tools = protocol.capability_tools(capabilities)?;
    let inherited_tools = crate::tool_compilation::inherit_profile_tools(capability_tools, profile);
    let (tools, clears_all_tools) = tools_override.map_or((inherited_tools, false), |tools| {
        (tools.configuration, tools.clears_all)
    });
    let tools = crate::tool_compilation::compile_tool_settings(compiler, tools)?;
    Ok((tools, clears_all_tools))
}

impl SessionCreationCommands {
    /// Finish the same resolved Resource generation for both creation entries.
    /// Error presentation stays with the caller: only pinning has a distinct
    /// preparation error. The suffix is the original compensation diagnostic,
    /// never a domain classification or a new cleanup authority.
    pub(crate) async fn finalize_creation_inputs<E: From<RunError>>(
        &self,
        owner_scope: &str,
        agent_defaults: &[awaken_resource_contract::InputBinding],
        attachments: &[crate::session_resource::SessionInputAttachment],
        resolved_skills: Option<Vec<crate::session_resource::ResolvedSkillBinding>>,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        map_pin_error: impl FnOnce(SessionPreparationError) -> E,
    ) -> Result<crate::session_resource::ResolvedSessionResources, (E, &'static str)> {
        let mut resources = crate::resource_lowering::resolve_session_inputs(
            self.resource_registry.as_deref(),
            owner_scope,
            agent_defaults,
            attachments,
        )
        .await
        .map_err(|error| (E::from(error), "input resolution"))?;
        if let Some(skills) = resolved_skills {
            resources = resources.with_skills(skills).map_err(|error| {
                (
                    E::from(RunError::bad_request(error.to_string())),
                    "Skill binding",
                )
            })?;
        }
        self.credentials
            .pin_repository_credentials(owner_scope, selected_holder, &mut resources)
            .await
            .map_err(|error| (map_pin_error(error), "credential pinning"))?;
        Ok(resources)
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
    ) -> Result<PersistedSession, SessionCreateRequestError> {
        let (queries, input_service) = (&self.queries, &self.inputs);
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
            context_seed,
            budget: budget_request,
        } = request;
        let context_seed = context_seed
            .map_or_else(
                || {
                    awaken_session_configuration_contract::SessionContextSeed::from_metadata(
                        awaken_session_configuration_contract::SessionTrigger::Interactive {},
                        &metadata,
                        None,
                    )
                },
                Ok,
            )
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        session_input::admit_memory_inputs(&inputs)?;
        session_input::preflight_repository_mount_paths(&inputs)?;
        session_input::admit_file_count(&inputs)?;
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
        // Probe every minted candidate against both durable roots and Runtime
        // truth. The repository insert remains the final collision fence.
        let session_id = admission
            .enter(|| async {
                match explicit_id {
                    Some(id) => Ok(id),
                    None => loop {
                        let candidate = protocol.session_id_candidate();
                        let repository_absent = match queries
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
        let profile =
            queries.selected_session_profile(&owner_scope, &agent_id, requested_agent_version);
        if let Some(version) = requested_agent_version
            && profile.is_none()
        {
            let message = if queries.has_agent_profile_source() {
                format!(
                    "agent_version_unavailable: Agent `{agent_id}` version {version} is unavailable"
                )
            } else {
                format!(
                    "agent_version_unavailable: agent `{agent_id}` has no executable publication at version {version}"
                )
            };
            return Err(RunError::bad_request(message).into());
        }
        if profile.is_none()
            && requested_agent_version.is_none()
            && queries.agent_unavailable(&owner_scope, &agent_id)
        {
            return Err(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            ))
            .into());
        }
        let capabilities = queries.capabilities_for(&session_id)?;
        let (tools, clears_all_tools) = compile_creation_tools(
            self.tool_configuration_compiler.as_ref(),
            profile.as_ref(),
            &capabilities,
            protocol,
            tools_override,
        )?;
        let agent_defaults = profile
            .as_ref()
            .map(|profile| profile.resources.as_slice())
            .unwrap_or_default();
        let prepared_inputs = input_service
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
        let selected_geo = model
            .as_ref()
            .and_then(|model| model.inference.inference_geo);
        let mut accepted_delegates =
            capture_accepted_delegates(profile.as_ref(), selected_geo, |reference| {
                queries.selected_session_profile(
                    &owner_scope,
                    &reference.agent_id,
                    reference.source_revision,
                )
            })?;
        // The accepted capture is exactly the original direct declaration order;
        // reuse its ids before any bare Runtime-capability presentation is added.
        let delegate_ids = accepted_delegates
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        // Freeze the root execution route before any Resource or Session write.
        // For budgeted Sessions, the single roster compiler also validates that
        // the root, exact ordinary roster revisions, and the Advisor route all
        // execute inside the Native per-request gate.
        let default_model = self.model();
        let resolved_model_id = model
            .as_ref()
            .map_or_else(|| default_model.clone(), |model| model.model_id.clone());
        let (execution_model_ref, published_backend_ref) = super::baseline::creation_model_route(
            model_override.as_ref(),
            profile
                .as_ref()
                .and_then(|profile| profile.execution_model_ref.as_deref()),
            profile.as_ref().map(|profile| profile.backend_ref.as_str()),
            &resolved_model_id,
        );
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
        let (mcp_candidates, mcp_targets) =
            crate::SessionCredentials::normalize_mcp_candidate_targets(initial_mcp_candidates(
                profile.as_ref(),
                mcp_override,
            )?)?;
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
        if let Some(vault_id) = self
            .credentials
            .missing_vault(&owner_scope, &vault_ids)
            .await?
        {
            return Err(SessionCreateRequestError::VaultNotFound(vault_id));
        }
        let mcp_drafts = self
            .credentials
            .normalize_mcp_drafts(
                &owner_scope,
                mcp_candidates,
                &vault_ids,
                &environment.credential_realization.mcp_holder,
            )
            .await?;
        let skills =
            skills_override.or_else(|| profile.as_ref().map(|profile| profile.skills.clone()));
        crate::runtime_commands::validate_session_skill_total(
            &self.queries,
            &owner_scope,
            &agent_id,
            profile.as_ref(),
            skills.as_deref().unwrap_or_default(),
            &accepted_delegates,
        )?;
        let resolved_skills = match &skills {
            Some(skills) => Some(self.resolve_session_skills(&owner_scope, skills).await?),
            None => None,
        };
        // Validate the advertised tool surface before persisting an activation
        // or touching a Host. A definition error cannot strand Prepared inputs.
        if profile.is_none() {
            accepted_delegates = crate::agent_configuration::capture_capability_delegates(
                &capabilities,
                |id, revision| queries.selected_session_profile(&owner_scope, id, revision),
            );
        }
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
                awaken_agent_contract::ToolsetSource::Mcp { server_name } => {
                    Some(server_name.clone())
                }
                awaken_agent_contract::ToolsetSource::Agent => None,
            })
            .collect::<Vec<_>>();
        validate_mcp_toolset_pairing(&effective_server_names, &effective_toolset_names)
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
        let (attachments, repository_configurations) = prepared_inputs
            .lower(&self.repository_participants, &session_id, &owner_scope)
            .await?;
        // Sole Session input resolution point. Environment selection has
        // already produced one exact snapshot above; the creation intent is the
        // only value that combines and freezes both families.
        let resolved_inputs = match self
            .finalize_creation_inputs(
                &owner_scope,
                agent_defaults,
                &attachments,
                resolved_skills,
                &environment.credential_realization.resource_holder,
                SessionCreateRequestError::from,
            )
            .await
        {
            Ok(inputs) => inputs,
            Err((first, stage)) => {
                self.repository_participants
                    .compensate(
                        &session_id,
                        &repository_configurations,
                        &format!(
                            "Managed Session Repository compensation remains pending after {stage}"
                        ),
                    )
                    .await;
                return Err(first);
            }
        };
        let agent_revision = profile
            .as_ref()
            .and_then(|profile| (profile.source_revision > 0).then_some(profile.source_revision));
        let intent = awaken_session_configuration_contract::SessionCreationIntent {
            control: awaken_session_configuration_contract::ControlSessionCreationInputs {
                mutation_policy: crate::session_resource::SessionMutationPolicy::Managed,
                application_origin: None,
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
                    None => SessionSystemPromptSelection::Inherit,
                    Some(None) => SessionSystemPromptSelection::Clear,
                    Some(Some(value)) => SessionSystemPromptSelection::Replace(value),
                },
                runtime: published_backend_ref,
                delegate_ids,
                toolsets: tools.toolsets.clone(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: vec![context_seed.encode()],
                transcript_prefix: None,
                resources: resolved_inputs,
                initial_mcp: mcp_drafts,
            },
        };
        // Finish every fallible protocol projection before the durable root
        // insert: after the root exists the protocol only caches it.
        let mut initial_agent_configuration =
            awaken_session_configuration_contract::SessionAgentConfiguration::creation_presentation_seed(
                &intent.control,
                agent_version,
                model,
                (
                    profile.as_ref().and_then(|value| value.name.as_deref()),
                    profile.as_ref().and_then(|value| value.description.as_deref()),
                ),
                profile.as_ref().and_then(|value| value.system.clone()),
                &tools,
                skills.unwrap_or_else(|| {
                    capabilities.skills.iter().map(AgentSkillBinding::custom).collect()
                }),
            );
        initial_agent_configuration.multiagent =
            crate::agent_configuration::resolve_session_multiagent(
                profile.as_ref(),
                &capabilities,
                Some(accepted_delegates),
                |agent_id, revision| {
                    queries.selected_session_profile(&owner_scope, agent_id, revision)
                },
            )?
            .0;
        protocol.validate_before_insert(&initial_agent_configuration)?;
        let command = CreateSessionCommand {
            initial_agent_configuration: Some(Box::new(initial_agent_configuration)),
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
        Ok(session)
    }
}

#[cfg(test)]
mod candidate_tests {
    use super::*;

    #[test]
    fn managed_delegate_capture_preserves_geography_errors_and_first_refusal_order() {
        // M1 first missing => original missing error, no second read; M2 first
        // geography mismatch then second missing => geo wins, no second read;
        // M3 matching first then mismatched second => second geo error;
        // M4 both match => original declaration order. Shared capture selects
        // no policy: this original Managed wrapper still owns every check.
        use awaken_runtime_contract::agent_bindings::InferenceGeography::Us;
        for (first, second, code, calls) in [
            (None, Some(Some(Us)), Some("multiagent_unavailable"), 1),
            (
                Some(None),
                None,
                Some("multiagent_inference_geo_mismatch"),
                1,
            ),
            (
                Some(Some(Us)),
                Some(None),
                Some("multiagent_inference_geo_mismatch"),
                2,
            ),
            (Some(Some(Us)), Some(Some(Us)), None, 2),
        ] {
            let profile = ExecutableAgentSessionProfile {
                delegates: ["first", "second"]
                    .into_iter()
                    .map(
                        |agent_id| awaken_executable_agent_contract::ExecutableAgentDelegate {
                            agent_id: agent_id.into(),
                            source_revision: Some(7),
                        },
                    )
                    .collect(),
                ..Default::default()
            };
            let mut reads = Vec::new();
            let result = capture_accepted_delegates(Some(&profile), Some(Us), |reference| {
                reads.push(reference.agent_id.clone());
                assert_eq!(reference.source_revision, Some(7));
                (if reference.agent_id == "first" {
                    first
                } else {
                    second
                })
                .map(|inference_geo| ExecutableAgentSessionProfile {
                    source_revision: 7,
                    inference: awaken_runtime_contract::agent_bindings::InferenceOptions {
                        inference_geo,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            });
            assert_eq!(reads, ["first", "second"][..calls]);
            if let Some(code) = code {
                let message = result.unwrap_err().message;
                let id = if calls == 1 { "first" } else { "second" };
                let expected = if code == "multiagent_unavailable" {
                    format!("multiagent_unavailable: Agent `{id}` has no exact executable profile")
                } else {
                    format!(
                        "multiagent_inference_geo_mismatch: coordinator is Some(Us), Agent `{id}` is None"
                    )
                };
                assert_eq!(message, expected);
            } else {
                assert_eq!(
                    result
                        .unwrap()
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect::<Vec<_>>(),
                    reads
                );
            }
        }
    }

    #[test]
    fn initial_mcp_candidates_keep_complete_pins_and_explicit_replacement_authority() {
        // One shared lowering owns Managed inheritance and Profiled union.
        // Cause/effect table: no pin -> inherit unpinned; both nonempty source
        // and positive revision -> inherit exact pin; partial/blank/zero pin
        // -> reject, never silently drop its credential. An explicit authored
        // replacement (including empty) suppresses inherited candidates and
        // does not validate unused publication pins. Cross all seven pin rows
        // with inheritance versus full replacement; absent profile -> empty.
        for (source, revision, valid) in [
            (None, None, true),
            (Some("source"), Some(1), true),
            (Some("source"), None, false),
            (None, Some(1), false),
            (Some(""), Some(1), false),
            (Some(" "), Some(1), false),
            (Some("source"), Some(0), false),
        ] {
            let profile = ExecutableAgentSessionProfile {
                mcp_servers: vec![awaken_executable_agent_contract::ExecutableAgentMcpServer {
                    name: "published".into(),
                    target: awaken_agent_contract::McpTarget::parse_http(
                        "https://example.test/mcp",
                    )
                    .unwrap(),
                    prompts_as_skills: false,
                    credential_source_id: source.map(str::to_owned),
                    credential_revision: revision,
                }],
                ..Default::default()
            };
            let inherited = initial_mcp_candidates(Some(&profile), None);
            assert_eq!(inherited.is_ok(), valid);
            if valid {
                let candidates = inherited.unwrap();
                assert_eq!(candidates.len(), 1);
                assert_eq!(
                    candidates[0].published_credential,
                    source.map(str::to_owned).zip(revision)
                );
                assert_eq!(
                    candidates[0].origin,
                    crate::session_mcp::McpAttachmentOrigin::Agent
                );
            } else {
                assert!(
                    inherited
                        .unwrap_err()
                        .message
                        .contains("credential pin is incomplete")
                );
            }
            assert!(
                initial_mcp_candidates(Some(&profile), Some(Vec::new()))
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(initial_mcp_candidates(None, None).unwrap().is_empty());
    }
}
