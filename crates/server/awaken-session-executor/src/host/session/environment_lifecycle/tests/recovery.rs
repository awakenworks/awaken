// @formal evidence session_environment.closed_revoked_recovery_consumes_only_exact_unprotected_owner
// @formal evidence session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
//! Rebuild and recovery admission over closed, revoked, and retiring owners.

use super::*;

#[tokio::test]
async fn resume_rebuilds_instead_of_reusing_a_cached_quiesced_runtime() {
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, managed_test_host, test_activation,
    };

    // Resume recovery cause/effect table: C1 ordinary execution owns a cached
    // Runtime around one aggregate-Unowned LegacyDirect Environment; C2
    // realization revocation moves that owner to Retiring and clears the cache
    // through the one retirement transition; C3 the resume helper requests a
    // context. Effects: E1 C2 leaves no cached Runtime to reuse; E2 C3 performs
    // the same exact legacy/direct rebuild path and returns a distinct
    // Runtime/Environment/Hand tuple; E3 the retiring root is disposed before
    // same-id recreation. Rules RR1=C1+C2=>E1, RR2=C1+C2+C3=>E2+E3.
    let thread = "cached-resume-revocation";
    let root = tempfile::tempdir().unwrap();
    let host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(root.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    let _managed = managed_test_host(host.clone());
    let snapshot = test_activation(thread, "cached-resume-publication").snapshot;
    // C1 is ordinary local ownership: install its exact configuration and
    // publication without a Control-frozen baseline/Session execution marker.
    host.register_thread_workspace(thread, "workspace");
    host.register_thread_agent_projection(thread, snapshot.root_agent_id.0.as_str());
    host.realizer
        .publication
        .retain_session_publication(thread, Some(&snapshot))
        .unwrap();
    host.install_environment_projection(thread, &eager_environment())
        .unwrap();
    let provider = host
        .realizer
        .projected_session_environment_provider(thread, None)
        .expect("RR1 canonical projected provider");
    let spec = host
        .realizer
        .staging
        .sandbox_spec_for_provider(thread, provider);
    let environment = Arc::new(
        host.create_session_environment(provider, &spec)
            .await
            .expect("RR1 exact LegacyDirect Environment"),
    );
    let retired_sentinel = root
        .path()
        .join("sandboxes")
        .join(thread)
        .join("retired-owner-sentinel");
    std::fs::write(&retired_sentinel, b"owned by the retiring Environment")
        .expect("RR1 retiring root sentinel");
    host.install_test_resident_session_environment(thread, environment.clone());
    let original_hand = environment.agent_tools().tool_executor();
    let agent_id = snapshot.root_agent_id.0.clone();
    let cached = host
        .ctx_for_snapshot(thread, Some(agent_id.as_str()), Some(snapshot))
        .await
        .expect("RR1 cached Runtime");
    assert!(Arc::ptr_eq(cached.env.as_ref().unwrap(), &environment));
    host.installed_sessions
        .read(thread, |slot| {
            assert!(
                slot.inputs.baseline.is_none(),
                "RR1 ordinary, no Root projection"
            );
            assert!(!slot.run.session_dispatch, "RR1 no Managed grant");
            assert!(
                slot.realization.authority.lease().is_none(),
                "RR1 no fake lease"
            );
        })
        .unwrap();

    let lifecycle = host
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| r.lifecycle.clone())
        .unwrap();
    {
        let _lifecycle = lifecycle.lock().await;
        assert!(
            host.retire_session_environment_for_revocation(thread)
                .await
                .unwrap(),
            "RR1 exact revocation"
        );
    }
    assert!(
        host.installed_sessions
            .read_run(thread, |run| run.context.is_none())
            .unwrap_or(false),
        "RR1/E1 revocation clears the cached Runtime"
    );

    let resumed = host
        .ctx_for_resume(thread)
        .await
        .expect("RR2 resume recovery");
    let rebuilt = resumed.env.as_ref().expect("E1 rebuilt Environment");
    assert!(!Arc::ptr_eq(&cached, &resumed), "RR2/E2 fresh Runtime");
    assert!(
        !Arc::ptr_eq(rebuilt, &environment),
        "RR2/E2 closed owner replaced"
    );
    assert!(
        !Arc::ptr_eq(&original_hand, &rebuilt.agent_tools().tool_executor()),
        "RR2/E2 rebuilt Environment owns a fresh Hand"
    );
    assert!(
        !retired_sentinel.exists(),
        "RR2/E3 retiring root was removed before same-id recreation"
    );
}

