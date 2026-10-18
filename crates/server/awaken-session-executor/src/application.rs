// @formal entrypoint capture.persistence_redaction_selection_is_total_exact_and_non_widening
// @formal entrypoint session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
// @formal entrypoint session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal entrypoint session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal entrypoint session_resources.terminal_assignment_retains_desired_generation
//! Claim-bound Runtime projections of one frozen Session.
//!
//! The host remains the sole owner of Session realization. An embedding
//! application supplies no late desired state: mounts, environment values,
//! prompts, and MCP generations come only from the frozen Control projection
//! and are installed into the same Session slot for the Native/ACP backend path.

mod agent_filesystem_context;
pub(crate) mod attempt_context;
mod realization_renewal;
mod resource_transition;

use awaken_session_configuration_contract::environment_projection;
use awaken_session_realization::peers::EnvironmentPeer as _;
use std::sync::Arc;

use crate::resource_staging::ResourcePreparation;
use awaken_dispatch_contract::RunClaim;
use awaken_session_realization_contract::CanonicalSessionProjection;
use awaken_session_realization_contract::{
    FrozenSessionProjection, ObservedSessionRealizationLease,
};

#[cfg(test)]
pub(crate) use realization_renewal::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS;
use resource_transition::{
    FrozenResourceProjectionMode, FrozenResourceTransitionInstallDecision,
    frozen_resource_transition_install_decision,
};

/// Already-closed process-local projections awaiting nonterminal cleanup.
/// Dropping this value cannot reopen admission or certify cleanup. A retry
/// reselects the retained slot owners through the same Host operation.
#[must_use = "closed projections still require cleanup or an explicit retry"]
pub struct SessionRealizationRevocation {
    selected: Vec<(String, crate::installed_session::ClosedSessionRealization)>,
}

#[cfg(kani)]
/// @obligation session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
/// @obligation session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
/// @obligation session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
#[kani::proof]
fn session_realization_renewal_failure_disposition_is_total_exact_and_fail_closed() {
    use awaken_session_realization_contract::SessionRealizationControlFailure;

    let selector = kani::any::<u8>() % 8;
    let error = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        _ => SessionRealizationControlFailure::Unavailable(String::new()),
    };
    // Keep the proof oracle independent from both the contract disposition and
    // this consumer. Terminal Control truth plus explicit StaleOwnership prove
    // loss; NotReady, Conflict, and Unavailable retain the retry path.
    let expected_retirement = matches!(selector, 0 | 2 | 3 | 4 | 6);
    let revokes = error.proves_current_realization_cannot_continue();

    assert_eq!(revokes, expected_retirement);
}

#[cfg(test)]
mod realization_renewal_tests {
    use awaken_session_realization_contract::SessionRealizationControlFailure;

    #[test]
    fn terminal_session_replies_retire_only_the_stale_local_projection() {
        // Renewal decision table: N1 NotFound, N2 Terminal, and N3 Invalid prove the local
        // projection no longer has a renewable frozen Control owner -> retire it
        // quietly; stale ownership is an equally conclusive ownership loss;
        // NotReady/conflict/unavailable retain the still-live lease for retry.
        for error in [
            SessionRealizationControlFailure::NotFound,
            SessionRealizationControlFailure::Retired,
            SessionRealizationControlFailure::Terminal,
            SessionRealizationControlFailure::Invalid("bad target".into()),
        ] {
            assert!(error.proves_current_realization_cannot_continue());
        }
        for error in [
            SessionRealizationControlFailure::NotReady,
            SessionRealizationControlFailure::Conflict,
            SessionRealizationControlFailure::Unavailable("network unavailable".into()),
        ] {
            assert!(!error.proves_current_realization_cannot_continue());
        }
        assert!(
            SessionRealizationControlFailure::StaleOwnership
                .proves_current_realization_cannot_continue()
        );
    }

    #[test]
    fn failed_realization_invalidates_only_its_local_generation() {
        // Failure-projection cause/effect graph: C1 owner, incarnation, and
        // epoch match; C2 the local expiry is equal/later than the admitted
        // effect; C3 a replacement generation is already installed. Effects:
        // E1 invalidate the failed/renewed local projection; E2 preserve a
        // regressed or replacement projection. The aggregate remains the lease
        // authority and the Runtime keeps no parallel failure state.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | F1 exact | yes | equal | no | E1 |
        // | F2 renewed | yes | later | no | E1 |
        // | F3 regressed | yes | earlier | no | E2 |
        // | F4 replacement | no | any | yes | E2 |
        let failed = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 7,
            expires_at_unix_ms: 10,
        };
        for (rule, current, expected) in [
            ("F1", failed.clone(), true),
            (
                "F2",
                awaken_session_configuration_contract::resource::SessionRealizationLease {
                    expires_at_unix_ms: 20,
                    ..failed.clone()
                },
                true,
            ),
            (
                "F3",
                awaken_session_configuration_contract::resource::SessionRealizationLease {
                    expires_at_unix_ms: 9,
                    ..failed.clone()
                },
                false,
            ),
            (
                "F4",
                awaken_session_configuration_contract::resource::SessionRealizationLease {
                    epoch: 8,
                    expires_at_unix_ms: 20,
                    ..failed.clone()
                },
                false,
            ),
        ] {
            assert_eq!(
                awaken_session_realization_contract::realization_lease_generation_authorizes(
                    &current, &failed
                ),
                expected,
                "{rule}"
            );
        }
    }
}

