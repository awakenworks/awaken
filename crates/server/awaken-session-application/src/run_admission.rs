// @formal entrypoint agent_profile.requested_revision_never_follows_mutable_current
// @formal entrypoint executable_projection.failed_refresh_never_enters_handler
// @formal entrypoint session_creation.network_intersection_is_canonical_and_non_widening
// @formal entrypoint session_creation.selected_environment_is_frozen_in_baseline
// @formal entrypoint session_event_batch.run_reservation_precedes_distinct_activity_receipt
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_run.reservation_precedes_activity
//! Session-owned admission in front of the neutral Run application port.
//!
//! Runtime Host executes an already-admitted Run. Session creation/recovery is
//! application policy, so it is configured here once for every public protocol.

use crate::{SessionProjectionReads as _, SessionRootReads as _};
use awaken_session_realization_contract::SessionRealizationDriveError;
use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_session_application_contract::{
    OwnerScopedRunApplication, Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};
use awaken_session_configuration_contract::mcp::McpAttachmentOrigin;
use awaken_session_configuration_contract::{
    ApplicationOriginBinding, ControlSessionCreationInputs, SessionContextSeed,
    SessionCreationIntent, SessionMcpAuthoringContext, SessionNetworkPolicy,
    SessionSystemPromptSelection, SessionToolConfiguration, SessionTrigger,
};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivityReceiptPlan, SessionRunDelivery,
    session_run_activity_operation_id, session_run_activity_receipt_plan,
};
use awaken_session_state_contract::SessionRepositoryError;
use awaken_session_types::RunError;
use awaken_session_types::SessionRevision;

