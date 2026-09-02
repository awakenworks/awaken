//! Worker projection synchronization and physical MCP effect adapters.
//!
//! The canonical realization phase driver remains in the parent module. This
//! private split only projects its frozen directive through the one
//! `WorkerProjectionSynchronizer` and the existing Host MCP effects.

use super::*;

pub(super) struct WorkerProjectionSynchronizer<'a> {
    pub(super) host: &'a SharedHost,
    pub(super) claim: Option<&'a awaken_dispatch::RunClaim>,
    pub(super) published_snapshot: Option<&'a awaken_runtime_contract::ExecutableAgentSnapshot>,
    pub(super) rebuild_unavailable_environment: bool,
    pub(super) requires_runtime_before_effects: bool,
    pub(super) claimed_environment_binding: Option<&'a str>,
}

impl<'a> WorkerProjectionSynchronizer<'a> {
    /// Build the one Worker effect adapter from an already-frozen directive and
    /// its optional claimed-dispatch authority. The directive remains the owner
    /// of MCP stage intent; this adapter derives only the physical ordering bit.
    pub(super) fn for_directive(
        host: &'a SharedHost,
        directive: &awaken_session_contract::SessionRealizationDirective,
        claim: Option<&'a awaken_dispatch::RunClaim>,
        published_snapshot: Option<&'a awaken_runtime_contract::ExecutableAgentSnapshot>,
        rebuild_unavailable_environment: bool,
        claimed_environment_binding: Option<&'a str>,
    ) -> Self {
        let requires_runtime_before_effects = matches!(
            &directive.action,
            awaken_session_contract::SessionRealizationAction::Stage { mcp_stages, .. }
                if mcp_stages
                    .iter()
                    .any(|stage| stage.target.sandbox_stdio_target().is_some())
        );
        Self {
            host,
            claim,
            published_snapshot,
            rebuild_unavailable_environment,
            requires_runtime_before_effects,
            claimed_environment_binding,
        }
    }
}