#[tokio::test]
async fn recovery_status_returns_the_canonical_adoption_disposition() {
    use crate::host::worker_resolver::test_support::AdoptionModel;

    // Cause/effect decision table: C1 an exact ordinary Direct Resident is
    // recovered through its explicitly selected provider with rebuild enabled;
    // C2 provider observation is Ready; C3 it is Terminal. Effects: E1 C1+C2
    // returns Ready and reuses the same Environment owner and Runtime Arcs with
    // zero Candidate, binding persistence, or publication; E2 C1+C3 returns RebuildRequired,
    // clears the exact closed owner, records its binding as rebuild input, and
    // never restores the Runtime. Rules R21: Ready=>E1; Terminal=>E2. Provider
    // selection is an input to this canonical API, so the removed historical
    // frozen-provider present/absent rows are no longer separate decisions.
    // Fixture constraint: executable context construction uses the production
    // DispatchSessionRuntime composition; no test-only context path is restored.
    let ready_thread = "ordinary-ready-recovery";
    let ready_storage = tempfile::tempdir().unwrap();
    let ready_host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(ready_storage.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    let _ready_managed = crate::ManagedHost::new(ready_host.clone());
    let ready_runtime = ready_host
        .ctx_for(ready_thread, None)
        .await
        .expect("R21/C1 ordinary direct Runtime");
    let ready = ready_runtime.env.clone().expect("R21/C2 Ready Environment");
    let ready_binding = serde_json::to_string(&ready.lifecycle().handle()).unwrap();
    let ready_owner = ready_host
        .installed_sessions
        .realizations()
        .read_realization(ready_thread, |r| r.environment_owner.clone())
        .expect("R21/C1 exact Resident owner");
    assert!(
        ready_host
            .installed_sessions
            .read(ready_thread, |slot| slot
                .inputs
                .published_snapshot
                .is_none())
            .unwrap_or(false),
        "R21/C5 ordinary direct Runtime has no frozen provider projection"
    );
    let disposition = ready_host
        .adopt_bound_session_environment(
            ready_thread,
            Some(&ready_binding),
            &ready_host.realizer.env.session_provider,
            None,
            true,
        )
        .await
        .expect("R21/E1 Ready direct owner reactivates");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::Ready,
        "R21/E1"
    );
    let resident = ready_host
        .realizer
        .session_environment(ready_thread)
        .await
        .expect("R21/E1 Resident restored");
    assert!(Arc::ptr_eq(&resident, &ready), "R21/E1 exact Arc reused");
    assert!(
        matches!(
            (
                &ready_owner,
                ready_host
                    .installed_sessions.realizations()
                    .read_realization(ready_thread, |r| r.environment_owner.clone())
                    .as_ref(),
            ),
            (
                SessionEnvironmentOwner::Resident(expected),
                Some(SessionEnvironmentOwner::Resident(current)),
            ) if current.exact_matches(expected)
        ),
        "R21/E1 exact identity is not republished"
    );
    let restored_runtime = ready_host
        .installed_sessions
        .read_run(ready_thread, |run| run.context.clone())
        .flatten()
        .expect("R21/E1 Runtime restored");
    assert!(
        Arc::ptr_eq(&restored_runtime, &ready_runtime),
        "R21/E1 exact Runtime Arc reused"
    );

    let missing_thread = "terminated-recovery-missing-provider";
    let missing_storage = tempfile::tempdir().unwrap();
    let missing_host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(missing_storage.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    let _missing_managed = crate::ManagedHost::new(missing_host.clone());
    let missing_runtime = missing_host
        .ctx_for(missing_thread, None)
        .await
        .expect("R21/C1 missing-provider Runtime");
    let terminated = missing_runtime
        .env
        .clone()
        .expect("R21/C3 terminated Environment");
    let terminated_binding = serde_json::to_string(&terminated.lifecycle().handle()).unwrap();
    terminated
        .lifecycle()
        .dispose()
        .await
        .expect("C3 terminated provider");
    let disposition = missing_host
        .adopt_bound_session_environment(
            missing_thread,
            Some(&terminated_binding),
            &missing_host.realizer.env.session_provider,
            None,
            true,
        )
        .await
        .expect("R21/E2 Terminal owner requests rebuild");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::RebuildRequired,
        "R21/E2"
    );
    assert!(
        missing_host
            .realizer
            .session_environment_owner_is_vacant(missing_thread),
        "R21/E2 exact closed owner cleared"
    );
    assert!(
        missing_host
            .installed_sessions
            .read_run(missing_thread, |run| run.context.is_none())
            .unwrap_or(false),
        "R21/E2 Terminal owner never restores its Runtime"
    );
}