use crate::creation::SessionCreationCompletion;
use crate::{
    ConfiguredSessionRepository, CreateSessionCommand, McpAttachmentCandidate,
    McpAttachmentCandidateTarget, SessionApplication, SessionCreationError, SessionMutationError,
    SessionPreparationError, SessionRepositoryOwner, SessionRepositoryResourceInput,
    session_repository_error,
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

pub struct RecoveredSessionProjection {
    pub owner_scope: String,
    pub session: awaken_session_state_contract::PersistedSession,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionProjectionRecoveryError {
    #[error("Session was not found")]
    NotFound,
    #[error(transparent)]
    Rejected(#[from] RunError),
    #[error("Session recovery is unavailable: {0}")]
    Unavailable(String),
}

fn mutation_error(error: SessionMutationError) -> RunError {
    match error {
        SessionMutationError::NotFound => RunError::bad_request("Session was not found"),
        SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
            RunError::unavailable("Session changed while it was being admitted")
        }
        SessionMutationError::Unavailable(message) => RunError::unavailable(message),
    }
}

fn preparation_error(error: SessionPreparationError) -> RunError {
    match error {
        SessionPreparationError::Rejected(error) => error,
        SessionPreparationError::Unavailable(message) => RunError::unavailable(message),
        SessionPreparationError::NotFound => RunError::bad_request("Session was not found"),
        SessionPreparationError::Conflict => {
            RunError::unavailable("Session changed while it was being admitted")
        }
    }
}

fn realization_error(error: SessionRealizationDriveError) -> RunError {
    match error {
        SessionRealizationDriveError::Effect(error) => error.into(),
        SessionRealizationDriveError::Control(error) => RunError::unavailable(error.to_string()),
        SessionRealizationDriveError::DidNotConverge => {
            RunError::unavailable("Session realization did not converge")
        }
    }
}

fn creation_error(error: SessionCreationError) -> RunError {
    match error {
        SessionCreationError::Rejected(error) => error,
        SessionCreationError::Conflict => RunError::unavailable("Session creation conflicted"),
        SessionCreationError::Tombstoned => {
            RunError::bad_request("Session identity is terminally occupied")
        }
        SessionCreationError::IdempotencyMismatch => {
            RunError::bad_request("Session creation idempotency mismatch")
        }
        SessionCreationError::Unavailable(message) => RunError::unavailable(message),
        SessionCreationError::Internal(message) => RunError::internal(message),
    }
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

impl SessionApplication {
    /// Reserve one stable Run, then commit or recover the exact Session activity
    /// receipt before activation. This is the sole durable Session Run admission
    /// boundary used by both public protocols and Event reconciliation.
    pub async fn admit_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        self.admit_session_run_with_owner(command, None).await
    }

    /// Admit a protocol request under its already-authenticated owner scope.
    /// This is the same durable admission as [`Self::admit_session_run`]; the
    /// extra coordinate is used only to create or authorize the Session before
    /// the shared reservation/activity/activation protocol starts.
    pub async fn admit_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        self.admit_session_run_with_owner(command, Some(owner_scope))
            .await
    }

    /// Admit one product-authored Run on an existing profiled Session under
    /// its authenticated owner. The profiled edge never addresses a Managed
    /// Session and never creates one.
    ///
    /// | Rule | Cause | Effect |
    /// |---|---|---|
    /// | P1 | no root visible to the owner, or no frozen baseline | `NotFound` |
    /// | P2 | the frozen mutation policy is Managed | `NotFound` |
    /// | P3 | the Run Agent differs from the frozen Agent | rejected |
    /// | P4 | otherwise | the shared owner-fenced durable admission |
    pub async fn admit_profiled_session_run(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, crate::SessionProjectionRecoveryError> {
        let projection = self
            .queries
            .read_session_projection(command.session_id.as_str(), Some(owner_scope))
            .await?
            .ok_or(crate::SessionProjectionRecoveryError::NotFound)?;
        let baseline = projection
            .session
            .frozen_baseline()
            .ok_or(crate::SessionProjectionRecoveryError::NotFound)?;
        if baseline.mutation_policy.is_managed() {
            return Err(crate::SessionProjectionRecoveryError::NotFound);
        }
        if baseline.agent_id != command.agent_id {
            return Err(crate::SessionProjectionRecoveryError::Rejected(
                RunError::bad_request(
                    "profiled Session Run Agent does not match the frozen Session",
                ),
            ));
        }
        self.admit_session_run_for_owner(owner_scope, command)
            .await
            .map_err(crate::SessionProjectionRecoveryError::Rejected)
    }

    fn admit_session_run_with_owner<'a>(
        &'a self,
        command: AdmitSessionRun,
        expected_owner: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AdmittedSessionRun, RunError>> + Send + 'a>,
    > {
        let coordination_runtime = self.runtime().coordination();

        // Return a boxed future at the application boundary. The admission
        // graph is intentionally deep; boxing here prevents protocol and
        // lifecycle callers from moving its complete state machine through a
        // default Tokio worker stack.
        Box::pin(async move {
            let session_id = command.session_id.clone();
            let agent_id = command.agent_id.clone();
            let run_id = command.run_id.clone();
            let thread_id = awaken_agent_contract::ThreadId(session_id.to_string());
            let operation = session_run_activity_operation_id(&session_id, &run_id);
            // Cause/effect decision table: an existing exact activity receipt is
            // response-loss truth and outranks current policy, but it says nothing
            // about this process's disposable Runtime projection. Every admission
            // therefore recovers the authoritative Session projection before Runtime
            // freezes the dispatch; only activity creation is skipped on receipt
            // replay. Reservation remains non-executable until the exact epoch is
            // activated.
            let durable_owner = match self.mutations.owner(&session_id).await {
                Ok(actual_owner) => {
                    if expected_owner.is_some_and(|expected| expected != actual_owner) {
                        return Err(RunError::bad_request("Session was not found"));
                    }
                    Some(actual_owner)
                }
                Err(SessionMutationError::NotFound) if expected_owner.is_some() => None,
                Err(error) => return Err(mutation_error(error)),
            };
            let existing_activity_epoch = match durable_owner.as_ref() {
                Some(_) => self
                    .activities
                    .recover_activity_for_operation(
                        &session_id,
                        &operation,
                        Some((&thread_id, &run_id)),
                    )
                    .await
                    .map_err(crate::SessionActivityError::run_error)?
                    .map(|(_, epoch)| epoch),
                None => None,
            };
            let owner_scope = durable_owner
                .as_deref()
                .or(expected_owner)
                .ok_or_else(|| RunError::bad_request("Session was not found"))?;
            let projected_revision = self
                .admit_run_session(owner_scope, session_id.as_str(), &agent_id)
                .await?;
            let reservation = coordination_runtime.reserve_session_run(command).await?;
            let delivery = |session_activity_epoch| SessionRunDelivery {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                session_activity_epoch,
            };

            match session_run_activity_receipt_plan(reservation, existing_activity_epoch) {
                SessionRunActivityReceiptPlan::CommitActivity {
                    reservation_replayed,
                } => {
                    let session_activity_epoch = self
                        .activities
                        .begin_activity_for_operation_at_revision(
                            &session_id,
                            &operation,
                            projected_revision,
                            (&thread_id, &run_id),
                        )
                        .await
                        .map_err(crate::SessionActivityError::run_error)?
                        .1;
                    Ok(if reservation_replayed {
                        AdmittedSessionRun::AlreadyReserved(delivery(session_activity_epoch))
                    } else {
                        AdmittedSessionRun::Reserved(delivery(session_activity_epoch))
                    })
                }
                SessionRunActivityReceiptPlan::UseReceipt {
                    session_activity_epoch,
                    reservation_replayed,
                } => Ok(if reservation_replayed {
                    AdmittedSessionRun::AlreadyReserved(delivery(session_activity_epoch))
                } else {
                    AdmittedSessionRun::Reserved(delivery(session_activity_epoch))
                }),
                SessionRunActivityReceiptPlan::UseActivatedReceipt {
                    session_activity_epoch,
                } => Ok(AdmittedSessionRun::AlreadyActivated(delivery(
                    session_activity_epoch,
                ))),
                SessionRunActivityReceiptPlan::RecoveryClaimed => {
                    Ok(AdmittedSessionRun::RecoveryClaimed { session_id, run_id })
                }
                SessionRunActivityReceiptPlan::Completed => {
                    Ok(AdmittedSessionRun::Completed { session_id, run_id })
                }
                SessionRunActivityReceiptPlan::RejectInvalidReceipt => Err(RunError::internal(
                    "reserved or activated Session Run has no activity epoch",
                )),
            }
        })
    }

    /// Admit, register-before-activation, and project one foreground Run from
    /// authoritative committed Thread truth. Foreground/background is only an
    /// observation policy; the durable state transition is the same admission.
    pub async fn run_admitted_session(
        &self,
        command: AdmitSessionRun,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let coordination_runtime = self.runtime().coordination();

        let input_message_ids = command.input_message_ids();
        let admitted = Box::pin(self.admit_session_run(command)).await?;
        Box::pin(coordination_runtime.activate_and_observe_session_run(
            admitted,
            input_message_ids,
            sink,
        ))
        .await
    }

    /// Owner-authenticated form of [`Self::run_admitted_session`] used by
    /// public protocol adapters. Both forms converge on the same durable Run
    /// reservation and foreground observation port.
    pub async fn run_admitted_session_for_owner(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let coordination_runtime = self.runtime().coordination();

        let input_message_ids = command.input_message_ids();
        let admitted = Box::pin(self.admit_session_run_for_owner(owner_scope, command)).await?;
        Box::pin(coordination_runtime.activate_and_observe_session_run(
            admitted,
            input_message_ids,
            sink,
        ))
        .await
    }

    /// Publish one already-admitted Session Run without observing its terminal
    /// result. Background product coordinators use this after
    /// [`Self::admit_session_run_for_owner`] and consume the canonical lifecycle
    /// feed independently; they never need access to the dispatch store.
    pub async fn activate_admitted_session_run(
        &self,
        admitted: AdmittedSessionRun,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        let coordination_runtime = self.runtime().coordination();

        match admitted {
            AdmittedSessionRun::Reserved(delivery)
            | AdmittedSessionRun::AlreadyReserved(delivery)
            | AdmittedSessionRun::AlreadyActivated(delivery) => {
                coordination_runtime.activate_session_run(delivery).await
            }
            AdmittedSessionRun::RecoveryClaimed { .. } => {
                Ok(awaken_session_execution_contract::SessionRunActivation::RecoveryClaimed)
            }
            AdmittedSessionRun::Completed { .. } => {
                Ok(awaken_session_execution_contract::SessionRunActivation::Completed)
            }
        }
    }

    /// Rebuild the disposable Runtime projection at an execution admission
    /// boundary. This is deliberately separate from [`crate::SessionQueries::read_session_projection`].
    pub async fn recover_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError> {
        let Some(recovered) = self
            .queries
            .read_session_projection(thread_id, expected_owner)
            .await?
        else {
            return Ok(None);
        };
        // Archived Sessions remain readable projections but deny every new
        // effect. Recovery must not reacquire a realization lease for them.
        if recovered.session.is_terminal() {
            return Ok(Some(recovered));
        }
        let admission = self
            .refresh_executable_projections()
            .await
            .map_err(|error| {
                SessionProjectionRecoveryError::Unavailable(format!(
                    "executable projection refresh failed: {error}"
                ))
            })?;
        let owner = recovered.owner_scope;
        // Resource convergence is the largest Session state machine. Keep its
        // future behind one heap indirection at this application boundary so
        // protocol handlers do not synchronously embed the complete recovery,
        // credential, and Resource reconciliation graph in their poll stack.
        // The owned aggregate still crosses exactly one domain boundary; this
        // is execution-shape isolation, not another persistence abstraction.
        let session = Box::pin(
            admission.enter(|| self.reconcile_persisted_resources(&owner, recovered.session)),
        )
        .await
        .map_err(|error| SessionProjectionRecoveryError::Rejected(preparation_error(error)))?;
        let requires_external_realization = session.requires_external_environment_owner();
        let session = if requires_external_realization {
            self.realizations
                .install_dispatch_projection(&owner, &session)
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(realization_error(error))
                })?;
            self.work_assignments
                .dispatch(&session)
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(RunError::unavailable_classified(
                        "session_work_dispatch_failed",
                        format!("Session realization work could not be dispatched: {error}"),
                    ))
                })?;
            session
        } else if !session.environment.admits_realization_effects() {
            // Suspending/Hibernated/Restoring are canonical continuation
            // phases, not invalid realization states. Install only the
            // complete non-physical dispatch projection so the durable Run can
            // be reserved. Its later activity boundary joins the one
            // continuation operation and performs the Resident handoff before
            // activation; no realization effect may run against the old phase.
            self.realizations
                .install_dispatch_projection(&owner, &session)
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(realization_error(error))
                })?;
            session
        } else {
            self.realizations
                .realize_session_after_refresh(&awaken_session_types::SessionId::from(thread_id))
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(realization_error(error))
                })?
        };
        Ok(Some(RecoveredSessionProjection {
            owner_scope: owner,
            session,
        }))
    }

    async fn recover_admitted_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
    ) -> Result<SessionRevision, RunApplicationError> {
        let recovered = self
            .recover_session_projection(thread_id, Some(workspace_id))
            .await
            .map_err(|error| match error {
                SessionProjectionRecoveryError::NotFound => {
                    RunError::bad_request("Session was not found")
                }
                SessionProjectionRecoveryError::Rejected(error) => error,
                SessionProjectionRecoveryError::Unavailable(message) => {
                    RunError::unavailable(message)
                }
            })?
            .ok_or_else(|| RunError::bad_request("Session was not found"))?;
        if recovered.session.is_terminal() {
            return Err(RunError::bad_request("Session no longer accepts new Runs"));
        }
        if let Some(baseline) = recovered
            .session
            .frozen_baseline()
            .filter(|baseline| baseline.environment.self_hosted)
        {
            self.environments
                .assignment
                .wake_session(
                    &baseline.environment.environment_id,
                    &recovered.session.session_id,
                )
                .await
                .map_err(|error| {
                    RunError::unavailable_classified(
                        "session_work_wake_failed",
                        format!("Session Work could not be awakened: {error}"),
                    )
                })?;
        }
        if !recovered.session.requires_external_environment_owner()
            && !recovered.session.execution.admits_activity()
        {
            return Err(RunError::unavailable_classified(
                "session_not_ready",
                format!(
                    "Session realization has not completed; current state is `{}`",
                    recovered.session.execution
                ),
            ));
        }
        Ok(recovered.session.revision)
    }

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
            .refresh_executable_projections()
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
                    self.compensate_request_repositories(
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
                self.compensate_request_repositories(
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
                self.compensate_request_repositories(
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

    async fn create_default_run_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<SessionRevision, RunApplicationError> {
        self.create_profiled_session(CreateProfiledSessionCommand {
            owner_scope: workspace_id.to_string(),
            session_id: thread_id.to_string(),
            mutation_policy: crate::session_resource::SessionMutationPolicy::Managed,
            application_origin: None,
            agent_id: agent_id.to_string(),
            source_revision: None,
            environment_id: None,
            model: None,
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            resource_inputs: Vec::new(),
            mcp_candidates: Vec::new(),
            repositories: Vec::new(),
            network_restriction: None,
            title: None,
            metadata: Default::default(),
            tools: None,
            idempotency: None,
        })
        .await
        .map_err(creation_error)
        .map(|session| session.revision)
    }

    pub async fn admit_run_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<SessionRevision, RunApplicationError> {
        match self
            .queries
            .session(&awaken_session_types::SessionId::from(thread_id))
            .await
        {
            Ok(_) => self.recover_admitted_session(workspace_id, thread_id).await,
            Err(SessionRepositoryError::NotFound) => {
                match self
                    .create_default_run_session(workspace_id, thread_id, agent_id)
                    .await
                {
                    Ok(revision) => Ok(revision),
                    // Concurrent first Runs race only at the durable create fence;
                    // every loser adopts the exact winner through the same recovery.
                    Err(error) if error.kind == awaken_session_types::RunErrorKind::Unavailable => {
                        match self
                            .queries
                            .session(&awaken_session_types::SessionId::from(thread_id))
                            .await
                        {
                            Ok(_) => self.recover_admitted_session(workspace_id, thread_id).await,
                            Err(SessionRepositoryError::NotFound) => Err(error),
                            Err(error) => Err(session_repository_error(error)),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(session_repository_error(error)),
        }
    }
}

/// The sole durable Session Run application for all public Run protocols.
/// The resolvers read the Runtime's installed Session; an unreadable one is
/// refused, never answered with the local Workspace or the default Agent.
type WorkspaceResolver = dyn Fn(&str) -> Result<String, RunError> + Send + Sync;
type ProjectedAgentResolver = dyn Fn(&str) -> Result<Option<String>, RunError> + Send + Sync;

pub struct SessionRunApplication {
    runtime: Arc<dyn RunApplication>,
    sessions: Arc<SessionApplication>,
    workspace: Arc<WorkspaceResolver>,
    projected_agent: Arc<ProjectedAgentResolver>,
}

impl SessionRunApplication {
    #[must_use]
    pub fn new(
        runtime: Arc<dyn RunApplication>,
        sessions: Arc<SessionApplication>,
        workspace: impl Fn(&str) -> Result<String, RunError> + Send + Sync + 'static,
        projected_agent: impl Fn(&str) -> Result<Option<String>, RunError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime,
            sessions,
            workspace: Arc::new(workspace),
            projected_agent: Arc::new(projected_agent),
        }
    }

    fn agent_for(&self, thread: &str, requested_agent: Option<&str>) -> Result<String, RunError> {
        if let Some(requested) = requested_agent {
            return Ok(requested.to_string());
        }
        Ok((self.projected_agent)(thread)?.unwrap_or_else(|| "assistant".to_string()))
    }

    fn admission_command(
        &self,
        operation_id: &str,
        thread: &str,
        requested_agent: Option<&str>,
        messages: Vec<Message>,
        traceparent: Option<String>,
        replacement: awaken_session_types::SessionRunReplacement,
    ) -> Result<AdmitSessionRun, RunApplicationError> {
        if operation_id.trim().is_empty() {
            return Err(RunError::bad_request(
                "Session Run requires a stable operation identity",
            ));
        }
        if messages.is_empty() {
            return Err(RunError::bad_request(
                "Session Run requires at least one new input message",
            ));
        }
        Ok(AdmitSessionRun {
            session_id: awaken_session_types::SessionId::from(thread),
            agent_id: self.agent_for(thread, requested_agent)?,
            operation_id: operation_id.to_string(),
            run_id: awaken_session_execution_contract::session_run_id(
                &awaken_session_types::SessionId::from(thread),
                operation_id,
            ),
            messages,
            data_subject_id: None,
            traceparent,
            execution_requirements: Default::default(),
            replacement,
        })
    }

    async fn run_admitted(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Box::pin(
            self.sessions
                .run_admitted_session_for_owner(owner_scope, command, sink),
        )
        .await
    }
}

#[async_trait::async_trait]
impl RunApplication for SessionRunApplication {
    async fn run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            None,
            awaken_session_types::SessionRunReplacement::PreservePrior,
        )?;
        self.run_admitted(&(self.workspace)(thread)?, command, None)
            .await
    }

    async fn run_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.run_streaming_for_owner(
            &(self.workspace)(thread)?,
            operation_id,
            thread,
            agent,
            messages,
            sink,
        )
        .await
    }

    async fn resume(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.resume_for_owner(
            &(self.workspace)(thread)?,
            operation_id,
            thread,
            tool_use_id,
            resume,
        )
        .await
    }

    async fn resume_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.resume_streaming_for_owner(
            &(self.workspace)(thread)?,
            operation_id,
            thread,
            tool_use_id,
            resume,
            sink,
        )
        .await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.runtime.interrupt(thread).await
    }

    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        self.runtime.pending(thread).await
    }

    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        self.runtime.history(thread).await
    }

    fn model(&self) -> String {
        self.runtime.model()
    }

    async fn usage(&self, thread: &str) -> Result<(u64, u64), RunApplicationError> {
        self.runtime.usage(thread).await
    }
}