impl crate::RunAssembly {
    #[cfg(test)]
    pub(crate) fn install_session_realization_lease(
        &self,
        session_id: &str,
        lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<(), crate::HostError> {
        // The test fixture installs the Session whose lease it installs.
        self.installed_sessions
            .install(session_id)
            .expect("installed Sessions");
        let realizations = self.installed_sessions.realizations();
        let changed = self
            .realization_executor
            .authority()
            .authorize_worker_effect(|| {
                realizations.update_realization(session_id, |r| {
                    if !r.install_lease(
                        awaken_session_realization::test_support::test_observed_lease(lease),
                    ) {
                        return Err(crate::HostError::internal(
                            "test lease installation rejected",
                        ));
                    }
                    Ok(r.execution().changed.clone())
                })?
            })?;
        changed.notify_waiters();
        Ok(())
    }

    /// Close only the failed local projection of one exact realization
    /// generation. A renewed same-generation projection is also invalidated,
    /// because the aggregate failure command authorizes the shorter admitted
    /// effect and expires that whole owner/incarnation/epoch generation. A
    /// replacement generation is never touched.
    pub(crate) fn invalidate_session_realization_lease(
        &self,
        session_id: &str,
        failed: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), crate::realization_error::RealizationError> {
        let realizations = self.installed_sessions.realizations();
        realizations
            .modify_realization(session_id, |r| {
                if r.execution().authority.lease().is_some_and(|current| {
                    awaken_session_realization_contract::realization_lease_generation_authorizes(
                        current, failed,
                    )
                }) {
                    r.close_authority(None);
                }
            })
            .map(|_| ())
    }

    /// Authorize an MCP effect admitted before a same-epoch lease extension.
    /// The exact request remains immutable; only the Control-installed local
    /// lease may prove that its owner incarnation and epoch still have live,
    /// monotonically extended authority.
    pub(crate) fn mcp_generation_is_authorized_at(
        &self,
        generation: &crate::session_mcp::McpGenerationRef,
        now_unix_ms: u64,
    ) -> Result<bool, awaken_session_types::RunError> {
        Ok(self
            .mcp_generation_root_authority(generation)?
            // Direct realization executor callers can present a live Control assertion
            // before installing a complete local Session projection. A relay
            // capability, by contrast, always requires the current root.
            .unwrap_or_else(|| {
                awaken_session_realization_contract::realization_lease_is_live_at(
                    generation.lease_expires_at_unix_ms(),
                    now_unix_ms,
                )
            }))
    }

    /// One root-authority decision shared by effect admission and the relay.
    pub(crate) fn mcp_generation_root_authority(
        &self,
        generation: &crate::session_mcp::McpGenerationRef,
    ) -> Result<Option<bool>, awaken_session_types::RunError> {
        use awaken_session_mcp::McpGenerationAuthority as _;
        self.mcp_root_authority
            .mcp_generation_root_authority(generation)
    }

    /// Drive an initial Worker-assigned Session through the same phase driver
    /// used by claimed Runs, but without opening a Run or model operation.
    pub async fn prepare_session_without_run(
        &self,
        assignment: &awaken_session_realization_contract::SessionPreparationAssignment,
    ) -> Result<(), crate::HostError> {
        let control = self
            .capabilities
            .worker
            .session_control
            .as_ref()
            .ok_or_else(|| {
                crate::HostError::internal("Worker has no Session realization Control client")
            })?;
        crate::application::worker_projection::realize_session(
            self,
            control.as_ref(),
            &assignment.session_id,
            assignment.directive.clone(),
            None,
            None,
            false,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
        .map_err(|error| {
            crate::HostError::internal(format!("Session preparation remained pending: {error}"))
        })
    }

    /// Schedule one terminal cleanup recovery sweep for this Worker's
    /// `target`. The sweep (claims, bounds, absence handling, retirement and
    /// revocation) is the realization driver's one orchestration; this
    /// executor serves only the process-local projections it installs.
    pub async fn recover_terminal_cleanup_assignments(
        &self,
        target: awaken_session_realization_contract::SessionRealizationTarget,
    ) -> Result<usize, crate::HostError> {
        let control = self.authority.terminal_cleanup_control().ok_or_else(|| {
            crate::HostError::internal(
                "active Worker Session projection has no terminal cleanup Control client",
            )
        })?;
        awaken_session_realization_driver::recover_terminal_cleanup_assignments(
            target,
            self.terminal_recovery_participants(control.as_ref()),
        )
        .await
        .map_err(terminal_recovery_sweep_error)
    }

    /// Remove one process-local realization after its continuing authority is
    /// no longer provable. This is not a terminal Session edge: preserve the
    /// durable Sandbox so the next authorized Worker can adopt it, while local
    /// processes, routes, credentials, and runtime references are discarded.
    pub(crate) async fn revoke_session_realization(
        &self,
        session_id: &str,
        expected_projection: Option<(
            &crate::session_resource::SessionRealizationLease,
            &Arc<tokio::sync::Mutex<()>>,
        )>,
    ) -> Result<bool, crate::HostError> {
        // Close admission under the synchronous slot mutex before waiting for
        // either async owner lock. Retain the exact closed projection for the
        // post-lock CAS; neither a renewal reply nor cancellation reopens it.
        let Some(selected) = self
            .installed_sessions
            .close_realization(session_id, expected_projection)?
        else {
            return Ok(false);
        };
        self.retire_selected_session_realization(
            session_id,
            selected,
            expected_projection.is_some(),
        )
        .await
    }

    /// Retire one closed root through its original physical owner. With
    /// `interrupt`, the Run controls captured at the close are signalled after
    /// the root still matches and before retirement; a foreground record that
    /// was unreadable at the close refuses the retirement before any effect.
    async fn retire_selected_session_realization(
        &self,
        session_id: &str,
        selected: crate::installed_session::ClosedSessionRealization,
        interrupt: bool,
    ) -> Result<bool, crate::HostError> {
        let run = if interrupt { selected.run? } else { None };
        self.realization_executor
            .environments()
            .retire_selected_session_environment(
                session_id,
                (&self.mcp, self.installed_sessions.as_ref()),
                &selected.root,
                || {
                    #[cfg(test)]
                    if let Some(after_check) = selected.after_check {
                        after_check();
                    }
                    if let Some(run) = &run {
                        run.interrupt();
                    }
                },
            )
            .await
            .map_err(crate::HostError::from)
    }

    #[cfg(test)]
    pub(crate) async fn retire_selected_session_realization_with_post_check(
        &self,
        session_id: &str,
        mut selected: crate::installed_session::ClosedSessionRealization,
        interrupt: bool,
        after_check: impl FnOnce() + Send + 'static,
    ) -> Result<bool, crate::HostError> {
        selected.after_check = Some(Box::new(after_check));
        self.retire_selected_session_realization(session_id, selected, interrupt)
            .await
    }

    /// Revoke every process-local Session projection after Worker authority is
    /// no longer provable. Durable Session environments remain available for
    /// adoption; only terminal Session lifecycle commands may dispose them.
    pub async fn revoke_all_session_realizations(&self) -> Result<usize, crate::HostError> {
        self.finish_session_realization_revocation(self.begin_session_realization_revocation()?)
            .await
    }

    /// Close all current Session authority BEFORE a Worker starts asynchronous
    /// drain/interruption. This selection is not a new Worker registry lease.
    pub fn begin_session_realization_revocation(
        &self,
    ) -> Result<SessionRealizationRevocation, crate::HostError> {
        Ok(SessionRealizationRevocation {
            selected: self.installed_sessions.close_all_realizations()?,
        })
    }

    /// Join every selected cleanup without letting one error or held owner lock
    /// prevent an unrelated Session from reaching its own stop edge. No detached
    /// task, additional scheduler, or cancellation-as-success path is created.
    pub async fn finish_session_realization_revocation(
        &self,
        revocation: SessionRealizationRevocation,
    ) -> Result<usize, crate::HostError> {
        let results = futures_util::future::join_all(revocation.selected.into_iter().map(
            |(id, selected)| async move {
                self.retire_selected_session_realization(&id, selected, false)
                    .await
            },
        ))
        .await;
        let mut revoked = 0;
        let mut error = None;
        for result in results {
            match result {
                Ok(true) => revoked += 1,
                Ok(false) => {}
                Err(failure) => {
                    error.get_or_insert(failure);
                }
            }
        }
        error.map_or(Ok(revoked), Err)
    }

    /// Install the protocol-neutral Runtime coordinates derived from one frozen
    /// Session projection. Managed local preparation and claimed Worker replay
    /// share this single lowering path; neither may independently reconstruct a
    /// different workspace, Agent, model, backend, Environment, or tool surface.
    #[cfg(test)]
    pub(crate) fn project_session_init(
        &self,
        thread: &str,
        init: &awaken_session_configuration_contract::SessionInit,
    ) -> Result<(), crate::HostError> {
        let environment = environment_projection::project_environment(&init.environment);
        self.installed_sessions
            .install_with(thread, |slot| {
                slot.inputs.validate_environment(thread, &environment)?;
                slot.replace_frozen_with(|next| next.lower_session_init(init, environment));
                Ok(())
            })
            .expect("installed Sessions")
    }

    pub(crate) async fn install_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
        realization_lease: Option<
            awaken_session_realization_contract::ObservedSessionRealizationLease,
        >,
    ) -> Result<(), crate::HostError> {
        self.install_frozen_session_projection_with_resource_authority(
            thread,
            projection,
            claim,
            if synchronize_resources {
                FrozenResourceProjectionMode::Synchronize
            } else {
                FrozenResourceProjectionMode::ValidateInstalled
            },
            realization_lease,
        )
        .await
    }

    /// Install the Coordinator's complete frozen dispatch projection. This is
    /// the sole caller authorized to amend an unattempted same-revision Resource
    /// generation; Worker and local realization callers use the fenced method
    /// above.
    pub(crate) async fn install_dispatch_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
    ) -> Result<(), crate::HostError> {
        self.install_frozen_session_projection_with_resource_authority(
            thread,
            projection,
            None,
            FrozenResourceProjectionMode::Dispatch,
            None,
        )
        .await
    }

