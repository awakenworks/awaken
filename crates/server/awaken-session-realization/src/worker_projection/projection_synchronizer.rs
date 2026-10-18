// @formal entrypoint session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Worker projection synchronization and physical MCP effect adapters.
//!
//! The canonical realization phase driver remains in the parent module. This
//! private split only projects its frozen directive through the one
//! `WorkerProjectionSynchronizer` and the existing Host MCP effects.

use super::SessionProjectionExecution;
use crate::peers::EnvironmentPeer as _;
use crate::session_environment::SessionEnvironmentAdoptionDisposition;
use awaken_session_configuration_contract::mcp as session_mcp;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::{CanonicalSessionProjection, requires_local_environment};

pub struct WorkerProjectionSynchronizer<'a, T> {
    /// The projection execution role the synchronizer drives.
    pub execution: &'a T,
    pub claim: Option<&'a awaken_dispatch_contract::RunClaim>,
    pub published_snapshot: Option<&'a awaken_runtime_contract::ExecutableAgentSnapshot>,
    pub rebuild_unavailable_environment: bool,
    pub requires_runtime_before_effects: bool,
    pub claimed_environment_binding: Option<&'a str>,
}

impl<'a, T> WorkerProjectionSynchronizer<'a, T>
where
    T: SessionProjectionExecution,
{
    /// Project claim-owned inputs at the same adapter boundary as directive
    /// inputs. The parent drives phases; this adapter alone maps claim fields.
    pub fn for_claimed(
        execution: &'a T,
        session_id: &str,
        directive: &awaken_session_realization_contract::SessionRealizationDirective,
        claimed: Option<&'a awaken_dispatch_contract::Claimed>,
        claim: Option<&'a awaken_dispatch_contract::RunClaim>,
    ) -> Self {
        let published_snapshot = claimed.and_then(|claimed| {
            (claimed.request.thread_id().0 == session_id)
                .then_some(&claimed.request.activation.snapshot)
        });
        let rebuild_unavailable_environment = claimed.is_some_and(|claimed| {
            claimed.request.placement.recovery
                == awaken_dispatch_contract::WorkerRecoveryMode::RebuildFromCommittedTruth
        });
        let claimed_environment_binding = claimed.and_then(|claimed| claimed.sandbox.as_deref());
        Self::for_directive(
            execution,
            directive,
            claim,
            published_snapshot,
            rebuild_unavailable_environment,
            claimed_environment_binding,
        )
    }

    /// Build the one Worker effect adapter from an already-frozen directive and
    /// its optional claimed-dispatch authority. The directive remains the owner
    /// of MCP stage intent; this adapter derives only the physical ordering bit.
    pub fn for_directive(
        execution: &'a T,
        directive: &awaken_session_realization_contract::SessionRealizationDirective,
        claim: Option<&'a awaken_dispatch_contract::RunClaim>,
        published_snapshot: Option<&'a awaken_runtime_contract::ExecutableAgentSnapshot>,
        rebuild_unavailable_environment: bool,
        claimed_environment_binding: Option<&'a str>,
    ) -> Self {
        let requires_runtime_before_effects = matches!(
            &directive.action,
            awaken_session_realization_contract::SessionRealizationAction::Stage { mcp_stages, .. }
                if mcp_stages
                    .iter()
                    .any(|stage| stage.target.sandbox_stdio_target().is_some())
        );
        Self {
            execution,
            claim,
            published_snapshot,
            rebuild_unavailable_environment,
            requires_runtime_before_effects,
            claimed_environment_binding,
        }
    }
}

#[async_trait::async_trait]
impl<T> awaken_session_realization_contract::SessionProjectionSynchronizer
    for WorkerProjectionSynchronizer<'_, T>
