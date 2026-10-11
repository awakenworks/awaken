// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
//! Claimed Worker installation of one already-frozen Session projection.

use super::realization_failure::{
    SessionRealizationWorkerEffect, session_realization_worker_effect,
};
use super::*;

/// Session projection authority is independent from the Run's admission shape.
/// Outcome Runs retain OrdinaryRoot identity, but the Coordinator's subordinate
/// Work receipt still requires the same frozen Session realization before effects.
pub(super) fn has_session_projection_authority(
    claimed: &awaken_dispatch_contract::Claimed,
) -> bool {
    claimed.request.session_thread_id.is_some() || claimed.session_work_authority.is_some()
}

/// A co-located dispatch pool has no outbound Control client because its
/// Session application already drives the same Runtime Host. It may consume a
/// claimed Managed Run only after that application has installed the complete
/// frozen projection. Treating the Run's derived execution snapshot as a
/// substitute would create a second Agent-publication authority during restart.
const fn local_claimed_projection_is_ready(
    has_frozen_baseline: bool,
    session_dispatch: bool,
) -> bool {
    has_frozen_baseline && session_dispatch
}

fn claimed_realization_continuation(
    slot: &crate::installed_session::InstalledSession,
    claim: &awaken_dispatch_contract::RunClaim,
) -> Option<awaken_session_resource_contract::SessionRealizationLease> {
    slot.run
        .dispatch_claim
        .as_ref()
        .filter(|previous| previous.run_id == claim.run_id && previous.owner == claim.owner)
        .and_then(|_| slot.realization.authority.remaining())
        .and_then(|_| slot.realization.authority.lease())
        .filter(|lease| lease.runtime_incarnation == claim.owner)
        .cloned()
}

fn map_claimed_session_control_error(
    run_id: &str,
    error: awaken_session_realization_contract::ClaimedSessionControlError,
) -> awaken_dispatch_execution::Error {
    match session_realization_worker_effect(error.disposition()) {
        SessionRealizationWorkerEffect::Defer => {
            awaken_dispatch_execution::Error::ResolutionNotReady(format!(
                "run {run_id} is waiting for its Session Environment Work slot"
            ))
        }
        SessionRealizationWorkerEffect::Relinquish => HostWorkerResolver::execution_error(format!(
            "run {run_id} frozen Session resume failed: {error}"
        )),
        SessionRealizationWorkerEffect::Absorb => HostWorkerResolver::terminal_resolution_error(
            format!("run {run_id} frozen Session cannot resume: {error}"),
        ),
    }
}

/// Compare the carried Run manifest with the one effect-capable Resource
/// transition authored by Control. Decoding through the projection's canonical
/// transition boundary keeps legacy validation and physical replay rules out of
/// the Worker transport adapter.
fn control_projection_matches_dispatched_manifest(
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
    dispatched: &awaken_session_resource_contract::SessionResourceManifest,
) -> Result<bool, awaken_session_types::RunError> {
    Ok(projection
        .resource_transition(
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
        )?
        .desired()
        == dispatched)
}