    async fn install_frozen_session_projection_with_resource_authority(
        &self,
        thread: &str,
        projection: FrozenSessionProjection,
        claim: Option<&RunClaim>,
        resource_mode: FrozenResourceProjectionMode,
        realization_lease: Option<ObservedSessionRealizationLease>,
    ) -> Result<(), crate::HostError> {
        let realization = &self.realization_executor;
        let exec = realization.environments();
        let sessions = &realization.sessions();
        if projection.baseline.fingerprint.0.trim().is_empty() {
            return Err(crate::HostError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        let has_mcp_projection = projection.mcp.iter().any(|attachment| {
            !matches!(
                attachment.state,
                crate::session_mcp::McpAttachmentState::Removed
                    | crate::session_mcp::McpAttachmentState::Failed
            )
        });
        let resource_transition = projection
            .resource_transition(resource_mode.transition_use())
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        let baseline = projection.baseline.clone();
        let init = projection.session_init();
        let environment_projection = environment_projection::project_environment(&init.environment);
        // Complete projection installation owns the Resource suffix lock from
        // the first comparison through publication of every correlated fact.
        // Desired-only staging and physical reconciliation therefore cannot
        // interleave a different resources/skills/manifest generation between
        // preflight and the baseline/transition/lease update below.
        self.installed_sessions.install(thread)?;
        let resource_projection = sessions.lock_resource_projection(thread).await?;
        let receipt =
            crate::realization_state::FrozenProjectionReceipt::from_projection(&projection)?;
        sessions.update_realization(thread, |r| r.validate_projection_receipt(&receipt))??;
        let (publication, model_candidate) =
            exec.publication().resolve_canonical_session_projection(
                &projection.workspace_id,
                CanonicalSessionProjection::Baseline(&projection.baseline),
                projection.agent_publication.clone(),
                Some(projection.resources.skills()),
            )?;
        let frozen_inputs = sessions.frozen(thread)?;
        frozen_inputs
            .validate_publication(publication.as_ref())
            .map_err(|_| {
                crate::HostError::internal(
                    "Session realization cannot replace its immutable Agent publication",
                )
            })?;
        frozen_inputs.validate_environment(thread, &environment_projection)?;
        // Validate the prospective complete layout before publishing any part of
        // a cold projection. The baseline is not resident yet on first install,
        // so a validator that reads only the current slot would miss a Repository
        // nested below one of its frozen mounts and could load Skills first. A
        // Worker-owned Dispatch is projection-only on the Coordinator: it uses
        // the same structural kernel without resolving a trusted-host provider
        // that only the claimed Worker may own. Every realization/cleanup path
        // and every Local dispatch retains the exact provider-effective check.
        if resource_mode == FrozenResourceProjectionMode::Dispatch
            && projection.baseline.runtime_placement
                == awaken_session_types::SessionRuntimePlacement::Worker
        {
            realization.staging().validate_structural_resource_layout(
                thread,
                &projection.resources,
                Some(&baseline.mounts),
                Some(&baseline.env),
                Some(&environment_projection),
            )?;
        } else {
            let provisioning = model_candidate.as_ref().map_or(
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                |candidate| candidate.provisioning(),
            );
            let provider = exec.session_environment_provider_for_thread(thread, provisioning)?;
            realization.staging().validate_resource_layout(
                thread,
                &projection.resources,
                provider,
                Some(&baseline.mounts),
                Some(&baseline.env),
                Some(&environment_projection),
            )?;
        }

        let baseline_to_install = if let Some(existing) = frozen_inputs.baseline.as_ref() {
            if existing.fingerprint != baseline.fingerprint {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Session baseline"
                )));
            }
            None
        } else {
            let is_realized = self
                .installed_sessions
                .read(thread, |slot| {
                    slot.run
                        .context
                        .as_ref()
                        .is_some_and(|context| !context.is_coordination_envelope())
                        || slot.realization.environment().owner.has_local_environment()
                })?
                .unwrap_or(false);
            if is_realized {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} was realized before its frozen Session baseline"
                )));
            }
            Some(baseline)
        };

        if baseline_to_install.is_some()
            && matches!(
                resource_mode,
                FrozenResourceProjectionMode::ValidateInstalled
            )
            && projection.resource_revision > 0
        {
            return Err(crate::HostError::internal(format!(
                "thread {thread} cannot cold-materialize frozen Session Resources during lease-only renewal"
            )));
        }
        let transition_install = sessions
            .read_realization(thread, |r| {
                frozen_resource_transition_install_decision(
                    r.staging().transition.as_ref(),
                    &resource_transition,
                    resource_mode.allows_unattempted_amendment(),
                )
            })?
            .unwrap_or(FrozenResourceTransitionInstallDecision::Stage);
        if transition_install == FrozenResourceTransitionInstallDecision::Reject {
            let existing = sessions
                .read_realization(thread, |r| r.staging().transition.clone())?
                .flatten();
            let existing_revision = existing
                .as_ref()
                .map(|transition| transition.desired().revision);
            let incoming_previous_matches = existing
                .as_ref()
                .is_some_and(|existing| existing.desired() == resource_transition.previous());
            return Err(crate::HostError::internal(format!(
                "thread {thread} cannot replace its current Session Resource projection \
                 (mode {resource_mode:?}, existing desired revision {existing_revision:?}, \
                 incoming {}->{}, previous matches existing desired: {incoming_previous_matches})",
                resource_transition.previous().revision,
                resource_transition.desired().revision,
            )));
        }
        // A child may commit Resource revision R before the parent resumes;
        // the frozen transition can then be R->R even though this Worker's
        // revoked physical wrapper still holds R-1. The root receipt and the
        // owner kernel, rather than the transition delta alone, govern whether
        // that exact monotonic reservation may be re-adopted.
        let pending_resource_reservation =
            resource_mode.allows_revoked_owner_reservation(projection.resource_revision);
        // Resource installation owns generation equality and fencing. Claimed
        // replay revalidates the exact aggregate transition. Requirements are
        // staged here, but only the physical transition body may publish the
        // active manifest.
        #[cfg(target_os = "linux")]
        if let Some(proof) = realization_lease.as_ref() {
            // Reject a foreign durable owner BEFORE retiring a valid execution
            // object. Publication still revalidates this same canonical kernel.
            self.installed_sessions.install_with(thread, |slot| {
                let newer = slot.realization.validate_projection_receipt(&receipt)?;
                slot.realization.environment().owner.after_projection(
                    thread,
                    &projection.workspace_id,
                    &projection.environment,
                    newer,
                    pending_resource_reservation,
                )?;
                Ok::<_, crate::HostError>(())
            })??;
            let mcp = (&self.mcp, self.installed_sessions.as_ref());
            exec.retire_replaced_scoped_execution(realization.peers(), thread, proof, mcp)
                .await?;
        }
        let preparation = if resource_mode.synchronizes() && projection.resource_revision > 0 {
            realization_lease
                .as_ref()
                .map(|lease| {
                    ResourcePreparation::begin(realization.authority(), thread, lease.clone())
                })
                .transpose()?
        } else {
            None
        };
        if resource_mode.synchronizes() && projection.resource_revision > 0 {
            let stage = realization
                .staging()
                .stage_prevalidated_resource_transition_under_resource_projection(
                    resource_projection.held(),
                    realization.peers(),
                    thread,
                    &resource_transition,
                    claim,
                    realization_lease.as_ref().map(|proof| &proof.lease),
                    projection.environment.binding().is_some(),
                );
            tokio::select! {
                biased;
                error = async {
                    match &preparation {
                        Some(preparation) => preparation.authority_ended().await,
                        None => std::future::pending().await,
                    }
                } => return Err(error.into()),
                result = stage => result.map_err(|error| crate::HostError::internal(error.to_string()))?,
            }
        }
        if let Some(preparation) = &preparation {
            preparation.validate()?;
        }
        realization.authority().authorize_worker_effect(|| {
            self.installed_sessions.install_with(thread, |slot| {
                // Revalidate fallible coordinates and stage the existing owner
                // transition under the final slot lock. No correlated field is
                // published before the time proof/preparation is admitted.
                slot.inputs.validate_publication(publication.as_ref())?;
                slot.inputs
                    .validate_environment(thread, &environment_projection)?;
                let newer = slot.realization.validate_projection_receipt(&receipt)?;
                let owner = slot.realization.environment().owner.after_projection(
                    thread,
                    &projection.workspace_id,
                    &projection.environment,
                    newer,
                    pending_resource_reservation,
                )?;
                if let Some(preparation) = &preparation {
                    preparation.finish(&mut slot.realization)?;
                } else if let Some(lease) = realization_lease {
                    if !slot.realization.install_lease(lease) {
                        return Err(crate::HostError::internal(
                            "Session realization time proof is not current",
                        ));
                    }
                    slot.realization.execution().changed.notify_waiters();
                }
                // This suffix is infallible and holds both Worker authority and
                // the slot lock: revoke/replacement cannot interleave a partial
                // publication between the check above and these writes.
                if let Some(claim) = claim {
                    slot.run.dispatch_claim = Some(claim.clone());
                }
                slot.realization.publish_frozen_projection(
                    owner,
                    &projection.environment,
                    slot.run.context.is_some(),
                    receipt,
                    has_mcp_projection,
                    resource_transition,
                );
                // A cold Coordinator may have opened a metadata-only
                // reservation envelope before this complete projection.
                // Rebuild it against the newly frozen baseline on its next
                // read; it has no executable Environment to preserve.
                if baseline_to_install.is_some() {
                    slot.run.drop_coordination_envelope();
                }
                // One complete next value replaces the frozen inputs. The
                // validated publication and an installed baseline are kept.
                slot.replace_frozen_with(|next| {
                    next.lower_session_init(&init, environment_projection);
                    if let Some(publication) = publication {
                        next.published_snapshot = Some(publication);
                    }
                    if let Some(baseline) = baseline_to_install {
                        next.baseline = Some(baseline);
                    }
                    next.request_context = projection.request_context.clone();
                });
                // The complete frozen projection is the cold Worker's first
                // observation of Session admission. Publish the existing execution
                // marker in the same atomic slot update as its baseline and
                // transition; local admission may set it earlier, while revocation
                // remains authorized to clear it with the rest of live execution
                // state.
                slot.run.session_dispatch = true;
                Ok::<_, crate::HostError>(())
            })??;
            Ok(())
        })
    }

    /// Terminal effect fixtures supply an already-received assignment. Actual
    /// transport-latency tests call the production entrance with their explicit
    /// request-start observation instead of this convenience adapter.
    #[cfg(test)]
    pub(crate) async fn install_test_terminal_cleanup_projection(
        &self,
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
    ) -> Result<(), crate::HostError> {
        let started = awaken_authority_clock::authority_proof_started().unwrap();
        self.install_terminal_cleanup_projection(
            assignment,
            assignment.authority.clone().observed(started),
        )
        .await
    }

    /// Install one aggregate-frozen terminal assignment without entering the
    /// active realization driver. Cleanup I/O remains owned by the terminal
    /// effect path.
    pub(crate) async fn install_terminal_cleanup_projection(
        &self,
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), crate::HostError> {
        let realizations = self.installed_sessions.realizations();
        if proof.lease != assignment.authority.lease {
            return Err(crate::HostError::internal(
                "terminal assignment time proof names another lease",
            ));
        }
        let _realization = self
            .installed_sessions
            .acquire_realization_with(assignment.session_id.as_str(), |_| ())
            .await?
            .0;
        self.install_frozen_session_projection_with_resource_authority(
            assignment.session_id.as_str(),
            assignment.projection.clone(),
            None,
            FrozenResourceProjectionMode::Terminal,
            Some(proof),
        )
        .await?;
        realizations.update_realization(assignment.session_id.as_str(), |r| {
            r.assign_terminal_cleanup(assignment.clone());
        })?;
        Ok(())
    }

    pub(crate) fn install_environment_projection(
        &self,
        thread: &str,
        environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
    ) -> Result<(), crate::HostError> {
        let projection = environment_projection::project_environment(environment);
        self.installed_sessions.install_with(thread, |slot| {
            slot.inputs.validate_environment(thread, &projection)?;
            slot.replace_frozen_with(|next| next.lower_environment(environment, projection));
            Ok(())
        })?
    }

    /// Return the exact credential-custody profile frozen in this Session's
    /// Environment. Inference admission uses this projection only when the
    /// immutable Agent publication constrains a plaintext boundary without
    /// naming one exact holder.
    pub(crate) fn thread_credential_realization(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_credential_contract::CredentialRealizationProfile>,
        crate::realization_error::RealizationError,
    > {
        self.installed_sessions
            .realizations()
            .credential_realization(thread)
    }

    #[cfg(test)]
    pub(crate) fn thread_session_prompts(&self, thread: &str) -> Vec<String> {
        let realizations = self.installed_sessions.realizations();
        realizations
            .prompts(thread, None, false, true)
            .expect("test Native prompt projection")
    }
}