/// Merge the one historical claim-first delivery fact without weakening the
/// Session aggregate. A missing binding is not sufficient evidence: suspended
/// continuation phases also omit one, but remain authoritative durable state.
pub(super) fn merge_legacy_claimed_environment_binding(
    environment: &mut awaken_session_contract::SessionEnvironmentState,
    claimed_environment_binding: Option<&str>,
) {
    if let (awaken_session_contract::SessionEnvironmentState::Unmaterialized, Some(binding)) =
        (&*environment, claimed_environment_binding)
    {
        *environment = awaken_session_contract::SessionEnvironmentState::Resident {
            binding: binding.to_string(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionProjectionSynchronizer for WorkerProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_session_contract::FrozenSessionProjection,
        lease: &awaken_session_contract::SessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), awaken_session_contract::RunError> {
        let retained_publication = self
            .host
            .session_slots
            .read(session_id, |slot| slot.published_snapshot.clone())
            .flatten();
        if let Some(claimed) = self.published_snapshot
            && !matches!(
                awaken_session_contract::frozen_agent_publication_decision(
                    &projection.baseline,
                    Some(claimed),
                ),
                awaken_session_contract::FrozenAgentPublicationDecision::Unpinned
                    | awaken_session_contract::FrozenAgentPublicationDecision::Exact
            )
        {
            return Err(awaken_session_contract::RunError::classified(
                "session_runtime_publication_conflict",
                "claimed Run does not match the frozen Session Agent publication",
            ));
        }
        let delivered_publication = projection
            .agent_publication
            .as_ref()
            .or(self.published_snapshot);
        let published_snapshot = delivered_publication.or(retained_publication.as_ref());
        match awaken_session_contract::frozen_agent_publication_decision(
            &projection.baseline,
            published_snapshot,
        ) {
            awaken_session_contract::FrozenAgentPublicationDecision::Unpinned
            | awaken_session_contract::FrozenAgentPublicationDecision::OptionalMissing
            | awaken_session_contract::FrozenAgentPublicationDecision::Exact => {}
            awaken_session_contract::FrozenAgentPublicationDecision::MissingRequired => {
                return Err(awaken_session_contract::RunError::classified(
                    "session_runtime_publication_missing",
                    "Worker Session realization requires its exact frozen Agent publication",
                ));
            }
            awaken_session_contract::FrozenAgentPublicationDecision::Mismatch => {
                return Err(awaken_session_contract::RunError::classified(
                    "session_runtime_publication_conflict",
                    "Worker Agent publication does not match the frozen Session identity, revision, or runtime",
                ));
            }
        }
        // Match ManagedHost's lifecycle -> Resource-projection lock order. A
        // claimed legacy retirement must be resolved before raw delivery cache
        // repair or any projection/Resource effect, and the same guard prevents
        // another owner transition until the replacement projection is wholly
        // installed.
        let lifecycle = self
            .host
            .session_slots
            .update(session_id, |slot| slot.lifecycle.clone());
        let lifecycle_guard = lifecycle.lock().await;
        if self.claim.is_some() {
            let (canonical_publication, model_candidate) = self
                .host
                .resolve_canonical_session_projection(
                    &projection.workspace_id,
                    crate::host::CanonicalSessionProjection::Baseline(&projection.baseline),
                    published_snapshot.cloned(),
                )
                .map_err(crate::managed_adapter_error::to_run_error)?;
            let a2a_only = canonical_publication.as_ref().is_some_and(|publication| {
                !crate::host::completion::requires_local_environment(&publication.resolved_spec)
            });
            let has_local_environment_owner = self
                .host
                .session_slots
                .read(session_id, |slot| {
                    slot.environment_owner.has_local_environment()
                })
                .unwrap_or(false);
            let has_environment_binding = self.claimed_environment_binding.is_some()
                || projection.environment.binding().is_some();
            if a2a_only && (has_environment_binding || has_local_environment_owner) {
                return Err(awaken_session_contract::RunError::classified(
                    "session_runtime_remote_environment_conflict",
                    "remote A2A Session realization cannot consume a local Environment binding or owner",
                ));
            }
            if self.rebuild_unavailable_environment && !a2a_only {
                let provider = if let Some(candidate) = model_candidate.as_ref() {
                    self.host
                        .session_environment_provider(candidate.provisioning())
                } else {
                    self.host.session_environment_provider(
                        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                    )
                }
                .map_err(crate::managed_adapter_error::to_run_error)?;
                self.host
                    .rebuild_claimed_legacy_environment_after_revocation(session_id, provider)
                    .await
                    .map_err(crate::managed_adapter_error::to_run_error)?;
            }
        }
        // A claim authorizes live Resource revalidation. A preparation Stage
        // authorizes local realization. Lease-only MCP renewal has neither and
        // must reuse the already-resident Resource/Skill projection instead of
        // opening an unclaimed remote materialization path.
        let synchronize_resources = self.claim.is_some() || prepare_session;
        // Control materializes current context on every directive. Install the
        // supplied projection even for a lease-only Complete action so a
        // Session command accepted between Runs cannot leave a warm slot stale.
        let mut projection = projection.clone();
        // The aggregate is authoritative after any Environment transition.
        // RunDispatch.sandbox is only a raw-handle delivery cache; the helper
        // below admits its single legacy repair and ignores every root-owned
        // resident or continuation phase.
        merge_legacy_claimed_environment_binding(
            &mut projection.environment,
            self.claimed_environment_binding,
        );
        if projection.agent_publication.is_none() {
            projection.agent_publication = published_snapshot.cloned();
        }
        self.host
            .install_frozen_session_projection(
                session_id,
                projection.clone(),
                self.claim,
                synchronize_resources,
                Some(lease.clone()),
            )
            .await
            .map_err(crate::managed_adapter_error::to_run_error)?;
        drop(lifecycle_guard);
        let published_snapshot = self
            .host
            .session_slots
            .read(session_id, |slot| slot.published_snapshot.clone())
            .flatten();
        let environment_absent = self.host.session_environment(session_id).await.is_none();
        let has_environment_binding =
            environment_absent && projection.environment.binding().is_some();
        let runtime_authority_resident = self
            .host
            .session_slots
            .read(session_id, |slot| {
                slot.runtime.is_some() || slot.environment_owner.is_resident()
            })
            .unwrap_or(false);
        if self.requires_runtime_before_effects
            && published_snapshot.is_none()
            && !runtime_authority_resident
        {
            return Err(awaken_session_contract::RunError::classified(
                "session_runtime_publication_missing",
                "sandbox stdio MCP realization requires the exact claimed Agent snapshot before effects",
            ));
        }
        let source_generation_id = projection
            .environment
            .generation()
            .map(|generation| generation.id.as_str());
        let adoption = if environment_absent && let Some(binding) = projection.environment.binding()
        {
            let provider = self
                .host
                .projected_session_environment_provider(session_id, None)
                .map_err(crate::managed_adapter_error::to_run_error)?;
            self.host
                .adopt_bound_session_environment(
                    session_id,
                    Some(binding),
                    provider,
                    source_generation_id,
                    self.rebuild_unavailable_environment,
                )
                .await
                .map_err(crate::managed_adapter_error::to_run_error)?
        } else {
            crate::host::session::SessionEnvironmentAdoptionDisposition::NoBinding
        };
        let rebuild_binding = adoption
            == crate::host::session::SessionEnvironmentAdoptionDisposition::RebuildRequired;
        if rebuild_binding {
            // The only admissible replacement path is the claim-bound recovery
            // decision above. Clear the process-local expectation only after the
            // provider has proved the exact durable binding unavailable; the new
            // Environment receipt must still pass the ordinary durable sink.
            debug_assert!(
                self.host
                    .durable_session_environment_binding(session_id)
                    .is_none(),
                "terminated recovery owner clears its durable binding"
            );
        }
        let adopted_ready =
            adoption == crate::host::session::SessionEnvironmentAdoptionDisposition::Ready;
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
            self.host
                .session_child_execution_substrate(session_id, published_snapshot.as_ref())
                .await
                .map_err(crate::managed_adapter_error::to_run_error)?;
            true
        } else {
            false
        };
        if synchronize_resources && !reconciled_by_substrate {
            self.host
                .apply_dispatched_resource_transition(
                    session_id,
                    &projection.resource_transition(
                        awaken_session_contract::FrozenResourceTransitionUse::ApplyEffects,
                    )?,
                    self.claim,
                )
                .await?;
        }
        Ok(())
    }
}

pub(in crate::host::worker_resolver) struct WorkerMcpEffects<'a>(
    pub(in crate::host::worker_resolver) &'a SharedHost,
);

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for WorkerMcpEffects<'_> {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
    {
        self.0.stage_dispatched_mcp(request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.publish_dispatched_mcp(generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.drain_dispatched_mcp(generation).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{RecoveryControl, RecoveryEnvironmentBindingSink, frozen_projection};
    use super::*;
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, empty_frozen_projection_for_snapshot, managed_test_host,
        test_activation,
    };
    use std::sync::Arc;

    /// Compatibility fixture for an aggregate-Unowned legacy claim. Returning
    /// `Unowned` keeps production creation on its marker-free V1 path; any
    /// attempted durable persist would prove that the scenario crossed into a
    /// different authority row.
    struct LegacyUnownedEnvironmentBindingSink;

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionEnvironmentBindingSink
        for LegacyUnownedEnvironmentBindingSink
    {
        async fn authorize(
            &self,
            _intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
        ) -> Result<
            awaken_session_contract::SessionEnvironmentEffectAuthorization,
            awaken_session_contract::RunError,
        > {
            Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned)
        }

        async fn persist(
            &self,
            _receipt: awaken_session_contract::SessionEnvironmentReceipt,
        ) -> Result<
            awaken_session_contract::SessionEnvironmentState,
            awaken_session_contract::RunError,
        > {
            panic!("legacy aggregate-Unowned recovery must not persist a durable binding")
        }
    }

    /// Reuse the production provider/create owner to establish the one legacy
    /// V1 revocation fixture shared by claimed pre-install ordering tests.
    async fn revoked_legacy_v1_environment(
        host: &SharedHost,
        thread: &str,
    ) -> (
        Arc<crate::session_environment::SessionEnvironment>,
        String,
        std::path::PathBuf,
    ) {
        let provider = host
            .projected_session_environment_provider(thread, None)
            .expect("select frozen provider for LegacyDirect fixture");
        let spec = host.sandbox_spec_for_provider(thread, provider);
        let environment = Arc::new(
            host.create_session_environment(provider, &spec)
                .await
                .expect("create marker-free LegacyDirect V1 owner"),
        );
        assert!(
            environment
                .handle()
                .filesystem_physical_incarnation()
                .expect("local V1 handle")
                .is_none(),
            "fixture must exercise handle-equal V1 recreation"
        );
        let binding = serde_json::to_string(&environment.handle()).expect("raw V1 binding");
        let sentinel = host
            .storage_dir()
            .expect("legacy fixture storage")
            .join("sandboxes")
            .join(thread)
            .join("retired-owner-sentinel");
        std::fs::write(&sentinel, b"retiring LegacyDirect owner").expect("legacy root sentinel");
        host.install_test_resident_session_environment(thread, environment.clone());
        let lifecycle = host
            .session_slots
            .read(thread, |slot| slot.lifecycle.clone())
            .expect("legacy lifecycle owner");
        {
            let _lifecycle = lifecycle.lock().await;
            assert!(
                host.retire_session_environment_for_revocation(thread)
                    .await
                    .expect("revoke exact LegacyDirect owner"),
                "LegacyDirect owner enters Retiring"
            );
        }
        (environment, binding, sentinel)
    }
}
