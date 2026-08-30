//! Worker projection synchronization and physical MCP effect adapters.
//!
//! The canonical realization phase driver remains in the parent module. This
//! private split only projects its frozen directive through the one
//! `WorkerProjectionSynchronizer` and the existing Host MCP effects.

use super::*;

pub(super) struct WorkerProjectionSynchronizer<'a> {
    pub(super) host: &'a SharedHost,
    pub(super) claim: Option<&'a awaken_run_ingress::RunClaim>,
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
        claim: Option<&'a awaken_run_ingress::RunClaim>,
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

    /// Claimed durable-Hand rebuild cause/effect graph: C1 an exact durable
    /// Environment A and Runtime R1 are Resident; C2 realization revocation
    /// closes A's Hand and clears R1 while preserving the physical Sandbox; C3
    /// a claimed Rebuild realization projects the same durable binding and the
    /// provider reports Ready. Effects: E1 the existing synchronizer moves only
    /// exact `Retiring(RealizationRevocation, Durable)` through canonical
    /// adoption; E2 Environment B and its Hand are fresh Arcs; E3 B retains A's
    /// exact typed handle/root and root sentinel; E4 the final claimed resolver
    /// installs a fresh Runtime R2 with B as the sole Resident owner.
    ///
    /// | Rule | claim | policy | owner | observation | Effect |
    /// |---|---|---|---|---|---|
    /// | DH1 | yes | Rebuild | exact Durable revocation | Ready | E1-E4 |
    /// | DH2 | no | any | exact Durable revocation | Ready | no claimed transition |
    /// | DH3 | yes | Continuity | foreign/non-Ready | any | retain exact fence |
    #[tokio::test]
    async fn claimed_ready_durable_revocation_adopts_a_fresh_hand_and_runtime() {
        use awaken_session_contract::SessionProjectionSynchronizer as _;

        let thread = "claimed-ready-durable-hand-rebuild";
        let storage = tempfile::tempdir().expect("durable Hand storage");
        let mut raw_host = SharedHost::new(Arc::new(AdoptionModel), "stub");
        raw_host.session_provider =
            crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
                storage.path(),
                false,
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            );
        let host = Arc::new(raw_host.with_store_dir(storage.path()));
        let managed = managed_test_host(host.clone());
        let snapshot = test_activation(thread, "run-claimed-ready-durable").snapshot;
        let mut projection = frozen_projection();
        projection.agent_publication = Some(snapshot.clone());
        let control = RecoveryControl::fixed(projection.clone());
        let binding_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
        awaken_session_contract::SessionRuntime::install_environment_binding_sink(
            &managed,
            binding_sink.clone(),
        );
        let initial_lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        awaken_session_contract::SessionRuntime::install_session_projection(
            &managed,
            thread,
            projection,
            awaken_session_contract::SessionProjectionInstallMode::Realization {
                lease: initial_lease,
                prepare_session: true,
            },
        )
        .await
        .expect("DH1 install complete durable projection");
        let cached_runtime = host
            .ctx_for_snapshot(thread, Some("agent-a"), Some(snapshot.clone()))
            .await
            .expect("DH1/C1 build durable Runtime R1");
        let original = host
            .session_environment(thread)
            .await
            .expect("DH1/C1 durable Resident Environment A");
        let original_handle = original.handle();
        let original_hand = original.tool_executor();
        let projection = control.current_projection();
        let binding = projection
            .environment
            .binding()
            .expect("DH1/C1 Store-read durable binding")
            .to_string();
        assert_eq!(binding_sink.calls(), 1, "DH1/C1 one durable Create");
        let root_sentinel = storage
            .path()
            .join("sandboxes")
            .join(thread)
            .join("durable-root-sentinel");
        std::fs::write(&root_sentinel, b"same physical durable root")
            .expect("DH1/C2 root sentinel");
        let lifecycle = host
            .session_slots
            .read(thread, |slot| slot.lifecycle.clone())
            .expect("DH1 lifecycle owner");
        {
            let _lifecycle = lifecycle.lock().await;
            assert!(
                host.retire_session_environment_for_revocation(thread)
                    .await
                    .expect("DH1/C2 revoke exact durable owner"),
                "DH1/C2 owner enters Retiring"
            );
        }
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.is_none()
                    && !slot.environment_owner.is_resident())
                .unwrap_or(false),
            "DH1/C2 revocation clears R1 and hides Retiring A"
        );

        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-claimed-ready-durable".into()),
            owner: "worker-a".into(),
            epoch: 2,
        };
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-2".into(),
            epoch: claim.epoch,
            expires_at_unix_ms: u64::MAX,
        };
        let directive = awaken_session_contract::SessionRealizationDirective {
            projection: projection.clone(),
            lease: lease.clone(),
            action: awaken_session_contract::SessionRealizationAction::Complete,
        };
        WorkerProjectionSynchronizer::for_directive(
            host.as_ref(),
            &directive,
            Some(&claim),
            Some(&snapshot),
            true,
            Some(&binding),
        )
        .synchronize_session_projection(thread, &projection, &lease, true)
        .await
        .expect("DH1/E1 canonical claimed adoption");

        let rebuilt = host
            .session_environment(thread)
            .await
            .expect("DH1/E1-E2 fresh Resident Environment B");
        let rebuilt_hand = rebuilt.tool_executor();
        assert!(!Arc::ptr_eq(&original, &rebuilt), "DH1/E2 fresh wrapper");
        assert!(
            !Arc::ptr_eq(&original_hand, &rebuilt_hand),
            "DH1/E2 fresh Hand"
        );
        assert_eq!(rebuilt.handle(), original_handle, "DH1/E3 typed handle");
        assert!(root_sentinel.exists(), "DH1/E3 root sentinel survives");
        assert_eq!(binding_sink.calls(), 2, "DH1/E1 one canonical Adopt");

        let effective_model_ref = snapshot
            .resolved_spec
            .model_binding
            .binding()
            .model_ref
            .clone();
        let publications = Arc::new(
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot.clone()])
                .expect("DH1 claimed publication source"),
        );
        let attempt = crate::host::session_ctx::ClaimedRuntimeInput {
            identity: crate::host::session_ctx::RuntimePublicationIdentity::from_publications(
                &snapshot,
                &[],
                &effective_model_ref,
            ),
            publications,
            effective_model_ref,
        };
        let fresh_runtime = host
            .ctx_for_claimed_snapshot(thread, Some("agent-a"), snapshot, attempt)
            .await
            .expect("DH1/E4 claimed Runtime R2");
        assert!(
            !Arc::ptr_eq(&cached_runtime, &fresh_runtime),
            "DH1/E4 R2 cannot reuse revoked R1"
        );
        assert!(
            fresh_runtime
                .env
                .as_ref()
                .is_some_and(|environment| Arc::ptr_eq(environment, &rebuilt)),
            "DH1/E4 R2 consumes only Resident B"
        );
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.as_ref().is_some_and(
                    |runtime| {
                        Arc::ptr_eq(runtime, &fresh_runtime) && slot.environment_owner.is_resident()
                    }
                ))
                .unwrap_or(false),
            "DH1/E4 fresh Runtime and Resident are published together"
        );
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

    /// Claimed legacy-rebuild cause/effect graph: C1 an exact claim selects
    /// RebuildFromCommittedTruth; C2 its slot owns a Ready, revoked
    /// LegacyDirect V1 Environment A; C3 the delivery cache carries A's raw
    /// binding while aggregate truth is still Unmaterialized. Effects: E1
    /// dispose and confirm A before raw merge/projection installation; E2 the
    /// resulting closed adoption follows the existing RebuildRequired path; E3
    /// install a fresh Environment B and Hand without a retiring-identity
    /// conflict; E4 preserve V1 compatibility where A and B have equal handles
    /// but distinct Arcs, so no handle-only filter may discard B.
    ///
    /// | Rule | claim | policy | revoked observation | raw binding | Effect |
    /// |---|---|---|---|---|---|
    /// | CR1 | yes | Rebuild | Ready V1 | A | E1 + E2 + E3 + E4 |
    /// | CR2 | no | any | any | any | no claimed cleanup (merge unit L1-L4) |
    /// | CR3 | yes | Continuity | any | any | no destructive recovery |
    /// | CR4 | yes | Rebuild | closed/error | A | typed lifecycle table CP3 |
    #[tokio::test]
    async fn claimed_rebuild_disposes_legacy_v1_before_projection_install() {
        use awaken_session_contract::SessionProjectionSynchronizer as _;

        let thread = "claimed-ready-v1-preinstall-rebuild";
        let storage = tempfile::tempdir().expect("legacy rebuild storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        awaken_session_contract::SessionRuntime::install_environment_binding_sink(
            &managed,
            Arc::new(LegacyUnownedEnvironmentBindingSink),
        );

        let snapshot = test_activation(thread, "run-claimed-ready-v1").snapshot;
        let mut projection = frozen_projection();
        projection.agent_publication = Some(snapshot.clone());
        host.install_frozen_session_projection(thread, projection.clone(), None, true, None)
            .await
            .expect("CR1 install exact frozen publication before the legacy owner");
        let (original, raw_binding, retired_sentinel) =
            revoked_legacy_v1_environment(host.as_ref(), thread).await;
        let original_handle = original.handle();
        let original_hand = original.tool_executor();

        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-claimed-ready-v1".into()),
            owner: "worker-a".into(),
            epoch: 1,
        };
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: claim.epoch,
            expires_at_unix_ms: u64::MAX,
        };
        WorkerProjectionSynchronizer {
            host: host.as_ref(),
            claim: Some(&claim),
            published_snapshot: Some(&snapshot),
            rebuild_unavailable_environment: true,
            requires_runtime_before_effects: false,
            claimed_environment_binding: Some(&raw_binding),
        }
        .synchronize_session_projection(thread, &projection, &lease, true)
        .await
        .expect("CR1/E1-E3 cleanup precedes merge and projection installation");

        let rebuilt = host
            .session_environment(thread)
            .await
            .expect("CR1/E3 fresh Resident Environment B");
        assert!(!Arc::ptr_eq(&original, &rebuilt), "CR1/E3 fresh Arc");
        assert_eq!(
            original_handle,
            rebuilt.handle(),
            "CR1/E4 V1 recreates the same logical handle"
        );
        assert!(
            !Arc::ptr_eq(&original_hand, &rebuilt.tool_executor()),
            "CR1/E3 same-id recreation publishes a fresh Hand"
        );
        assert!(
            !retired_sentinel.exists(),
            "CR1/E1 old root is disposed before same-id recreation"
        );
    }

    /// Remote claimed-realization cause/effect graph: C1 a claimed rebuild
    /// carries an A2A-only publication; C2 the slot retains an exact Ready,
    /// revoked local LegacyDirect owner; C3 neither the aggregate projection nor
    /// raw delivery cache supplies a local binding. Effects: E1 classify the
    /// conflict before legacy disposal, projection/Resource installation, or MCP
    /// eligibility; E2 retain the exact Retiring Arc, Ready root sentinel, frozen
    /// publication/lease caches, and Resource/MCP projections unchanged.
    ///
    /// | Rule | claim | route | local input | policy | Effect |
    /// |---|---|---|---|---|---|
    /// | RA1 | yes | A2A-only | Retiring owner | Rebuild | E1 + E2 |
    /// | RA2 | yes | A2A-only | none | either | ordinary remote realization |
    /// | RA3 | yes | local-capable | Retiring LegacyDirect | Rebuild | CR1 |
    #[tokio::test]
    async fn claimed_remote_a2a_rejects_a_retiring_local_owner_before_any_effect() {
        use awaken_session_contract::SessionProjectionSynchronizer as _;

        let thread = "claimed-remote-a2a-retiring-local";
        let storage = tempfile::tempdir().expect("remote A2A guard storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let local_snapshot = test_activation(thread, "run-local-owner").snapshot;
        let mut local_projection = frozen_projection();
        local_projection.agent_publication = Some(local_snapshot.clone());
        let cached_lease = awaken_session_contract::SessionRealizationLease {
            owner: "previous-worker".into(),
            runtime_incarnation: "previous-worker/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        host.install_frozen_session_projection(
            thread,
            local_projection,
            None,
            true,
            Some(cached_lease.clone()),
        )
        .await
        .expect("RA1 install retained local projection");
        let (retiring, _raw_binding, root_sentinel) =
            revoked_legacy_v1_environment(host.as_ref(), thread).await;
        let cached = host
            .session_slots
            .read(thread, |slot| {
                (
                    slot.published_snapshot.clone(),
                    slot.realization_lease.clone(),
                    slot.environment_projection
                        .as_ref()
                        .map(|projection| projection.fingerprint.clone()),
                    slot.mcp.len(),
                )
            })
            .expect("RA1 retained slot caches");
        let cached_resources = host.thread_resource_manifest(thread);

        let remote_snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "remote-agent",
                "",
                "a2a:https://agent.example.test",
            ))
            .build();
        let mut remote_projection = empty_frozen_projection_for_snapshot(
            "workspace",
            eager_environment(),
            &remote_snapshot,
        );
        remote_projection.agent_publication = Some(remote_snapshot.clone());
        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-remote-a2a-owner-conflict".into()),
            owner: "worker-a".into(),
            epoch: 2,
        };
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-2".into(),
            epoch: claim.epoch,
            expires_at_unix_ms: u64::MAX,
        };
        let directive = awaken_session_contract::SessionRealizationDirective {
            projection: remote_projection.clone(),
            lease: lease.clone(),
            action: awaken_session_contract::SessionRealizationAction::Complete,
        };
        let error = WorkerProjectionSynchronizer::for_directive(
            host.as_ref(),
            &directive,
            Some(&claim),
            Some(&remote_snapshot),
            true,
            None,
        )
        .synchronize_session_projection(thread, &remote_projection, &lease, true)
        .await
        .expect_err("RA1/E1 remote A2A must fail before local effects");
        assert_eq!(
            error.code, "session_runtime_remote_environment_conflict",
            "RA1/E1 classified fail-closed"
        );

        assert_eq!(
            retiring.status().await.expect("RA1 retained owner status"),
            awaken_provisioning_contract::SandboxStatus::Ready,
            "RA1/E2 rebuild disposal never ran"
        );
        assert!(root_sentinel.exists(), "RA1/E2 physical root is untouched");
        assert!(
            host.session_slots
                .read(thread, |slot| {
                    matches!(
                        &slot.environment_owner,
                        crate::session_slot::SessionEnvironmentOwner::Retiring(current)
                            if current.cause
                                == crate::session_slot::SessionEnvironmentRetirementCause::RealizationRevocation
                                && Arc::ptr_eq(&current.owned.environment(), &retiring)
                    )
                })
                .unwrap_or(false),
            "RA1/E2 exact Retiring owner remains fenced"
        );
        assert_eq!(
            host.session_slots.read(thread, |slot| {
                (
                    slot.published_snapshot.clone(),
                    slot.realization_lease.clone(),
                    slot.environment_projection
                        .as_ref()
                        .map(|projection| projection.fingerprint.clone()),
                    slot.mcp.len(),
                )
            }),
            Some(cached),
            "RA1/E2 publication, lease, Environment, and MCP caches are unchanged"
        );
        assert_eq!(
            host.thread_resource_manifest(thread),
            cached_resources,
            "RA1/E2 Resource projection is unchanged"
        );
    }
}