where
    T: SessionProjectionExecution,
{
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_types::RunError,
    > {
        awaken_authority_clock::authority_proof_started().map_err(|error| {
            awaken_session_types::RunError::unavailable(format!("authority clock: {error}"))
        })
    }

    async fn synchronize_session_projection(
        &self,
        session_id: &awaken_session_types::SessionId,
        projection: &awaken_session_realization_contract::FrozenSessionProjection,
        lease: &awaken_session_realization_contract::ObservedSessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), awaken_session_types::RunError> {
        let realizations = self.execution.sessions();
        let exec = self.execution.realization_executor();
        let retained_publication = realizations.published_snapshot(session_id.as_str())?;
        if let Some(claimed) = self.published_snapshot
            && !matches!(
                awaken_session_realization_contract::frozen_agent_publication_decision(
                    &projection.baseline,
                    Some(claimed),
                ),
                awaken_session_realization_contract::FrozenAgentPublicationDecision::Unpinned
                    | awaken_session_realization_contract::FrozenAgentPublicationDecision::Exact
            )
        {
            return Err(awaken_session_types::RunError::classified(
                "session_runtime_publication_conflict",
                "claimed Run does not match the frozen Session Agent publication",
            ));
        }
        let delivered_publication = projection
            .agent_publication
            .as_ref()
            .or(self.published_snapshot);
        let published_snapshot = delivered_publication.or(retained_publication.as_ref());
        match awaken_session_realization_contract::frozen_agent_publication_decision(
            &projection.baseline,
            published_snapshot,
        ) {
            awaken_session_realization_contract::FrozenAgentPublicationDecision::Unpinned
            | awaken_session_realization_contract::FrozenAgentPublicationDecision::OptionalMissing
            | awaken_session_realization_contract::FrozenAgentPublicationDecision::Exact => {}
            awaken_session_realization_contract::FrozenAgentPublicationDecision::MissingRequired => {
                return Err(awaken_session_types::RunError::classified(
                    "session_runtime_publication_missing",
                    "Worker Session realization requires its exact frozen Agent publication",
                ));
            }
            awaken_session_realization_contract::FrozenAgentPublicationDecision::Mismatch => {
                return Err(awaken_session_types::RunError::classified(
                    "session_runtime_publication_conflict",
                    "Worker Agent publication does not match the frozen Session identity, revision, or runtime",
                ));
            }
        }
        // Match ManagedHost's lifecycle -> Resource-projection lock order. A
        // claimed legacy retirement must be resolved before raw delivery cache
        // repair or any projection/Resource effect, and the same guard prevents
        // another owner transition until the replacement projection is wholly
        // installed. Installing Control's frozen projection is the install path.
        realizations.install_session(session_id.as_str())?;
        let lifecycle_guard = realizations.lock_lifecycle(session_id.as_str()).await?;
        if self.claim.is_some() {
            let (canonical_publication, model_candidate) = exec
                .environments()
                .publication()
                .resolve_canonical_session_projection(
                    &projection.workspace_id,
                    CanonicalSessionProjection::Baseline(&projection.baseline),
                    published_snapshot.cloned(),
                    Some(projection.resources.skills()),
                )?;
            let a2a_only = canonical_publication
                .as_ref()
                .is_some_and(|publication| !requires_local_environment(&publication.resolved_spec));
            let has_local_environment_owner = realizations
                .read_realization(session_id.as_str(), |r| {
                    r.environment().owner.has_local_environment()
                })?
                .unwrap_or(false);
            let has_environment_binding = self.claimed_environment_binding.is_some()
                || projection.environment.binding().is_some();
            if a2a_only && (has_environment_binding || has_local_environment_owner) {
                return Err(awaken_session_types::RunError::classified(
                    "session_runtime_remote_environment_conflict",
                    "remote A2A Session realization cannot consume a local Environment binding or owner",
                ));
            }
            if self.rebuild_unavailable_environment && !a2a_only {
                let provider = if let Some(candidate) = model_candidate.as_ref() {
                    exec.environments().session_environment_provider_for_thread(
                        session_id.as_str(),
                        candidate.provisioning(),
                    )
                } else {
                    exec.environments().session_environment_provider_for_thread(
                        session_id.as_str(),
                        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                    )
                }?;
                exec.rebuild_claimed_legacy_environment_after_revocation(
                    session_id.as_str(),
                    provider,
                )
                .await?;
            }
        }
        // A claim authorizes live Resource revalidation. A preparation Stage
        // authorizes local realization. Lease-only MCP renewal has neither and
        // must reuse the already-resident Resource/Skill projection instead of
        // opening an unclaimed remote materialization path.
        let synchronize_resources = self.claim.is_some() || prepare_session;
        // Control materializes current context on every directive. Install the
        // supplied projection even for a lease-only Complete action so a Session
        // command accepted between Runs cannot leave a warm installed Session stale.
        let mut projection = projection.clone();
        // The aggregate is authoritative after any Environment transition.
        // RunDispatch.sandbox is only a raw-handle delivery cache; the
        // aggregate's `LegacyBindingDelivered` decision admits its single
        // legacy repair and keeps every root-owned resident or continuation
        // step.
        projection
            .environment
            .deliver_legacy_binding(self.claimed_environment_binding);
        if projection.agent_publication.is_none() {
            projection.agent_publication = published_snapshot.cloned();
        }
        self.execution
            .install_frozen_projection(
                session_id.as_str(),
                projection.clone(),
                self.claim,
                synchronize_resources,
                Some(lease.clone()),
            )
            .await?;
        // A Control-synchronized projection is the same complete Managed
        // Session authority as a co-located SessionRuntime installation. Mark
        // that existing ownership only after every frozen fact is installed;
        // otherwise the later claimed resolver mistakes an empty compatibility
        // envelope for ordinary desired state and drains Control's active MCP
        // generation.
        realizations.enter_session_dispatch(session_id.as_str())?;
        drop(lifecycle_guard);
        let published_snapshot = realizations.published_snapshot(session_id.as_str())?;
        // A resident Arc is process-local ownership, not evidence that its
        // physical Sandbox survived. Revalidate every durable binding through
        // the one adoption path; Ready reuses that Arc, while closed evidence
        // follows the existing policy-fenced Rebuild decision.
        let has_environment_binding = projection.environment.binding().is_some();
        let runtime_authority_resident = realizations
            .modify_session(session_id.as_str(), |held| {
                held.run.holds_context() || held.realization.environment().owner.is_resident()
            })?
            .unwrap_or(false);
        if self.requires_runtime_before_effects
            && published_snapshot.is_none()
            && !runtime_authority_resident
        {
            return Err(awaken_session_types::RunError::classified(
                "session_runtime_publication_missing",
                "sandbox stdio MCP realization requires the exact claimed Agent snapshot before effects",
            ));
        }
        let source_generation_id = projection
            .environment
            .generation()
            .map(|generation| generation.id());
        let adoption = if let Some(binding) = projection.environment.binding() {
            let provider = exec
                .environments()
                .projected_session_environment_provider(session_id.as_str(), None)?;
            self.execution
                .realization_executor()
                .adopt_bound_session_environment(
                    session_id.as_str(),
                    Some(binding),
                    provider,
                    source_generation_id,
                    self.rebuild_unavailable_environment,
                )
                .await?
        } else {
            SessionEnvironmentAdoptionDisposition::NoBinding
        };
        let rebuild_binding = adoption == SessionEnvironmentAdoptionDisposition::RebuildRequired;
        if rebuild_binding {
            // The only admissible replacement path is the claim-bound recovery
            // decision above. Clear the process-local expectation only after the
            // provider has proved the exact durable binding unavailable; the new
            // Environment receipt must still pass the ordinary durable sink.
            debug_assert!(
                exec.environments()
                    .durable_environment_binding(session_id.as_str())?
                    .is_none(),
                "terminated recovery owner clears its durable binding"
            );
        }
        let adopted_ready = adoption == SessionEnvironmentAdoptionDisposition::Ready;
        // Synchronization is the single ordering boundary between Control's
        // frozen projection and MCP effects. A first-use Environment has no
        // durable binding to adopt yet, but its stage still needs the exact Run
        // publication installed before it may realize sandbox stdio. Only the
        // physical substrate is needed here; parent Agent plugins remain owned
        // by the exact root attempt and must not be constructed as a side effect.
        let reconciled_by_substrate = if adopted_ready {
            true
        } else if published_snapshot.is_some()
            && (has_environment_binding || self.requires_runtime_before_effects)
        {
            self.execution
                .ensure_session_substrate(session_id.as_str(), published_snapshot.as_ref())
                .await?;
            true
        } else {
            false
        };
        if synchronize_resources && !reconciled_by_substrate {
            self.execution
                .realization_executor()
                .apply_session_inputs_with_context(
                    session_id.as_str(),
                    &projection.resource_transition(
                        awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
                    )?,
                    self.claim,
                    Some(&lease.lease),
                )
                .await?;
        }
        Ok(())
    }

    async fn invalidate_session_realization_lease(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &session_resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        self.execution
            .invalidate_realization_lease(session_id.as_str(), lease)
            .map_err(awaken_session_types::RunError::from)
    }
}

pub struct WorkerMcpEffects<'a, T>(pub &'a T);

#[async_trait::async_trait]
impl<T: SessionProjectionExecution> awaken_session_execution_contract::McpAttachmentRealizer
    for WorkerMcpEffects<'_, T>
{
    async fn stage_mcp_attachment(
        &self,
        request: session_mcp::StageMcpAttachment,
    ) -> Result<session_mcp::McpRealizationReceipt, awaken_session_types::RunError> {
        self.0.mcp_effects().stage_mcp_attachment(request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        self.0
            .mcp_effects()
            .publish_mcp_generation(generation)
            .await
    }

    async fn drain_mcp_generation(
        &self,
        generation: session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        self.0.mcp_effects().drain_mcp_generation(generation).await
    }
}