#[cfg(test)]
mod terminal_cleanup_drive_error_tests {
    use super::terminal_recovery_sweep_error as sweep_error;
    use crate::HostErrorKind;
    use awaken_session_realization_driver::{TerminalRecoveryError, TerminalRecoverySweepError};

    #[test]
    fn terminal_driver_error_mapping_preserves_effect_faults_and_control_boundary() {
        // Cause/effect graph: C1 the canonical driver returns a classified
        // Runtime effect failure; C2 it returns a Control/protocol failure.
        // Effects: E1 preserve the Runtime fault kind and stable code through
        // the existing inverse adapter; E2 keep Control/protocol failure at the
        // Host's terminal reconciliation boundary with the Session identity.
        // Decision rules: M1=C1=>E1; M2=C2=>E2.
        let effect = sweep_error(TerminalRecoverySweepError {
            session_id: Some("session-a".into()),
            error: TerminalRecoveryError::Drive(
                awaken_session_realization_contract::SessionRealizationDriveError::Effect(
                    awaken_session_types::RunError::unavailable_classified(
                        "terminal_provider_retry",
                        "provider is temporarily unavailable",
                    ),
                ),
            ),
        });
        assert_eq!(effect.kind, HostErrorKind::Unavailable, "M1/E1");
        assert_eq!(effect.code, "terminal_provider_retry", "M1/E1");

        let control = sweep_error(TerminalRecoverySweepError {
            session_id: Some("session-a".into()),
            error: TerminalRecoveryError::Drive(
                awaken_session_realization_contract::SessionRealizationDriveError::Control(
                    awaken_session_realization_contract::SessionRealizationControlFailure::Conflict,
                ),
            ),
        });
        assert_eq!(control.kind, HostErrorKind::Internal, "M2/E2");
        assert!(control.message.contains("session-a"), "M2/E2");
        assert!(control.message.contains("changed concurrently"), "M2/E2");
    }
}

