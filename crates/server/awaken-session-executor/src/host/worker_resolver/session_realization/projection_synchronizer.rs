// @formal entrypoint session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
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
    /// Project claim-owned inputs at the same adapter boundary as directive
    /// inputs. The parent drives phases; this adapter alone maps claim fields.
    pub(super) fn for_claimed(
        host: &'a SharedHost,
        session_id: &str,
        directive: &awaken_session_realization_contract::SessionRealizationDirective,
        claimed: Option<&'a awaken_dispatch::Claimed>,
        claim: Option<&'a awaken_dispatch::RunClaim>,
    ) -> Self {
        let published_snapshot = claimed.and_then(|claimed| {
            (claimed.request.thread_id().0 == session_id)
                .then_some(&claimed.request.activation.snapshot)
        });
        let rebuild_unavailable_environment = claimed.is_some_and(|claimed| {
            claimed.request.placement.recovery
                == awaken_dispatch::WorkerRecoveryMode::RebuildFromCommittedTruth
        });
        let claimed_environment_binding = claimed.and_then(|claimed| claimed.sandbox.as_deref());
        Self::for_directive(
            host,
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
    pub(super) fn for_directive(
        host: &'a SharedHost,
        directive: &awaken_session_realization_contract::SessionRealizationDirective,
        claim: Option<&'a awaken_dispatch::RunClaim>,
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
    environment: &mut awaken_session_state_contract::SessionEnvironmentState,
    claimed_environment_binding: Option<&str>,
) {
    if let (awaken_session_state_contract::SessionEnvironmentState::Unmaterialized, Some(binding)) =
        (&*environment, claimed_environment_binding)
    {
        *environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: binding.to_string(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionProjectionSynchronizer
    for WorkerProjectionSynchronizer<'_>
{
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_authority_clock::authority_proof_started().map_err(|error| {
            awaken_session_error_contract::RunError::unavailable(format!(
                "authority clock: {error}"
            ))
        })
    }

    async fn synchronize_session_projection(
        &self,
        session_id: &awaken_session_types::SessionId,
        projection: &awaken_session_realization_contract::FrozenSessionProjection,
        lease: &awaken_session_realization_contract::ObservedSessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        let retained_publication = self
            .host
            .session_slots
            .read(session_id.as_str(), |slot| slot.published_snapshot.clone())
            .flatten();
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
            return Err(awaken_session_error_contract::RunError::classified(
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
                return Err(awaken_session_error_contract::RunError::classified(
                    "session_runtime_publication_missing",
                    "Worker Session realization requires its exact frozen Agent publication",
                ));
            }
            awaken_session_realization_contract::FrozenAgentPublicationDecision::Mismatch => {
                return Err(awaken_session_error_contract::RunError::classified(
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
            .update(session_id.as_str(), |slot| slot.lifecycle.clone());
        let lifecycle_guard = lifecycle.lock().await;
        if self.claim.is_some() {
            let (canonical_publication, model_candidate) = self
                .host
                .resolve_canonical_session_projection(
                    &projection.workspace_id,
                    crate::host::CanonicalSessionProjection::Baseline(&projection.baseline),
                    published_snapshot.cloned(),
                    Some(projection.resources.skills()),
                )
                .map_err(crate::managed_adapter_error::to_run_error)?;
            let a2a_only = canonical_publication.as_ref().is_some_and(|publication| {
                !crate::host::completion::requires_local_environment(&publication.resolved_spec)
            });
            let has_local_environment_owner = self
                .host
                .session_slots
                .read(session_id.as_str(), |slot| {
                    slot.environment_owner.has_local_environment()
                })
                .unwrap_or(false);
            let has_environment_binding = self.claimed_environment_binding.is_some()
                || projection.environment.binding().is_some();
            if a2a_only && (has_environment_binding || has_local_environment_owner) {
                return Err(awaken_session_error_contract::RunError::classified(
                    "session_runtime_remote_environment_conflict",
                    "remote A2A Session realization cannot consume a local Environment binding or owner",
                ));
            }
            if self.rebuild_unavailable_environment && !a2a_only {
                let provider = if let Some(candidate) = model_candidate.as_ref() {
                    self.host.session_environment_provider_for_thread(
                        session_id.as_str(),
                        candidate.provisioning(),
                    )
                } else {
                    self.host.session_environment_provider_for_thread(
                        session_id.as_str(),
                        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                    )
                }
                .map_err(crate::managed_adapter_error::to_run_error)?;
                self.host
                    .rebuild_claimed_legacy_environment_after_revocation(
                        session_id.as_str(),
                        provider,
                    )
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
                session_id.as_str(),
                projection.clone(),
                self.claim,
                synchronize_resources,
                Some(lease.clone()),
            )
            .await
            .map_err(crate::managed_adapter_error::to_run_error)?;
        // A Control-synchronized projection is the same complete Managed
        // Session authority as a co-located SessionRuntime installation. Mark
        // that existing ownership only after every frozen fact is installed;
        // otherwise the later claimed resolver mistakes an empty compatibility
        // envelope for ordinary desired state and drains Control's active MCP
        // generation.
        self.host
            .session_slots
            .update(session_id.as_str(), |slot| slot.session_dispatch = true);
        drop(lifecycle_guard);
        let published_snapshot = self
            .host
            .session_slots
            .read(session_id.as_str(), |slot| slot.published_snapshot.clone())
            .flatten();
        // A resident Arc is process-local ownership, not evidence that its
        // physical Sandbox survived. Revalidate every durable binding through
        // the one adoption path; Ready reuses that Arc, while closed evidence
        // follows the existing policy-fenced Rebuild decision.
        let has_environment_binding = projection.environment.binding().is_some();
        let runtime_authority_resident = self
            .host
            .session_slots
            .read(session_id.as_str(), |slot| {
                slot.runtime.is_some() || slot.environment_owner.is_resident()
            })
            .unwrap_or(false);
        if self.requires_runtime_before_effects
            && published_snapshot.is_none()
            && !runtime_authority_resident
        {
            return Err(awaken_session_error_contract::RunError::classified(
                "session_runtime_publication_missing",
                "sandbox stdio MCP realization requires the exact claimed Agent snapshot before effects",
            ));
        }
        let source_generation_id = projection
            .environment
            .generation()
            .map(|generation| generation.id.as_str());
        let adoption = if let Some(binding) = projection.environment.binding() {
            let provider = self
                .host
                .projected_session_environment_provider(session_id.as_str(), None)
                .map_err(crate::managed_adapter_error::to_run_error)?;
            self.host
                .adopt_bound_session_environment(
                    session_id.as_str(),
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
                    .durable_session_environment_binding(session_id.as_str())
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
                .session_child_execution_substrate(session_id.as_str(), published_snapshot.as_ref())
                .await
                .map_err(crate::managed_adapter_error::to_run_error)?;
            true
        } else {
            false
        };
        if synchronize_resources && !reconciled_by_substrate {
            self.host
                .apply_dispatched_resource_transition(
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
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        self.host
            .invalidate_session_realization_lease(session_id.as_str(), lease);
        Ok(())
    }
}

pub(in crate::host::worker_resolver) struct WorkerMcpEffects<'a>(
    pub(in crate::host::worker_resolver) &'a SharedHost,
);

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for WorkerMcpEffects<'_> {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_mcp_contract::StageMcpAttachment,
    ) -> Result<
        awaken_session_mcp_contract::McpRealizationReceipt,
        awaken_session_error_contract::RunError,
    > {
        self.0.stage_dispatched_mcp(request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        self.0.publish_dispatched_mcp(generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        self.0.drain_dispatched_mcp(generation).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{RecoveryControl, RecoveryEnvironmentBindingSink, frozen_projection};
    use super::*;
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, empty_frozen_projection_for_snapshot, test_activation,
    };
    use awaken_sandbox_local::FilesystemHandleCodec as _;
    use std::sync::Arc;

    /// Compatibility fixture for an aggregate-Unowned legacy claim. Returning
    /// `Unowned` keeps production creation on its marker-free V1 path; any
    /// attempted durable persist would prove that the scenario crossed into a
    /// different authority row.
    struct LegacyUnownedEnvironmentBindingSink;

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::SessionEnvironmentBindingSink
        for LegacyUnownedEnvironmentBindingSink
    {
        async fn authorize(
            &self,
            _intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
        ) -> Result<
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
            awaken_session_error_contract::RunError,
        > {
            Ok(awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned)
        }

        async fn persist(
            &self,
            _receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
        ) -> Result<
            awaken_session_state_contract::SessionEnvironmentState,
            awaken_session_error_contract::RunError,
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
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "requires Linux mount-namespace path fidelity"
    )]
    async fn claimed_ready_durable_revocation_adopts_a_fresh_hand_and_runtime() {
        claimed_ready_durable_replacement(false, true, false, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_revocation_rebinds_execution_not_only_hand() {
        // SR1: scoped Resident A + explicit revocation + a fresh Session epoch
        // + exact Ready durable binding => fresh executable B, same root/bytes;
        // A remains denied. A fresh Hand around A's closed execution binding is
        // insufficient. This reuses DH1's production projection/receipt path.
        claimed_ready_durable_replacement(true, true, false, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_new_epoch_replaces_resident_execution() {
        // SR2: scoped Resident A + no explicit revocation + new Session epoch
        // => stop A before publishing executable B; preserve the exact physical
        // binding. This is the warm approval/resume path, distinct from SR1.
        // Both rules assert real process admission, not merely wrapper identity.
        claimed_ready_durable_replacement(true, false, false, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_unknown_predecessor_blocks_replacement_and_retries() {
        // SR3: new epoch + indeterminate durable process observer => stop the
        // exact local tree but retain A as Retiring, old lease and root bytes;
        // do not publish a Candidate/new proof. Restore exact observer evidence
        // under this fixture's exclusive ownership => retry reaches SR2's B.
        claimed_ready_durable_replacement(true, false, true, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_committed_candidate_replaces_execution() {
        // SC1: Create returned, sink readback committed, local publication did
        // not happen; a new Session generation must retire that exact hidden
        // Candidate and adopt fresh execution over the same root. SC2: renewal
        // must retain its Arc and hidden phase; a foreign binding must neither
        // publish nor stop it. Reuse SR2's real execution and receipt oracles.
        claimed_ready_durable_replacement(
            true,
            false,
            false,
            InitialEnvironment::CommittedCandidate,
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_committed_candidate_unknown_then_retry() {
        // SC3: SC1 + unknown predecessor observation => retain exact Bound
        // Retiring owner, old authority, no Adopt receipt. Exact evidence on
        // retry => fresh executable successor, old Arc permanently denied.
        claimed_ready_durable_replacement(
            true,
            false,
            true,
            InitialEnvironment::CommittedCandidate,
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_committed_candidate_explicit_revocation() {
        // SC4: committed hidden Candidate + explicit revocation before new
        // projection => Bound retirement, not Unbound cleanup; ordinary Ready
        // adoption must still preserve the root and create fresh execution.
        claimed_ready_durable_replacement(
            true,
            true,
            false,
            InitialEnvironment::CommittedCandidate,
        )
        .await;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitialEnvironment {
        Resident,
        CommittedCandidate,
        BeforeCommit,
        LostResponse,
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_candidate_cancelled_before_commit_recovers() {
        // UC1: actual context Future is cancelled at the sink before root
        // commit. UC2: after root commit, only its response is lost (next test).
        // Cross direct epoch replacement, unknown observer then exact retry,
        // and explicit revocation. All rows must retain the sole physical root,
        // stop old execution, obtain a new authorized Adopt receipt, then
        // publish executable successor. No Create retry or raw cache may invent
        // commitment; same-generation renewal must preserve the hidden Arc.
        for (revoke, unknown) in [(false, false), (false, true), (true, false)] {
            claimed_ready_durable_replacement(
                true,
                revoke,
                unknown,
                InitialEnvironment::BeforeCommit,
            )
            .await;
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_candidate_lost_commit_response_recovers() {
        // UC2 crosses the same three UC1 schedules, but the sole Control root
        // already contains Create. Adoption must preserve its binding and the
        // old object must stay denied even though its initial receipt was lost.
        for (revoke, unknown) in [(false, false), (false, true), (true, false)] {
            claimed_ready_durable_replacement(
                true,
                revoke,
                unknown,
                InitialEnvironment::LostResponse,
            )
            .await;
        }
    }

    async fn claimed_ready_durable_replacement(
        scoped: bool,
        revoke: bool,
        unknown: bool,
        initial: InitialEnvironment,
    ) {
        claimed_ready_durable_replacement_chain(scoped, revoke, unknown, initial, &[]).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_successive_adopt_cancellations_recover() {
        // MC cause/effect graph: C1 A is Resident or an uncommitted Create;
        // C2 B's Adopt commits before cancellation; C3 C's Adopt commits before
        // cancellation. D is an uncancelled current successor. All 2x2x2 rules
        // are feasible: MC00/01/10/11 for each initial provenance. Actual
        // Pending Futures are dropped at the delegated sink, not simulated by
        // changing owner phases. E1 retain one exact physical root and bytes;
        // E2 every retired execution Arc is denied; E3 B/C remain unpublished;
        // E4 only committed prefixes plus D write receipts; E5 D's fresh
        // Runtime consumes only D. Same immutable Sandbox generation is not
        // proof that this Adopt committed. The fixture root is in-memory;
        // this is Host/real-process evidence, not distributed database CAS.
        // MC-M: a published filesystem tool selects filesystem delivery, not
        // semantic Memory. All eight schedules carry one writable Copy Memory
        // participant. Each actual scoped process reads its predecessor's
        // exact bytes before writing its own; cancelled adoption must not
        // rematerialize the seed or lose the materialization heads. The source
        // store stays unchanged until terminal reconciliation, outside this
        // live adoption test. This does not stand in for FUSE/Worker-death tests.
        for initial in [
            InitialEnvironment::Resident,
            InitialEnvironment::BeforeCommit,
        ] {
            for schedule in [[false, false], [false, true], [true, false], [true, true]] {
                claimed_ready_durable_replacement_chain(true, false, false, initial, &schedule)
                    .await;
            }
        }
    }

    async fn claimed_ready_durable_replacement_chain(
        scoped: bool,
        revoke: bool,
        unknown: bool,
        initial: InitialEnvironment,
        successor_cancellations: &[bool],
    ) {
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;
        let committed_candidate = initial != InitialEnvironment::Resident;
        let acknowledged = matches!(
            initial,
            InitialEnvironment::Resident | InitialEnvironment::CommittedCandidate
        );
        let persisted_creates = usize::from(initial != InitialEnvironment::BeforeCommit);

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
        #[cfg(target_os = "linux")]
        if scoped {
            raw_host.session_provider = crate::session_environment::SessionEnvironmentProvider::trusted_host_workdir_with_agent_stderr(storage.path(), false);
            raw_host.session_provider.install_workdir_hand(
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
                crate::acp_backend::deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER")
                    .to_string_lossy()
                    .into_owned(),
                std::time::Duration::ZERO,
            );
            raw_host
                .session_provider
                .install_scoped_execution(
                    awaken_sandbox_local::LocalExecutionDeployment::new(
                        crate::acp_backend::deployment_test_binary("AWAKEN_TEST_SCOPED_LAUNCHER"),
                        crate::acp_backend::deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER"),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        let host = Arc::new(raw_host.with_store_dir(storage.path()));
        let managed = crate::host::worker_resolver::test_support::complete_managed_test_host(
            crate::ManagedHost::new(host.clone())
                .with_resource_validator(crate::host::tests::test_resource_validator()),
        );
        let snapshot = test_activation(thread, "run-claimed-ready-durable").snapshot;
        let snapshot = if successor_cancellations.is_empty() {
            snapshot
        } else {
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .resolved_model(snapshot.resolved_spec.model_binding.clone())
                .tools(
                    crate::config::advertised_tools(&Default::default(), &Default::default(), &[])
                        .into_iter()
                        .filter(|tool| tool.id == "bash"),
                )
                .build()
        };
        let mut projection = frozen_projection();
        let memory_store = "replacement-chain-memory";
        if !successor_cancellations.is_empty() {
            crate::host::tests::install_test_memory_mounter(&host);
            host.memory_repository()
                .create(memory_store, "/note.txt", "seed")
                .await
                .unwrap();
            let resources = awaken_session_resource_contract::ResolvedSessionResources::try_new(
                vec![awaken_session_resource_contract::ResolvedInput {
                    created_at_unix_ms: None,
                    updated_at_unix_ms: None,
                    binding_id: "chain-memory".into(),
                    source: awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
                        memory_store_id: memory_store.into(),
                        config: awaken_resource_contract::MemoryStoreConfigVersion {
                            memory_store_id: memory_store.into(),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            retention_policy: Default::default(),
                        },
                    },
                    mount_path: "/mnt/memory".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadWrite,
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap();
            projection = crate::host::worker_resolver::test_support::frozen_projection_for_manifest(
                projection.baseline.environment.clone(),
                &awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    projection.workspace_id.clone(),
                    1,
                    resources,
                ),
            );
        }
        projection.agent_publication = Some(snapshot.clone());
        let control = RecoveryControl::fixed(projection.clone());
        let binding_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
        awaken_session_execution_contract::RealizationRuntime::install_environment_binding_sink(
            &managed,
            binding_sink.clone(),
        );
        let initial_lease = awaken_session_resource_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 1,
            expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                + 60_000,
        };
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &managed,
            thread,
            projection,
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease: crate::session_slot::test_observed_lease(initial_lease.clone()),
                prepare_session: true,
            },
        )
        .await
        .expect("DH1 install complete durable projection");
        if !successor_cancellations.is_empty() {
            assert_eq!(
                host.sandbox_spec(thread).mounts.len(),
                1,
                "MC-M complete frozen projection stages the Memory requirement before Create"
            );
        }
        let cached_runtime = if !acknowledged {
            let sink = Arc::new(
                crate::host::worker_resolver::test_support::BlockingBindingSink::around(
                    binding_sink.clone(),
                    initial == InitialEnvironment::LostResponse,
                ),
            );
            awaken_session_execution_contract::RealizationRuntime::install_environment_binding_sink(
                &managed,
                sink.clone(),
            );
            let creating = {
                let host = host.clone();
                let snapshot = snapshot.clone();
                tokio::spawn(async move {
                    host.ctx_for_snapshot(thread, Some("agent-a"), Some(snapshot))
                        .await
                })
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), sink.entered.notified())
                .await
                .expect("UC1/UC2 actual persistence boundary reached");
            let candidate = host.prepared_session_environment(thread).unwrap();
            assert!(
                candidate.durable_identity().is_none(),
                "UC no returned commit proof"
            );
            assert_eq!(
                binding_sink.calls(),
                persisted_creates,
                "UC root commit prefix"
            );
            creating.abort();
            match creating.await {
                Err(error) => assert!(error.is_cancelled()),
                Ok(_) => panic!("UC pending context unexpectedly finished"),
            }
            assert!(
                host.prepared_session_environment(thread)
                    .unwrap()
                    .exact_matches(&candidate)
            );
            awaken_session_execution_contract::RealizationRuntime::install_environment_binding_sink(
                &managed,
                binding_sink.clone(),
            );
            None
        } else if committed_candidate {
            // Stop at the production sink-readback/local-publication boundary.
            // No synthetic generation or Resident -> Candidate demotion: use
            // the canonical authorization, fenced Provider and retention edges.
            let lifecycle = host
                .session_slots
                .update(thread, |slot| slot.lifecycle.clone());
            let _lifecycle = lifecycle.lock().await;
            let effect = host
                .authorize_environment_effect_before_io(
                    thread,
                    awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
                    None,
                )
                .await
                .unwrap();
            let fence = effect.provider_fence.as_ref().unwrap();
            let authority = host
                .session_slots
                .provider_effect_authority(thread, fence, host.execution_authority.as_ref())
                .unwrap();
            let environment = Arc::new(
                host.session_provider
                    .create_effective_for_effect(
                        &host.sandbox_spec(thread),
                        crate::session_environment::ProviderEffect::Fenced {
                            fence,
                            authority: &authority,
                        },
                        None,
                        awaken_sandbox_container::ContainerRealizationIntent::Create,
                    )
                    .await
                    .unwrap(),
            );
            let candidate = host
                .begin_session_environment_preparation(thread, environment)
                .unwrap();
            let identity = host
                .persist_environment_before_publish(thread, &candidate)
                .await
                .unwrap();
            host.retain_committed_session_environment_candidate(thread, &candidate, identity)
                .unwrap();
            assert!(
                host.session_environment(thread).await.is_none(),
                "SC1 still hidden"
            );
            None
        } else {
            Some(
                host.ctx_for_snapshot(thread, Some("agent-a"), Some(snapshot.clone()))
                    .await
                    .expect("DH1/C1 build durable Runtime R1"),
            )
        };
        // This fixture also compiles for Namespace on non-Linux platforms.
        // Query the existing portable publication phases, not Linux's scoped
        // execution-authority view; renewal/conflict checks share this oracle.
        let owned_environment = || {
            host.session_slots
                .read(thread, |slot| {
                    slot.environment_owner.resident().or_else(|| {
                        slot.environment_owner
                            .unpublished_candidate()
                            .map(|candidate| candidate.environment.clone())
                    })
                })
                .flatten()
                .expect("DH1/SC1 exact owned Environment A")
        };
        let original = owned_environment();
        let original_handle = original.handle();
        let original_hand = original.tool_executor();
        let mut memory_bytes = "seed".to_string();
        if !successor_cancellations.is_empty() {
            scoped_memory_edit(&original, &memory_bytes, "generation-1").await;
            memory_bytes = "generation-1".into();
            assert_eq!(
                original_handle
                    .memory_materializations()
                    .unwrap()
                    .unwrap()
                    .len(),
                1
            );
        }
        let projection = control.current_projection();
        let binding = serde_json::to_string(&original_handle).unwrap();
        let claimed_binding = projection.environment.binding();
        if let Some(persisted) = claimed_binding {
            assert_eq!(persisted, binding, "DH1/UC2 exact root binding");
        }
        assert_eq!(
            binding_sink.calls(),
            persisted_creates,
            "DH1/UC root Create prefix"
        );
        let root_sentinel = storage
            .path()
            .join("sandboxes")
            .join(thread)
            .join("durable-root-sentinel");
        std::fs::write(&root_sentinel, b"same physical durable root")
            .expect("DH1/C2 root sentinel");
        if scoped {
            // SR4: same generation, later expiry => preserve A and its bound
            // execution capability. Renewal is NOT a replacement or shutdown.
            let mut renewed = initial_lease.clone();
            renewed.expires_at_unix_ms += 30_000;
            host.install_frozen_session_projection(
                thread,
                projection.clone(),
                None,
                true,
                Some(crate::session_slot::test_observed_lease(renewed.clone())),
            )
            .await
            .unwrap();
            assert!(Arc::ptr_eq(&original, &owned_environment()));
            assert_eq!(
                host.session_environment(thread).await.is_none(),
                committed_candidate,
                "SR4/SC2 renewal preserves publication phase"
            );
            assert_eq!(
                binding_sink.calls(),
                persisted_creates,
                "SR4 no adoption receipt"
            );
            scoped_admission(&original)
                .await
                .expect("SR initial execution is admitted");
        }
        let predecessor = if scoped {
            let mut command = awaken_provisioning_contract::Command::new([
                "/bin/sh",
                "-c",
                "setsid /bin/sleep 60 & /bin/sleep 60 & printf active > scope-ready; wait",
            ]);
            command.cwd = "/workspace".into();
            command.stdio = awaken_provisioning_contract::Stdio::Piped;
            let (process, channel) = original.spawn_agent(command).await.unwrap();
            let ready = root_sentinel
                .parent()
                .unwrap()
                .join("workspace/scope-ready");
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while std::fs::read(&ready).ok().as_deref() != Some(b"active") {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(process.poll().await.unwrap().is_none());
            Some((process, channel))
        } else {
            None
        };
        let lifecycle = host
            .session_slots
            .read(thread, |slot| slot.lifecycle.clone())
            .expect("DH1 lifecycle owner");
        if revoke {
            let _lifecycle = lifecycle.lock().await;
            // UC3: repeat after Live -> Closed. Closed provenance must retain
            // the same Candidate; it is not permission to delete its root.
            for _ in 0..2 {
                assert!(
                    host.retire_session_environment_for_revocation(thread)
                        .await
                        .expect("DH1/C2 revoke exact durable owner"),
                    "DH1/C2 owner enters Retiring"
                );
            }
        }
        assert!(
            !revoke
                || host
                    .session_slots
                    .read(thread, |slot| slot.runtime.is_none()
                        && !slot.environment_owner.is_resident())
                    .unwrap_or(false),
            "DH1/C2 revocation clears R1 and hides Retiring A"
        );

        // CR4 projection publication now carries the actual Dispatch claim.
        // Use its authoritative queue row, not a fabricated tuple that would
        // bypass the deferred binding fence when the slot had no claim.
        use awaken_dispatch::DispatchQueue as _;
        let dispatch = host.dispatch_store().unwrap();
        dispatch
            .enqueue(awaken_dispatch::RunDispatch::new(test_activation(
                thread,
                "run-claimed-ready-durable",
            )))
            .await
            .unwrap();
        let claimed = dispatch
            .claim("worker-a", 300_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let claim = awaken_dispatch::RunClaim::from(&claimed.lease);
        let lease = awaken_session_resource_contract::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-2".into(),
            epoch: 2,
            expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                + 60_000,
        };
        let directive = awaken_session_realization_contract::SessionRealizationDirective {
            projection: projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
            action: awaken_session_realization_contract::SessionRealizationAction::Complete,
        };
        if scoped && !revoke {
            // SR5/SC2: a newer root with a foreign binding OR generation is not retirement
            // authority. Validate the prospective owner before any tree stop;
            // final publication validation alone would reject too late.
            for foreign_generation in [false, true] {
                // An unacknowledged Candidate knows its physical handle, not
                // a Store-generated generation; do not fabricate that proof.
                if foreign_generation && !acknowledged {
                    continue;
                }
                let mut foreign = projection.clone();
                foreign.revision.0 += 1;
                if foreign.environment.binding().is_none() {
                    foreign.environment =
                        awaken_session_state_contract::SessionEnvironmentState::Resident {
                            binding: binding.clone(),
                            effect_id: None,
                            generation: None,
                            idle_since_unix_ms: None,
                        };
                }
                if let awaken_session_state_contract::SessionEnvironmentState::Resident {
                    binding,
                    generation,
                    ..
                } = &mut foreign.environment
                {
                    if foreign_generation {
                        generation.as_mut().unwrap().id.push_str("-foreign");
                    } else {
                        binding.push_str("-foreign");
                    }
                } else {
                    panic!("SR5 Resident projection")
                }
                assert!(
                    host.install_frozen_session_projection(
                        thread,
                        foreign,
                        None,
                        true,
                        Some(crate::session_slot::test_observed_lease(lease.clone()))
                    )
                    .await
                    .is_err()
                );
                assert!(Arc::ptr_eq(&original, &owned_environment()));
                assert_eq!(
                    host.session_environment(thread).await.is_none(),
                    committed_candidate,
                    "SR5/SC2 foreign projection preserves publication phase"
                );
                assert!(
                    predecessor
                        .as_ref()
                        .unwrap()
                        .0
                        .poll()
                        .await
                        .unwrap()
                        .is_none()
                );
                scoped_admission(&original)
                    .await
                    .expect("SR5 valid prior execution was not retired");
                assert_eq!(binding_sink.calls(), persisted_creates);
            }
        }
        if unknown {
            let marker = root_sentinel
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!(".{thread}.awaken-realization.json"));
            let original_marker = std::fs::read(&marker).unwrap();
            let mut foreign: serde_json::Value = serde_json::from_slice(&original_marker).unwrap();
            for execution in foreign["executions"].as_array_mut().unwrap() {
                execution["boot_id"] = serde_json::to_value([7_u8; 16]).unwrap();
            }
            std::fs::write(&marker, serde_json::to_vec(&foreign).unwrap()).unwrap();
            let result = WorkerProjectionSynchronizer::for_directive(
                host.as_ref(),
                &directive,
                Some(&claim),
                Some(&snapshot),
                true,
                claimed_binding,
            )
            .synchronize_session_projection(
                &awaken_session_types::SessionId::from(thread),
                &projection,
                &crate::session_slot::test_observed_lease(lease.clone()),
                true,
            )
            .await;
            assert!(
                result.is_err(),
                "SR3 unknown tree cannot authorize replacement"
            );
            host.session_slots
                .read(thread, |slot| {
                    assert!(matches!(&slot.environment_owner,
                    crate::session_slot::SessionEnvironmentOwner::Retiring(retiring)
                    if Arc::ptr_eq(&retiring.owned.environment(), &original)));
                    assert_eq!(slot.realization_authority.lease().unwrap().epoch, 1);
                    assert!(slot.runtime.is_none());
                })
                .unwrap();
            assert_eq!(
                binding_sink.calls(),
                persisted_creates,
                "SR3 no Adopt receipt"
            );
            assert_eq!(
                std::fs::read(&root_sentinel).unwrap(),
                b"same physical durable root"
            );
            std::fs::write(&marker, original_marker).unwrap();
        }
        let mut retired = vec![original.clone()];
        for round in 0..=successor_cancellations.len() {
            let projection = control.current_projection();
            let lease = awaken_session_resource_contract::SessionRealizationLease {
                epoch: 2 + round as u64,
                runtime_incarnation: format!("worker-a/boot-{}", 2 + round),
                ..lease.clone()
            };
            let directive = awaken_session_realization_contract::SessionRealizationDirective {
                projection: projection.clone(),
                authority: crate::session_slot::test_lease_receipt(lease.clone()),
                action: awaken_session_realization_contract::SessionRealizationAction::Complete,
            };
            let cancelled_sink = successor_cancellations.get(round).map(|commit| {
                Arc::new(
                    crate::host::worker_resolver::test_support::BlockingBindingSink::around(
                        binding_sink.clone(),
                        *commit,
                    ),
                )
            });
            if let Some(sink) = &cancelled_sink {
                awaken_session_execution_contract::RealizationRuntime::install_environment_binding_sink(
                    &managed,
                    sink.clone(),
                );
            }
            {
                let recovering = async {
                    WorkerProjectionSynchronizer::for_directive(
                        host.as_ref(),
                        &directive,
                        Some(&claim),
                        Some(&snapshot),
                        true,
                        projection.environment.binding(),
                    )
                    .synchronize_session_projection(
                        &awaken_session_types::SessionId::from(thread),
                        &projection,
                        &crate::session_slot::test_observed_lease(lease.clone()),
                        true,
                    )
                    .await
                    .expect("DH1/MC canonical claimed adoption");
                    if projection.environment.binding().is_none() {
                        // UC1 has no invented dispatch binding: ordinary lazy
                        // Tool entry recovers the retained physical owner.
                        host.ensure_session_environment_for_tool(thread)
                            .await
                            .expect("UC1/MC ordinary lazy Environment recovery");
                    }
                };
                tokio::pin!(recovering);
                if let Some(sink) = &cancelled_sink {
                    tokio::select! {
                        _ = &mut recovering => panic!("MC expected pending Adopt"),
                        result = tokio::time::timeout(
                            std::time::Duration::from_secs(5), sink.entered.notified(),
                        ) => result.expect("MC actual Adopt persistence boundary"),
                    }
                    assert_eq!(sink.calls(), 1, "MC one pending Adopt attempt");
                } else {
                    recovering.await;
                }
                // Drop the borrowed future here, releasing the actual locks.
            }
            let after = control.current_projection();
            if successor_cancellations.get(round) == Some(&false) {
                assert_eq!(
                    after.environment, projection.environment,
                    "MC no root commit"
                );
            } else {
                assert_ne!(
                    after.environment.effect_id(),
                    projection.environment.effect_id(),
                    "MC each committed Adopt has its own receipt, including final recovery"
                );
                assert_eq!(after.environment.binding(), Some(binding.as_str()));
                if let Some(generation) = projection.environment.generation() {
                    assert_eq!(
                        after.environment.generation(),
                        Some(generation),
                        "MC Adopt never rotates the physical Sandbox generation"
                    );
                }
            }
            if let Some(sink) = cancelled_sink {
                let candidate = host
                    .prepared_session_environment(thread)
                    .expect("MC cancelled successor retains Candidate");
                assert!(
                    host.session_environment(thread).await.is_none(),
                    "MC hidden owner"
                );
                assert_eq!(candidate.environment.handle(), original_handle);
                for old in &retired {
                    assert!(!Arc::ptr_eq(old, &candidate.environment));
                    assert!(
                        scoped_admission(old).await.is_err(),
                        "MC retired Arc stays denied"
                    );
                }
                scoped_admission(&candidate.environment)
                    .await
                    .expect("MC current Candidate execution remains valid before replacement");
                let next_bytes = format!("generation-{}", claim.epoch);
                scoped_memory_edit(&candidate.environment, &memory_bytes, &next_bytes).await;
                memory_bytes = next_bytes;
                assert_eq!(
                    binding_sink.calls(),
                    persisted_creates
                        + successor_cancellations[..=round]
                            .iter()
                            .filter(|commit| **commit)
                            .count()
                );
                assert_eq!(
                    std::fs::read(&root_sentinel).unwrap(),
                    b"same physical durable root"
                );
                retired.push(candidate.environment);
                awaken_session_execution_contract::RealizationRuntime::install_environment_binding_sink(
                    &managed,
                    binding_sink.clone(),
                );
                drop(sink);
            }
        }

        let rebuilt = host
            .session_environment(thread)
            .await
            .expect("DH1/E1-E2 fresh Resident Environment B");
        let rebuilt_hand = rebuilt.tool_executor();
        if !successor_cancellations.is_empty() {
            scoped_memory_edit(&rebuilt, &memory_bytes, "generation-final").await;
            assert_eq!(
                std::fs::read(root_sentinel.parent().unwrap().join("mnt/memory/note.txt")).unwrap(),
                b"generation-final",
                "MC-M exact local Copy bytes survive every cancelled adoption"
            );
            assert_eq!(
                host.memory_repository()
                    .get_by_path(memory_store, "/note.txt")
                    .await
                    .unwrap()
                    .unwrap()
                    .content
                    .as_deref(),
                Some("seed"),
                "MC-M live adoption does not invent terminal writeback"
            );
        }
        if scoped {
            assert!(
                predecessor
                    .as_ref()
                    .unwrap()
                    .0
                    .poll()
                    .await
                    .unwrap()
                    .is_some(),
                "SR predecessor tree stops before successor execution"
            );
            for old in &retired {
                assert!(
                    scoped_admission(old).await.is_err(),
                    "SR/MC old execution stays denied"
                );
            }
            scoped_admission(&rebuilt)
                .await
                .expect("SR fresh execution must be admitted");
        }
        assert!(!Arc::ptr_eq(&original, &rebuilt), "DH1/E2 fresh wrapper");
        assert!(
            !Arc::ptr_eq(&original_hand, &rebuilt_hand),
            "DH1/E2 fresh Hand"
        );
        assert_eq!(rebuilt.handle(), original_handle, "DH1/E3 typed handle");
        assert!(root_sentinel.exists(), "DH1/E3 root sentinel survives");
        assert_eq!(
            std::fs::read(&root_sentinel).unwrap(),
            b"same physical durable root"
        );
        assert_eq!(
            binding_sink.calls(),
            persisted_creates
                + successor_cancellations
                    .iter()
                    .filter(|commit| **commit)
                    .count()
                + 1,
            "DH1/UC/MC committed prefixes plus final canonical Adopt"
        );

        let effective_model_ref = snapshot
            .resolved_spec
            .model_binding
            .binding()
            .identity()
            .1
            .to_owned();
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
        if let Some(cached_runtime) = cached_runtime {
            assert!(
                !Arc::ptr_eq(&cached_runtime, &fresh_runtime),
                "DH1/E4 R2 cannot reuse revoked R1"
            );
        }
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

    async fn scoped_memory_edit(
        environment: &crate::session_environment::SessionEnvironment,
        before: &str,
        after: &str,
    ) {
        // Literal fixture bytes are positional arguments, never shell source.
        let view = environment.agent_filesystem_view(
            None,
            crate::session_environment::AgentFilesystemClient::AcpProcess,
        );
        let handle = environment.handle();
        let materializations = handle.memory_materializations().unwrap().unwrap();
        assert_eq!(materializations.len(), 1, "MC-M exact Copy participant");
        assert_eq!(materializations[0].mount_path, "/mnt/memory");
        let rendered = view.prompt_paths.render("/mnt/memory/note.txt");
        let mut command = awaken_provisioning_contract::Command::new([
            "/bin/sh",
            "-c",
            "test \"$(cat \"$3\")\" = \"$1\" && printf %s \"$2\" > \"$3\"",
            "memory-chain",
            before,
            after,
            &rendered,
        ]);
        command.cwd = "/workspace".into();
        command.stdio = awaken_provisioning_contract::Stdio::Piped;
        let (process, _channel) = environment.spawn_agent(command).await.unwrap();
        assert_eq!(
            process.wait().await.unwrap().code,
            Some(0),
            "MC-M real Memory read/write before {after}"
        );
        // The provider creates the process cwd at launch. Observe from the
        // host AFTER that operation; a lexical cwd/../mount path need not be
        // traversable before the first process. The child checked prior bytes.
        assert_eq!(
            std::fs::read(std::path::Path::new(&view.protocol_cwd).join(&rendered)).unwrap(),
            after.as_bytes(),
            "MC-M independent physical Memory bytes after {after}"
        );
    }

    async fn scoped_admission(
        environment: &crate::session_environment::SessionEnvironment,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let mut command = awaken_provisioning_contract::Command::new(["/usr/bin/true"]);
        command.cwd = "/workspace".into();
        command.stdio = awaken_provisioning_contract::Stdio::Piped;
        let (process, _channel) = environment.spawn_agent(command).await?;
        assert_eq!(
            process.wait().await?.code,
            Some(0),
            "real scoped process exits"
        );
        Ok(())
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
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;

        let thread = "claimed-ready-v1-preinstall-rebuild";
        let storage = tempfile::tempdir().expect("legacy rebuild storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        awaken_session_execution_contract::RealizationRuntime::install_environment_binding_sink(
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

        // CR4 the legacy recovery fixture must also exercise real binding CAS,
        // not an unregistered claim previously hidden by the empty local slot.
        use awaken_dispatch::DispatchQueue as _;
        let dispatch = host.dispatch_store().unwrap();
        dispatch
            .enqueue(awaken_dispatch::RunDispatch::new(test_activation(
                thread,
                "run-claimed-ready-v1",
            )))
            .await
            .unwrap();
        let claimed = dispatch
            .claim("worker-a", 300_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let claim = awaken_dispatch::RunClaim::from(&claimed.lease);
        let lease = awaken_session_resource_contract::SessionRealizationLease {
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
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &projection,
            &crate::session_slot::test_observed_lease(lease.clone()),
            true,
        )
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
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;

        let thread = "claimed-remote-a2a-retiring-local";
        let storage = tempfile::tempdir().expect("remote A2A guard storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let local_snapshot = test_activation(thread, "run-local-owner").snapshot;
        let mut local_projection = frozen_projection();
        local_projection.agent_publication = Some(local_snapshot.clone());
        let cached_lease = awaken_session_resource_contract::SessionRealizationLease {
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
            Some(crate::session_slot::test_observed_lease(
                cached_lease.clone(),
            )),
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
                    slot.realization_authority.lease().cloned(),
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
        let claim = awaken_dispatch::RunClaim {
            run_id: RunId("run-remote-a2a-owner-conflict".into()),
            owner: "worker-a".into(),
            epoch: 2,
        };
        let lease = awaken_session_resource_contract::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-2".into(),
            epoch: claim.epoch,
            expires_at_unix_ms: u64::MAX,
        };
        let directive = awaken_session_realization_contract::SessionRealizationDirective {
            projection: remote_projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
            action: awaken_session_realization_contract::SessionRealizationAction::Complete,
        };
        let error = WorkerProjectionSynchronizer::for_directive(
            host.as_ref(),
            &directive,
            Some(&claim),
            Some(&remote_snapshot),
            true,
            None,
        )
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &remote_projection,
            &crate::session_slot::test_observed_lease(lease.clone()),
            true,
        )
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
                    slot.realization_authority.lease().cloned(),
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