#[async_trait::async_trait]
impl OwnerScopedRunApplication for SessionRunApplication {
    async fn run_streaming_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            None,
            awaken_session_types::SessionRunReplacement::PreservePrior,
        )?;
        self.run_admitted(owner_scope, command, Some(sink)).await
    }

    async fn resume_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.sessions
            .resume_session_run_for_owner(
                owner_scope,
                operation_id,
                &awaken_session_types::SessionId::from(thread),
                tool_use_id,
                resume,
            )
            .await
    }
    async fn resume_streaming_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.sessions
            .resume_session_run_streaming_for_owner(
                owner_scope,
                operation_id,
                &awaken_session_types::SessionId::from(thread),
                tool_use_id,
                resume,
                sink,
            )
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionRunReplacementApplication
    for SessionRunApplication
{
    async fn supersede_session_run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            None,
            awaken_session_types::SessionRunReplacement::SupersedePrior,
        )?;
        self.run_admitted(&(self.workspace)(thread)?, command, None)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionRunBackgroundApplication
    for SessionRunApplication
{
    async fn submit_session_run_background(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
    ) -> Result<awaken_agent_contract::agent::run::Id, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            traceparent,
            awaken_session_types::SessionRunReplacement::PreservePrior,
        )?;
        let run_id = command.run_id.clone();
        let owner_scope = (self.workspace)(thread)?;
        let admitted = Box::pin(
            self.sessions
                .admit_session_run_for_owner(&owner_scope, command),
        )
        .await?;
        self.sessions
            .activate_admitted_session_run(admitted)
            .await?;
        Ok(run_id)
    }
}