#[tokio::test]
async fn closed_revoked_durable_environment_can_enter_canonical_rebuild() {
    // Cause/effect table: C1 exact durable Resident was revoked; C2 provider
    // subsequently proves that physical incarnation closed; C3 frozen policy
    // permits rebuild. R1 C1+C2+C3 => consume the exact revoked owner, retain
    // source binding/generation for aggregate Rebuild, never reuse its Hand.
    // Revocation is not a competing RecoveryDiscard intent: changing its cause
    // would be rejected by the retirement priority fence before any rebuild.
    let thread = "closed-revoked-durable-rebuild";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
    let source_generation = generation(thread).id().to_owned();
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| {
            r.environment_owner = resident(thread, environment.clone());
        });
    host.realizer
        .retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::RealizationRevocation,
            RetirementSelection::Exact(&environment),
        )
        .expect("C1 exact revocation")
        .expect("C1 retained owner");
    dispose_current_environment(&environment, thread).await;
    // R0 C1+C2+!C3 => reject and retain the exact revoked owner; only the
    // frozen aggregate policy may authorize the subsequent R1 rebuild.
    assert!(
        host.adopt_bound_session_environment(
            thread,
            Some(&binding),
            &provider,
            Some(&source_generation),
            false
        )
        .await
        .is_err()
    );
    assert!(
        matches!(
            host.installed_sessions
                .realizations()
                .read_realization(thread, |r| r.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(_))
        ),
        "R0 retain revoked owner"
    );
    let disposition = host
        .adopt_bound_session_environment(
            thread,
            Some(&binding),
            &provider,
            Some(&source_generation),
            true,
        )
        .await
        .expect("R1 closed revoked owner must permit canonical rebuild");
    assert_eq!(
        disposition,
        SessionEnvironmentAdoptionDisposition::RebuildRequired
    );
    assert!(
        host.realizer.session_environment_owner_is_vacant(thread),
        "R1 closed owner consumed"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization(thread, |r| r.environment_rebuild_source.clone())
            .flatten(),
        Some((binding, Some(source_generation))),
        "R1 exact aggregate rebuild predecessor retained"
    );
}

#[test]
fn closed_environment_recovery_admission_covers_every_owner_class() {
    // Causes: C1 Reject/Rebuild policy; C2 exact Revocation,
    // RecoveryDiscard, Resident, pending, Vacant, or protected/stale owner.
    // Effects: E1 only Rebuild plus an exact unprotected class consumes; E2
    // every Reject or protected/stale row retains. This is the complete 2x6
    // decision table used before the production plan carries the exact fence.
    // Layout rule: large retirement/pending payloads remain exact owned values
    // behind indirection; small variants and the fail-closed plan must not reserve
    // hundreds of inline bytes across async recovery. A relative pointer-width
    // budget covers the representation independently of target architecture.
    assert!(
        std::mem::size_of::<ClosedEnvironmentRecoveryOwner>() <= 4 * std::mem::size_of::<usize>()
    );
    assert!(
        std::mem::size_of::<ClosedEnvironmentRecoveryPlan<'_>>()
            <= 6 * std::mem::size_of::<usize>()
    );
    let owners = [
        ClosedEnvironmentRecoveryOwnerClass::Revoked,
        ClosedEnvironmentRecoveryOwnerClass::RecoveryDiscard,
        ClosedEnvironmentRecoveryOwnerClass::Resident,
        ClosedEnvironmentRecoveryOwnerClass::Pending,
        ClosedEnvironmentRecoveryOwnerClass::Vacant,
        ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale,
    ];
    for policy in [
        SessionEnvironmentUnavailablePolicy::Reject,
        SessionEnvironmentUnavailablePolicy::Rebuild,
    ] {
        for owner in owners {
            assert_eq!(
                closed_environment_recovery_admission(policy, owner),
                policy == SessionEnvironmentUnavailablePolicy::Rebuild
                    && owner != ClosedEnvironmentRecoveryOwnerClass::ProtectedOrStale
            );
        }
    }
}