/// Install the frozen Session projection under the authenticated Run claim.
/// Session authoring is complete before WorkQueue dispatch; the Worker can
/// realize committed truth but cannot contribute another desired-state input.
pub(super) async fn install_claimed_session_projection(
    host: &SharedHost,
    claimed: &awaken_dispatch_contract::Claimed,
    thread_id: &awaken_agent_contract::agent::thread::Id,
    dispatched_resources: Option<&awaken_session_resource_contract::SessionResourceManifest>,
) -> Result<(), awaken_dispatch_execution::Error> {
    let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
    let (_realization, continuation) = host
        .installed_sessions
        .acquire_realization_with(&thread_id.0, |slot| {
            // Only an unfinished retry of this same Run in the same process
            // can retain its still-live creation authority. A new Run, closed
            // Work, expired observation or cold Worker must request assignment.
            // Do not publish the incoming claim before Control succeeds: a
            // failed new Run must not relabel its predecessor's live lease as
            // its own on the next retry. Frozen projection publication installs
            // the claim with the admitted lease under the same slot lock.
            claimed_realization_continuation(slot, &claim)
        })
        .await;
    if !has_session_projection_authority(claimed) {
        if let Some(manifest) = dispatched_resources {
            host.install_dispatched_resources(&thread_id.0, manifest, Some(&claim))
                .await
                .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
        } else if let Some(scope) = claimed.request.execution_scope.as_ref() {
            // Model-only ordinary Runs still belong to the frozen execution
            // Workspace; they need no fabricated Resource manifest to carry it.
            host.register_thread_workspace(&thread_id.0, &scope.0.0);
        }
        return Ok(());
    }
    let Some(control) = host.worker.session_control.as_ref() else {
        let projection_ready = host
            .installed_sessions
            .read(&thread_id.0, |slot| {
                local_claimed_projection_is_ready(
                    slot.inputs.baseline.is_some(),
                    slot.run.session_dispatch,
                )
            })
            .unwrap_or(false);
        if !projection_ready {
            return Err(awaken_dispatch_execution::Error::ResolutionNotReady(
                format!(
                    "run {} is waiting for its co-located frozen Session projection",
                    claimed.lease.run_id().0
                ),
            ));
        }
        if let Some(manifest) = dispatched_resources {
            host.install_dispatched_resources(&thread_id.0, manifest, Some(&claim))
                .await
                .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
        }
        return Ok(());
    };

    // Cause graph: concurrent attempts may address one Session, but only the
    // admitted Work/Run owner may advance its realization lease. Serializing
    // before the first Control mutation prevents a later entrant from fencing a
    // slow earlier effect between directive creation and installation.
    let dispatch: Arc<dyn awaken_dispatch_contract::DispatchClaimOwnership> = host
        .dispatch_store()
        .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
    let ownership = awaken_dispatch_execution::claim_bound_ownership_verifier(
        dispatch,
        awaken_dispatch_contract::RunClaim::from(&claimed.lease),
    );
    ownership.verify_current().await.map_err(|error| {
        HostWorkerResolver::execution_error(format!(
            "run {} lost ownership before Session realization: {error}",
            claimed.lease.run_id().0
        ))
    })?;
    let request_started = awaken_authority_clock::authority_proof_started()
        .map_err(|error| HostWorkerResolver::execution_error(error.to_string()))?;
    let directive = control
        .resume_frozen(
            &claim,
            &thread_id.0,
            claimed.session_work_authority.as_ref(),
            continuation.as_ref(),
        )
        .await
        .map_err(|error| map_claimed_session_control_error(&claimed.lease.run_id().0, error))?
        .ok_or_else(|| {
            HostWorkerResolver::execution_error(
                "WorkQueue dispatched a Session whose creation was not finalized",
            )
        })?;
    if let Some(receipt) = claimed.session_work_authority.as_ref()
        && !receipt.binds_realization(
            &thread_id.0,
            &directive.authority.lease.runtime_incarnation,
            directive.projection.baseline.environment.self_hosted,
            &directive.projection.baseline.environment.environment_id,
        )
    {
        return Err(HostWorkerResolver::execution_error(
            "Session realization directive identity does not match claimed Work authority",
        ));
    }
    ownership.verify_current().await.map_err(|error| {
        HostWorkerResolver::execution_error(format!(
            "run {} lost ownership during frozen Session resume: {error}",
            claimed.lease.run_id().0
        ))
    })?;
    if let Some(dispatched) = dispatched_resources {
        let exact = control_projection_matches_dispatched_manifest(
            &directive.projection,
            dispatched,
        )
        .map_err(|error| {
            HostWorkerResolver::execution_error(format!(
                "Control resume projection has no effect-capable Resource transition: {error}"
            ))
        })?;
        if !exact {
            // The claimed envelope is immutable. Relinquishing would claim
            // the same incompatible Run forever; use the existing fenced
            // terminal commit instead of replacing its resources on retry.
            return Err(HostWorkerResolver::terminal_resolution_error(
                "Control resume projection conflicts with the claimed resource snapshot",
            ));
        }
    }
    HostWorkerResolver::drive_session_realization(
        host,
        control.as_ref(),
        &awaken_session_types::SessionId::from(&thread_id.0),
        directive,
        Some(claimed),
        request_started,
        &_realization,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_claim_retry_retains_only_its_live_realization() {
        // C1 previous claim present/same Run/same owner; C2 local proof
        // uninstalled/live/closed/expired; C3 proof belongs to this incarnation.
        // R1 exact C1 + live C2 + exact C3 => retain creation identity across a
        // dispatch retry epoch. R2 every missing/changed/closed/expired input =>
        // no continuation, so ordinary fenced assignment remains authoritative.
        // In particular Work retirement must never reopen its Closed lease.
        use crate::installed_session::{InstalledSession, SessionRealizationAuthority as A};
        let claim = awaken_dispatch_contract::RunClaim {
            run_id: awaken_agent_contract::agent::run::Id("run".into()),
            owner: "worker:incarnation".into(),
            epoch: 2,
        };
        let lease = awaken_session_resource_contract::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: claim.owner.clone(),
            epoch: 7,
            expires_at_unix_ms: u64::MAX,
        };
        let previous = awaken_dispatch_contract::RunClaim {
            epoch: 1,
            ..claim.clone()
        };
        let mut foreign_run = previous.clone();
        foreign_run.run_id.0 = "other-run".into();
        let mut foreign_owner = previous.clone();
        foreign_owner.owner = "other-worker".into();
        let mut foreign_lease = lease.clone();
        foreign_lease.runtime_incarnation = "other-worker".into();
        for (previous, exact_claim) in [
            (None, false),
            (Some(previous.clone()), true),
            (Some(foreign_run), false),
            (Some(foreign_owner), false),
        ] {
            for (authority, live_exact_proof) in [
                (A::Uninstalled, false),
                (
                    A::Live(crate::installed_session::test_observed_lease(lease.clone())),
                    true,
                ),
                (A::Closed(Some(lease.clone())), false),
                (
                    A::Live(crate::installed_session::test_expired_observed_lease(
                        lease.clone(),
                    )),
                    false,
                ),
                (
                    A::Live(crate::installed_session::test_observed_lease(
                        foreign_lease.clone(),
                    )),
                    false,
                ),
            ] {
                let expected = exact_claim && live_exact_proof;
                let slot = InstalledSession {
                    realization: crate::installed_session::SessionRealization {
                        authority,
                        ..Default::default()
                    },
                    run: crate::installed_session::SessionRunBinding {
                        dispatch_claim: previous.clone(),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                assert_eq!(
                    claimed_realization_continuation(&slot, &claim),
                    expected.then(|| lease.clone()),
                    "R1/R2"
                );
            }
        }
    }

    #[test]
    fn co_located_claim_requires_the_complete_frozen_projection() {
        // Cause/effect graph: C1 the frozen baseline is installed; C2 the
        // Session application has published its dispatch/preparation marker.
        // E1 only C1+C2 admits the local claimed path; every partial/cold row
        // waits for the canonical application projection and cannot reinterpret
        // the Run's attempt-local snapshot as immutable Session truth.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | L1 | no | no | wait |
        // | L2 | yes | no | wait |
        // | L3 | no | yes | wait |
        // | L4 | yes | yes | E1 admit |
        for (baseline, dispatch, expected) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            assert_eq!(
                local_claimed_projection_is_ready(baseline, dispatch),
                expected,
                "baseline={baseline}, dispatch={dispatch}"
            );
        }
    }

    #[test]
    fn control_projection_requires_the_exact_dispatched_resource_manifest() {
        use crate::host::worker_resolver::test_support::{
            deferred_environment, frozen_projection_for_manifest,
        };

        // Cause/effect graph: C1 Control supplies an effect-capable transition;
        // C2 the carried manifest matches/mismatches desired Workspace,
        // revision, or resources; C3 a resident legacy projection omits the
        // prior generation. Effects: E1 only exact desired truth is accepted;
        // E2 every individual coordinate mismatch rejects; E3 ApplyEffects
        // fails closed before the Worker treats legacy desired state as a
        // physical transition.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | R1 | yes | exact | no | E1 accept |
        // | R2 | yes | Workspace mismatch | no | E2 reject |
        // | R3 | yes | revision mismatch | no | E2 reject |
        // | R4 | yes | resources mismatch | no | E2 reject |
        // | R5 | no | exact desired | yes | E3 error |
        let desired = awaken_session_resource_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            7,
            awaken_session_resource_contract::ResolvedSessionResources::default(),
        );
        let projection = frozen_projection_for_manifest(deferred_environment(), &desired);
        assert!(
            control_projection_matches_dispatched_manifest(&projection, &desired)
                .expect("R1 effect-capable transition"),
            "R1/E1 exact desired manifest"
        );

        let different_resources =
            awaken_session_resource_contract::ResolvedSessionResources::try_new(
                Vec::new(),
                vec![awaken_session_resource_contract::ResolvedSkillBinding {
                    kind: awaken_agent_contract::AgentSkillKind::Custom,
                    skill_id: "different-skill".into(),
                    version: 1,
                    bundle_sha256: "sha256-different".into(),
                }],
            )
            .expect("R4 valid different resources");
        for (rule, carried) in [
            (
                "R2",
                awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    "workspace-b",
                    7,
                    Default::default(),
                ),
            ),
            (
                "R3",
                awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    "workspace-a",
                    8,
                    Default::default(),
                ),
            ),
            (
                "R4",
                awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    "workspace-a",
                    7,
                    different_resources,
                ),
            ),
        ] {
            assert!(
                !control_projection_matches_dispatched_manifest(&projection, &carried)
                    .unwrap_or_else(|error| panic!("{rule} transition decode: {error}")),
                "{rule}/E2"
            );
        }

        let mut legacy_bound = projection;
        legacy_bound.previous_resource_manifest = None;
        legacy_bound.environment =
            awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding: "durable-environment".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            };
        let error = control_projection_matches_dispatched_manifest(&legacy_bound, &desired)
            .expect_err("R5 legacy bound projection cannot authorize effects");
        assert_eq!(error.code, "session_resource_transition_missing", "R5/E3");
    }

    #[test]
    fn claimed_session_control_uses_the_contract_disposition() {
        // Cause/effect graph: C1 temporary Work-slot backpressure, C2 a
        // retryable dependency failure, and C3 durable terminal Session truth.
        // Effects: E1 defer, E2 relinquish through Execution, and E3 absorb the
        // Run. In particular C3 must never enter the claim/relinquish hot loop.
        //
        // | Rule | Control cause | durable Dispatch effect |
        // | R1 | NotReady | ResolutionNotReady |
        // | R2 | stale/conflict/unavailable | Execution |
        // | R3 | not-found/retired/terminal/invalid | TerminalResolution |
        let cases = [
            (
                "R1",
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
                "resolution_not_ready",
            ),
            (
                "R2 stale",
                awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership,
                "execution",
            ),
            (
                "R2 conflict",
                awaken_session_realization_contract::SessionRealizationControlFailure::Conflict,
                "execution",
            ),
            (
                "R2 unavailable",
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    "dependency".into(),
                ),
                "execution",
            ),
            (
                "R3 not found",
                awaken_session_realization_contract::SessionRealizationControlFailure::NotFound,
                "terminal_resolution",
            ),
            (
                "R3 terminal",
                awaken_session_realization_contract::SessionRealizationControlFailure::Terminal,
                "terminal_resolution",
            ),
            (
                "R3 retired",
                awaken_session_realization_contract::SessionRealizationControlFailure::Retired,
                "terminal_resolution",
            ),
            (
                "R3 invalid",
                awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                    "invalid phase".into(),
                ),
                "terminal_resolution",
            ),
        ];
        for (rule, failure, expected) in cases {
            let actual = map_claimed_session_control_error("run-a", failure.into());
            let actual = match actual {
                awaken_dispatch_execution::Error::ResolutionNotReady(_) => "resolution_not_ready",
                awaken_dispatch_execution::Error::Execution(_) => "execution",
                awaken_dispatch_execution::Error::TerminalResolution(_) => "terminal_resolution",
                error => panic!("{rule}: unexpected {error:?}"),
            };
            assert_eq!(actual, expected, "{rule}");
        }
    }

    #[tokio::test]
    async fn realization_admission_precedes_concurrent_control_mutation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Claim publication graph: R1 a current realization fence is held ->
        // next claimant publishes neither claim nor Control mutation. R2 an old
        // revocation clears the claim -> the waiter publishes its exact new
        // claim after release. R3 retirement removes the whole slot -> retry
        // acquisition on the replacement Arc before publishing. No stale Arc
        // can lend authority to a newly created Session slot.
        for remove_slot in [false, true] {
            let host = (crate::HostDependencies::ephemeral(
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                "stub",
            )
            .await)
                .build()
                .expect("validated Host dependencies");
            let old = awaken_dispatch_contract::RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run".into()),
                owner: "old".into(),
                epoch: 1,
            };
            let new = awaken_dispatch_contract::RunClaim {
                owner: "new".into(),
                epoch: 2,
                ..old.clone()
            };
            let (first, ()) = host
                .installed_sessions
                .acquire_realization_with("shared-session", |slot| {
                    slot.run.dispatch_claim = Some(old.clone());
                })
                .await;
            let second_entered = Arc::new(AtomicBool::new(false));
            let waiting = {
                let host = host.clone();
                let entered = second_entered.clone();
                let claim = new.clone();
                tokio::spawn(async move {
                    let (guard, ()) = host
                        .installed_sessions
                        .acquire_realization_with("shared-session", |slot| {
                            slot.run.dispatch_claim = Some(claim.clone());
                        })
                        .await;
                    entered.store(true, Ordering::SeqCst);
                    guard
                })
            };
            tokio::task::yield_now().await;
            assert!(!second_entered.load(Ordering::SeqCst), "R1");
            assert_eq!(
                host.installed_sessions
                    .read_run("shared-session", |run| run.dispatch_claim.clone())
                    .flatten(),
                Some(old),
                "R1"
            );
            if remove_slot {
                host.installed_sessions.remove("shared-session");
            } else {
                host.installed_sessions
                    .update_run("shared-session", |run| run.dispatch_claim = None);
            }
            drop(first);
            let second = waiting.await.expect("next admission joins");
            assert!(second_entered.load(Ordering::SeqCst), "R2/R3");
            assert_eq!(
                host.installed_sessions
                    .read_run("shared-session", |run| run.dispatch_claim.clone())
                    .flatten(),
                Some(new),
                "R2/R3"
            );
            assert!(
                host.installed_sessions
                    .realization_lock("shared-session")
                    .try_lock()
                    .is_err(),
                "R3 waiter owns the current Arc"
            );
            drop(second);
        }
    }
}