#[cfg(test)]
mod preparation_presentation_tests {
    use super::*;

    #[test]
    fn profiled_creation_retains_each_original_preparation_presentation() {
        // F4 presentation table: every original preparation variant crosses
        // the existing Profiled projector, then existing From<RunError>.
        // Requested creation instead keeps Preparation. This is deliberately
        // independent of current pin-only errors (all Rejected), so future
        // pin variants cannot silently flatten NotFound/Conflict/outage.
        for (input, expected) in [
            (
                SessionPreparationError::NotFound,
                RunError::bad_request("Session was not found"),
            ),
            (
                SessionPreparationError::Conflict,
                RunError::unavailable("Session changed while it was being admitted"),
            ),
            (
                SessionPreparationError::Unavailable("original outage".into()),
                RunError::unavailable("original outage"),
            ),
            (
                SessionPreparationError::Rejected(RunError::classified(
                    "original-code",
                    "original rejection",
                )),
                RunError::classified("original-code", "original rejection"),
            ),
        ] {
            let actual = SessionCreationError::from(preparation_error(input));
            let SessionCreationError::Rejected(actual) = actual else {
                panic!("Profiled presentation changed")
            };
            assert_eq!(
                (actual.kind, actual.code, actual.message),
                (expected.kind, expected.code, expected.message)
            );
        }
    }
}
