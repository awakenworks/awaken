//! Integration tests for the dispatch-owned claim laws.
use super::*;
use awaken_dispatch_execution::session_worker_resolver::claimed_session::*;
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
        use crate::installed_session::InstalledSession;
        use crate::session_realizations::SessionRealizationAuthority as A;
        let claim = awaken_dispatch_contract::RunClaim {
            run_id: awaken_agent_contract::agent::run::Id("run".into()),
            owner: "worker:incarnation".into(),
            epoch: 2,
        };
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
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
                    A::Live(
                        awaken_session_realization::test_support::test_observed_lease(
                            lease.clone(),
                        ),
                    ),
                    true,
                ),
                (A::Closed(Some(lease.clone())), false),
                (
                    A::Live(
                        awaken_session_realization::test_support::test_expired_observed_lease(
                            lease.clone(),
                        ),
                    ),
                    false,
                ),
                (
                    A::Live(
                        awaken_session_realization::test_support::test_observed_lease(
                            foreign_lease.clone(),
                        ),
                    ),
                    false,
                ),
            ] {
                let expected = exact_claim && live_exact_proof;
                let mut slot = InstalledSession {
                    realization: crate::session_realizations::SessionRealization {
                        execution: crate::session_realizations::ExecutionAuthorityState {
                            authority,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    run: crate::installed_session::SessionRunBinding {
                        dispatch_claim: previous.clone(),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                assert_eq!(
                    claimed_realization_continuation(
                        &crate::session_realizations::SessionRealizationAccess {
                            inputs: &slot.inputs,
                            realization: &mut slot.realization,
                            run: &mut slot.run
                        },
                        &claim
                    ),
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
        let desired =
            awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                "workspace-a",
                7,
                awaken_session_configuration_contract::resource::ResolvedSessionResources::default(
                ),
            );
        let projection = frozen_projection_for_manifest(deferred_environment(), &desired);
        assert!(
            control_projection_matches_dispatched_manifest(&projection, &desired)
                .expect("R1 effect-capable transition"),
            "R1/E1 exact desired manifest"
        );

        let different_resources =
            awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
                Vec::new(),
                vec![
                    awaken_session_configuration_contract::resource::ResolvedSkillBinding {
                        kind: awaken_agent_contract::AgentSkillKind::Custom,
                        skill_id: "different-skill".into(),
                        version: 1,
                        bundle_sha256: "sha256-different".into(),
                    },
                ],
            )
            .expect("R4 valid different resources");
        for (rule, carried) in [
            (
                "R2",
                awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                    "workspace-b",
                    7,
                    Default::default(),
                ),
            ),
            (
                "R3",
                awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                    "workspace-a",
                    8,
                    Default::default(),
                ),
            ),
            (
                "R4",
                awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
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
        // can lend authority to a newly created Session slot. The admissions below
        // traverse SessionRealizations, the same view used by migrated dispatch.
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
                .realizations()
                .acquire_realization_with("shared-session", |slot| {
                    slot.run.install_dispatch_claim(old.clone());
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
                        .realizations()
                        .acquire_realization_with("shared-session", |slot| {
                            slot.run.install_dispatch_claim(claim.clone());
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
                    .realizations()
                    .realization_lock("shared-session")
                    .try_lock()
                    .is_err(),
                "R3 waiter owns the current Arc"
            );
            drop(second);
        }
    }
}