#[cfg(test)]
mod network_policy_tests {
    /// FMECA: F1 a prepared image and packages are both realized (S8/O3/D4,
    /// RPN96) causes nondeterministic/double installation; mitigation makes the
    /// digest the sole rootfs and clears package requirements. F2 no prepared
    /// image drops authored packages (S7/O3/D3, RPN63); mitigation losslessly
    /// projects non-empty managers with an exact revision resolution id.
    /// Cause/effect graph: C1=prepared digest present; C2=authored packages;
    /// E1=image rootfs; E2=runtime package install; constraint C1 XOR E2.
    /// | Rule | C1 | C2 | E1 | E2 |
    /// | R1   | 0  | 1  | 0  | 1  |
    /// | R2   | 1  | 1  | 1  | 0  |
    /// Empty managers disappear and no protocol DTO reaches provisioning.
    #[test]
    fn environment_packages_project_losslessly_to_the_provisioning_contract() {
        let environment = awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "env_packages".into(),
            revision: awaken_environment_contract::EnvironmentRevision(3),
            self_hosted: false,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "fp".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: awaken_environment_contract::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                pip: vec!["httpx==0.28".into()],
                ..Default::default()
            },
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let projected =
            awaken_session_configuration_contract::environment_projection::project_environment(
                &environment,
            );
        assert_eq!(
            projected.packages.managers,
            [
                ("npm".into(), vec!["tsx@4".into()]),
                ("pip".into(), vec!["httpx==0.28".into()]),
            ]
            .into_iter()
            .collect()
        );
        assert!(!projected.packages.managers.contains_key("apt"));
        assert_eq!(
            projected.packages.resolution_id.as_deref(),
            Some("env_packages:3")
        );