#[tokio::test]
async fn closed_recovery_cannot_override_protected_retirements() {
    // C1 exact closed physical owner; C2 Terminal or Checkpoint owns retirement;
    // C3 rebuild is enabled. R2 C1+C2+C3 => error, identical protected fence,
    // no rebuild predecessor. Physical absence is not lifecycle authority.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    for terminal in [false, true] {
        let thread = if terminal {
            "closed-terminal"
        } else {
            "closed-checkpoint"
        };
        let environment = environment(&provider, thread).await;
        let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
        let host = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await)
            .build()
            .expect("validated Host dependencies");
        host.installed_sessions
            .realizations()
            .update_realization(thread, |r| {
                r.environment_owner = resident(thread, environment.clone());
            });
        let cause = if terminal {
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal".into(),
            }
        } else {
            let generation = generation(thread);
            SessionEnvironmentRetirementCause::CheckpointSource {
                operation: awaken_session_state_contract::SessionEnvironmentOperation::new(
                    "workspace",
                    &awaken_session_types::SessionId::from(thread),
                    "suspend",
                    &generation,
                    1,
                    None,
                    None,
                ),
                generation,
            }
        };
        let protected = host
            .realizer
            .retire_current_environment(thread, cause, RetirementSelection::Exact(&environment))
            .unwrap()
            .unwrap();
        dispose_current_environment(&environment, thread).await;
        assert!(
            host.adopt_bound_session_environment(thread, Some(&binding), &provider, None, true)
                .await
                .is_err(),
            "R2 protected intent"
        );
        assert!(
            matches!(host.installed_sessions.realizations().read_realization(thread, |r| r.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(current)) if current.exact_matches(&protected.owner)),
            "R2 identical fence"
        );
        assert!(
            host.installed_sessions
                .realizations()
                .read_realization(thread, |r| r.environment_rebuild_source.is_none())
                .unwrap(),
            "R2 no rebuild"
        );
    }
}

#[tokio::test]
async fn stale_same_binding_retirement_cannot_clear_an_aba_replacement() {
    // Cause/effect graph: C1 two wrappers carry the same durable identity,
    // handle, and binding; C2 their Arc identities differ; C3 stale owner
    // reports Terminated after replacement entered Retiring. Effects: E1 C3
    // cannot clear replacement; E2 replacement's exact observation clears.
    // Decision rules R17/R19: C1+C2+C3=>E1; exact replacement=>E2.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let first = environment(&provider, "aba-retirement").await;
    let second = Arc::new(
        provider
            .adopt(
                &crate::provisioning::agent_run_sandbox_spec("aba-retirement"),
                &first.lifecycle().handle(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(
        first.lifecycle().handle(),
        second.lifecycle().handle(),
        "R17/C1"
    );
    assert!(!Arc::ptr_eq(&first, &second), "R17/C2");

    // R17 also covers revoked-owner completion: neither same binding nor same
    // generation can substitute for the exact captured wrapper identity.
    for cause in [
        SessionEnvironmentRetirementCause::RecoveryDiscard,
        SessionEnvironmentRetirementCause::RealizationRevocation,
    ] {
        let mut owner = resident("aba-retirement", first.clone());
        let stale = owner
            .begin_retirement(cause.clone(), RetirementSelection::Current)
            .unwrap()
            .unwrap();
        owner = resident("aba-retirement", second.clone());
        let replacement = owner
            .begin_retirement(cause, RetirementSelection::Current)
            .unwrap()
            .unwrap();
        assert!(
            !owner
                .observe_retirement_status::<()>(
                    &stale,
                    Ok(awaken_provisioning_contract::SandboxStatus::Terminated),
                )
                .unwrap(),
            "R17/E1"
        );
        assert!(
            matches!(owner, SessionEnvironmentOwner::Retiring(_)),
            "R17/E1"
        );
        assert!(
            owner
                .observe_retirement_status::<()>(
                    &replacement,
                    Ok(awaken_provisioning_contract::SandboxStatus::Terminated),
                )
                .unwrap(),
            "R17/E2"
        );
    }
}

#[tokio::test]
async fn checkpoint_disposal_cannot_consume_a_different_prepared_durable_owner() {
    // Cause/effect graph: C1 Preparation retains a durable G1 source; C2 the
    // later physical Disposal asserts G2 for the same binding; C3 its typed
    // provider authorization is otherwise closed. Effects: E1 Disposal fails
    // before provider I/O; E2 the exact G1 identity, binding, and Arc remain
    // Retiring under their original Preparation. Decision rule R19/R20:
    // C1+C2+C3 => E1+E2 zero physical effect. Cold source reconstruction is
    // separately covered by the canonical end-to-end source Preparation test
    // `source_release_preparation_is_withheld_for_an_absent_pod_with_live_claim`.
    let thread = "checkpoint-owner-fence";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    host.installed_sessions
        .realizations()
        .update_realization(thread, |r| {
            // Test fixture write remains inside the sole lifecycle-owner module.
            r.environment_owner = resident(thread, environment.clone());
        });
    let prepared_generation = generation(thread);
    let prepared_operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &prepared_generation,
        7,
        None,
        None,
    );
    host.realizer
        .retain_checkpoint_source_environment_for_disposal(
            thread,
            &prepared_operation,
            &prepared_generation,
            &binding,
            &environment,
        )
        .expect("C1 exact Preparation owner");
    let asserted_generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(thread),
        2,
        u64::MAX,
        "other-environment",
        "other-image",
    );
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &asserted_generation,
        7,
        None,
        None,
    );
    let authorization = source_disposal_authorization(&prepared_operation.effect_id);

    let error = host
        .realizer
        .dispose_prepared_checkpoint_source_environment(
            thread,
            &operation,
            &asserted_generation,
            &binding,
            &authorization,
        )
        .await
        .expect_err("R19/E1 mismatched identity is denied");
    assert!(error.to_string().contains("retained Preparation owner"));
    let retained = host
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| match &r.environment_owner {
            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause:
                    SessionEnvironmentRetirementCause::CheckpointSource {
                        operation,
                        generation,
                    },
                owned: RetiringEnvironmentOwner::Bound(owned),
            }) if operation == &prepared_operation && generation == &prepared_generation => {
                Some(owned.clone())
            }
            _ => None,
        })
        .flatten()
        .expect("R20/E2 exact owner remains in its Preparation");
    assert_eq!(retained.identity, identity(thread), "R20/E2");
    assert!(Arc::ptr_eq(&retained.environment, &environment), "R20/E2");
}

#[tokio::test]
async fn legacy_suspending_projection_keeps_exact_binding_provenance_without_fabrication() {
    // Cause/effect rule: C1 an upgrade reads a legacy Suspending row whose
    // source effect was not serialized; C2 its generation and binding remain
    // exact. Effect E1 project one `LegacyDirect` pending owner containing the
    // exact durable binding and an explicit absent effect, never a fabricated
    // `Durable` generation identity. New suspend rows take the sibling Durable
    // branch because their source effect is non-empty.
    let thread = "legacy-suspending-projection";
    let generation = generation(thread);
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &generation,
        7,
        None,
        None,
    );
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Suspending {
            operation,
            source_effect_id: Box::new(String::new()),
            source_binding: "legacy-binding".into(),
            generation,
            suspend_phase: awaken_session_state_contract::SuspendPhase::ReadyToDispose,
            checkpoint: None,
            source_release_preparation: None,
        },
    )
    .expect("E1 legacy projection");
    assert!(matches!(
        host.installed_sessions.realizations()
            .read_realization(thread, |r| r.environment_owner.clone()),
        Some(SessionEnvironmentOwner::Preparing(
            SessionEnvironmentPreparation::AwaitingAdoption {
                identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::DurableBinding {
                        binding,
                        effect: crate::environment_owner::LegacyEnvironmentEffect::Absent,
                    }
                ),
                binding: owner_binding,
            }
        )) if binding == "legacy-binding" && owner_binding == binding
    ));
}