        let mut prepared = environment;
        prepared.prepared_image = Some("registry/awaken@sha256:prepared".into());
        let projected =
            awaken_session_configuration_contract::environment_projection::project_environment(
                &prepared,
            );
        assert!(projected.packages.is_empty(), "R2");
        assert!(matches!(
            projected.sandbox.and_then(|value| value.environment),
            Some(awaken_provisioning_contract::EnvironmentKind::Image { reference })
                if reference == "registry/awaken@sha256:prepared"
        ));
    }
}

pub(crate) mod worker_projection;

/// The installed projection a terminal lookup belongs to: its realization
/// phase and the local Run controls selected in the same slot read.
pub struct TerminalRecoveryOrigin {
    phase: Arc<tokio::sync::Mutex<()>>,
    local: Option<crate::host::SelectedAttemptInterruption>,
}

fn terminal_recovery_run_error(
    error: impl Into<crate::HostError>,
) -> awaken_session_types::RunError {
    crate::host_error::to_run_error(error.into())
}

impl crate::RunAssembly {
    /// The participants a terminal recovery runs over: the given Control, this
    /// executor's realization, terminal cleanup and local projections, and the
    /// realization executor's effects.
    pub(crate) fn terminal_recovery_participants<'a>(
        &'a self,
        control: &'a dyn awaken_session_realization_contract::SessionTerminalCleanupControl,
    ) -> awaken_session_realization_driver::TerminalRecoveryParticipants<'a, Self> {
        awaken_session_realization_driver::TerminalRecoveryParticipants {
            control,
            realization: self,
            effects: self.realization_executor.as_ref(),
            cleanup: self,
            projections: self,
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_driver::TerminalRecoveryProjections for crate::RunAssembly {
    type Origin = TerminalRecoveryOrigin;

    fn capture_terminal_origin(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Option<TerminalRecoveryOrigin>, awaken_session_types::RunError> {
        // One read, never creating a slot; an unreadable foreground record
        // refuses the whole recovery.
        self.installed_sessions
            .read(session_id.as_str(), |slot| {
                slot.run
                    .select_interruption()
                    .map(|selected| TerminalRecoveryOrigin {
                        phase: slot.realization.execution().phase.clone(),
                        local: selected.local,
                    })
            })
            .map_err(terminal_recovery_run_error)?
            .transpose()
            .map_err(terminal_recovery_run_error)
    }

    async fn retire_completed_terminal_projection(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<bool, awaken_session_types::RunError> {
        self.realization_executor
            .effects()
            .retire_completed_terminal_cleanup_projection(&self.mcp, session_id.as_str(), lease)
            .await
            .map_err(awaken_session_types::RunError::from)
    }

    fn interrupt_origin_runs(&self, origin: &TerminalRecoveryOrigin) {
        if let Some(local) = &origin.local {
            local.interrupt();
        }
    }

    async fn retire_origin_generation(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &crate::session_resource::SessionRealizationLease,
        origin: &TerminalRecoveryOrigin,
    ) -> Result<(), awaken_session_types::RunError> {
        let Some(selected) = self
            .installed_sessions
            .close_realization_generation(session_id.as_str(), lease, &origin.phase)
            .map_err(terminal_recovery_run_error)?
        else {
            return Ok(());
        };
        self.retire_selected_session_realization(session_id.as_str(), selected, true)
            .await
            .map(drop)
            .map_err(crate::host_error::to_run_error)
    }

    async fn revoke_settled_projection(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(), awaken_session_types::RunError> {
        self.revoke_session_realization(session_id.as_str(), None)
            .await
            .map(drop)
            .map_err(crate::host_error::to_run_error)
    }
}

/// The executor boundary's projection of one recovery failure: an effect or
/// local projection failure keeps its own class; a Control or protocol
/// failure stays pending (internal).
fn terminal_recovery_error(
    error: awaken_session_realization_driver::TerminalRecoveryError,
) -> crate::HostError {
    match error {
        awaken_session_realization_driver::TerminalRecoveryError::Drive(
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(error),
        )
        | awaken_session_realization_driver::TerminalRecoveryError::Projection(error) => {
            crate::managed_adapter_error::from_run_error(error)
        }
        error => crate::HostError::internal(error.to_string()),
    }
}

/// A sweep failure keeps its first cause's class and names its Session.
fn terminal_recovery_sweep_error(
    error: awaken_session_realization_driver::TerminalRecoverySweepError,
) -> crate::HostError {
    let awaken_session_realization_driver::TerminalRecoverySweepError { session_id, error } = error;
    let projected = terminal_recovery_error(error);
    match session_id {
        Some(session_id) => crate::HostError {
            message: format!(
                "Session `{session_id}` terminal cleanup remained pending: {}",
                projected.message
            ),
            ..projected
        },
        None => projected,
    }
}

/// Reconcile one claimed terminal assignment through the driver, projected
/// onto this executor's error; composition tests drive it with the real
/// installed Sessions.
#[cfg(test)]
pub(crate) async fn reconcile_terminal_cleanup_for_test(
    assembly: &crate::RunAssembly,
    control: &dyn awaken_session_realization_contract::SessionTerminalCleanupControl,
    session_id: &awaken_session_types::SessionId,
    lease: &crate::session_resource::SessionRealizationLease,
) -> Result<awaken_session_realization_driver::TerminalReconciliation, crate::HostError> {
    awaken_session_realization_driver::reconcile_terminal_cleanup(
        session_id,
        lease,
        assembly.terminal_recovery_participants(control),
    )
    .await
    .map_err(terminal_recovery_error)
}
