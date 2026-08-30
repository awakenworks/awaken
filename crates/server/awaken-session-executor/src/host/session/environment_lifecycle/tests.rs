use super::state::ProjectedEnvironmentOwner;
use super::*;
use crate::session_slot::UnboundSessionEnvironmentOrigin;

async fn environment(
    provider: &crate::session_environment::SessionEnvironmentProvider,
    thread: &str,
) -> Arc<crate::session_environment::SessionEnvironment> {
    Arc::new(
        provider
            .create(&crate::provisioning::agent_run_sandbox_spec(thread))
            .await
            .expect("test Environment"),
    )
}

fn generation(thread: &str) -> awaken_session_contract::SandboxGeneration {
    awaken_session_contract::SandboxGeneration::new(thread, 1, u64::MAX, "environment", "image")
}

fn identity(thread: &str) -> BoundSessionEnvironmentIdentity {
    BoundSessionEnvironmentIdentity::Durable {
        effect_id: format!("effect-{thread}"),
        generation: generation(thread),
    }
}

struct NonOwningEnvironmentBindingSink;

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for NonOwningEnvironmentBindingSink {
    async fn owns(&self, _session_id: &str) -> Result<bool, awaken_session_contract::RunError> {
        Ok(false)
    }

    async fn persist(
        &self,
        _receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        panic!("a non-owning binding sink must not persist")
    }
}

fn restore_request(thread: &str) -> awaken_session_contract::SandboxRestoreRequest {
    let generation = generation(thread);
    let suspend = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        6,
        None,
        None,
    );
    let checkpoint = awaken_session_contract::SandboxCheckpointRef {
        id: format!("checkpoint-{thread}"),
        format: "portable-test-v1".into(),
        digest: format!("digest-{thread}"),
        size_bytes: 7,
        created_at_unix_ms: 10,
        expires_at_unix_ms: u64::MAX,
        environment_fingerprint: generation.environment_fingerprint.clone(),
        base_image_fingerprint: generation.base_image_fingerprint.clone(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: suspend.effect_id,
    };
    let restore = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "restore",
        &generation,
        7,
        None,
        Some(&checkpoint),
    );
    awaken_session_contract::SandboxRestoreRequest {
        workspace_id: "workspace".into(),
        session_id: thread.into(),
        effect_id: restore.effect_id,
        generation_id: generation.id,
        checkpoint,
    }
}

#[test]
fn restoring_owner_is_only_an_exact_request_fence_until_durable_adoption() {
    // Cause/effect table: C1 Vacant receives exact Restoring request A; C2 A is
    // replayed; C3 request B or mismatched Resident authority arrives; C4 the
    // aggregate commits Resident for A; C5 exact restored-target disposal for A
    // succeeds. Effects: E1 one hidden Awaiting fence with no Arc; E2 no-write
    // replay; E3 reject and retain A; E4 move only to spec-aware pending durable
    // adoption, never Resident; E5 clear only the exact Awaiting fence. Rules:
    // F1=C1=>E1, F2=C2=>E2, F3=C3=>E3, F4=C4=>E4, F5=C5=>E5.
    let request = restore_request("restore-owner");
    let mut owner = SessionEnvironmentOwner::Vacant;
    owner.begin_restore(&request).expect("F1/E1");
    assert!(!owner.has_local_environment(), "F1/E1 no local Arc");
    owner.begin_restore(&request).expect("F2/E2");

    let other = restore_request("other-restore-owner");
    assert!(owner.begin_restore(&other).is_err(), "F3/E3");
    assert!(
        owner.complete_restore_target_disposal(&other).is_err(),
        "F3/E3"
    );

    let generation = generation("restore-owner");
    let exact_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: request.effect_id.clone(),
        generation: generation.clone(),
    };
    let mismatched_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: other.effect_id.clone(),
        generation: generation.clone(),
    };
    assert!(
        owner
            .install_projection(ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: mismatched_identity,
                binding: "binding".into(),
            })
            .is_err(),
        "F3/E3"
    );
    owner
        .install_projection(ProjectedEnvironmentOwner::AwaitingAdoption {
            identity: exact_identity,
            binding: "binding".into(),
        })
        .expect("F4/E4");
    assert!(matches!(
        owner,
        SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
    ));

    let mut terminal = SessionEnvironmentOwner::Vacant;
    terminal.begin_restore(&request).expect("F1/E1 terminal");
    terminal
        .complete_restore_target_disposal(&request)
        .expect("F5/E5");
    assert!(matches!(terminal, SessionEnvironmentOwner::Vacant), "F5/E5");
}

fn retry_input_mount() -> awaken_provisioning_contract::MountRequirement {
    awaken_provisioning_contract::MountRequirement {
        mount_id: "retry-input".into(),
        source: awaken_provisioning_contract::MountSource::InlineBytes {
            contents: b"retry".to_vec(),
            content_hash: None,
        },
        mount_path: ".mnt/retry-input".into(),
        access: awaken_provisioning_contract::MountAccess::ReadOnly,
        lifetime: awaken_provisioning_contract::MountLifetime::Session,
        required: true,
    }
}

fn resident(
    thread: &str,
    environment: Arc<crate::session_environment::SessionEnvironment>,
) -> SessionEnvironmentOwner {
    SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
        identity: identity(thread),
        binding: serde_json::to_string(&environment.handle()).unwrap(),
        environment,
    })
}

#[tokio::test]
async fn cancelled_cleanup_keeps_the_exact_owner_retiring_and_hidden_from_tools() {
    // Cause/effect graph: C1 Resident exact owner; C2 cleanup changes phase
    // before its first fallible/cancellable provider await; C3 that Future
    // is cancelled; C4 a different terminal effect tries to consume it.
    // Effects: E1 external reader stops seeing the owner at C2; E2 C3 retains
    // phase+effect+generation+binding+Arc for retry; E3 C4 cannot relabel it.
    // Decision rules R18/R19: C1+C2+C3 => E1+E2; C2+C4 => E3.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, "cancel-retirement").await;
    let owner = Arc::new(tokio::sync::Mutex::new(resident(
        "cancel-retirement",
        environment,
    )));
    let entered = Arc::new(tokio::sync::Notify::new());
    let task = {
        let owner = owner.clone();
        let entered = entered.clone();
        tokio::spawn(async move {
            let retirement = {
                let mut owner = owner.lock().await;
                let retirement = owner
                    .begin_retirement(
                        SessionEnvironmentRetirementCause::Terminal {
                            effect_id: "terminal-effect".into(),
                        },
                        RetirementSelection::Current,
                    )
                    .unwrap()
                    .unwrap();
                assert!(owner.resident().is_none(), "R18/E1");
                retirement
            };
            entered.notify_one();
            std::future::pending::<()>().await;
            retirement
        })
    };
    entered.notified().await;
    task.abort();
    let _ = task.await;
    let mut owner = owner.lock().await;
    assert!(
        matches!(&*owner, SessionEnvironmentOwner::Retiring(_)),
        "R19/E2"
    );
    assert!(owner.resident().is_none(), "R18/E1");
    assert!(
        owner
            .begin_retirement(
                SessionEnvironmentRetirementCause::Terminal {
                    effect_id: "different-terminal-effect".into(),
                },
                RetirementSelection::Current,
            )
            .is_err(),
        "R19/E3"
    );
    assert!(
        matches!(
            &*owner,
            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::Terminal { effect_id },
                ..
            }) if effect_id == "terminal-effect"
        ),
        "R19/E3"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_dispose_error_retains_owner_and_same_command_retry_consumes_it() {
    // Cause/effect decision table: C1 terminal end owns a Resident Workdir;
    // C2 filesystem disposal fails after Resident->Retiring; C3 the same
    // terminal effect retries after the provider fault clears. Effects: E1 C2
    // returns Err and keeps the exact hidden Arc/binding/identity/cause; E2 C3
    // retries that Arc and clears only after Terminated. Rules R19/R21:
    // C1+C2=>E1; C1+C2+C3=>E2. The sibling cancellation rule above owns the
    // Future-drop row at the same pre-I/O transition boundary.
    use std::os::unix::fs::PermissionsExt as _;

    let thread = "terminal-dispose-retry";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_test_resident_session_environment(thread, environment.clone());
    let original_permissions = std::fs::metadata(root.path()).unwrap().permissions();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

    let lifecycle = host
        .session_slots
        .read(thread, |slot| slot.lifecycle.clone())
        .unwrap();
    let lifecycle = lifecycle.lock().await;
    let first = host.end_session(thread, "terminal-effect").await;
    std::fs::set_permissions(root.path(), original_permissions).unwrap();
    assert!(first.is_err(), "R19/E1 injected dispose failure");
    assert!(
        host.session_environment(thread).await.is_none(),
        "R19/E1 hidden"
    );
    let retained = host
        .session_slots
        .read(thread, |slot| match &slot.environment_owner {
            SessionEnvironmentOwner::Retiring(retiring) => Some(retiring.clone()),
            _ => None,
        })
        .flatten()
        .expect("R19/E1 exact owner retained");
    assert!(
        Arc::ptr_eq(&retained.owned.environment(), &environment),
        "R19/E1"
    );

    host.end_session(thread, "terminal-effect")
        .await
        .expect("R21/E2 same command retry");
    drop(lifecycle);
    assert!(!host.session_slots.contains(thread), "R21/E2");
    assert_eq!(
        environment.status().await.unwrap(),
        awaken_provisioning_contract::SandboxStatus::Terminated,
        "R21/E2"
    );
}

#[tokio::test]
async fn discard_keeps_live_owner_and_exact_retry_clears_after_termination() {
    // Cause/effect decision table: C1 discard observes exact Resident Arc;
    // C2 stop/status still reports Ready; C3 the same provider owner later
    // reports Terminated; C4 retry supplies that same Arc. Effects: E1 C2
    // returns false and retains hidden Retiring; E2 C3+C4 exact-clears. Rules
    // R19/R21: C1+C2=>E1; C1+C2+C3+C4=>E2. The separate ABA rule owns !C4.
    let thread = "discard-live-retry";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_test_resident_session_environment(thread, environment.clone());

    assert!(
        !host.discard_session_environment(thread, &environment).await,
        "R19/E1 live owner is retained"
    );
    assert!(
        host.session_environment(thread).await.is_none(),
        "R19/E1 hidden"
    );
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(_))
        ),
        "R19/E1"
    );

    environment
        .dispose()
        .await
        .expect("R21/C3 terminate provider owner");
    assert!(
        host.discard_session_environment(thread, &environment).await,
        "R21/E2 exact retry clears"
    );
    assert!(host.session_environment_owner_is_vacant(thread), "R21/E2");
}

#[tokio::test]
async fn only_exact_terminated_status_clears_a_retiring_owner() {
    // Cause/effect decision table: C1 exact Retiring owner; C2 status is
    // Provisioning/Ready; C3 status query is Err (unknown); C4 status is
    // Terminated. Effects: E1 C2 retains; E2 C3 returns Err and retains;
    // E3 only C4 clears. Rules R19/R21: C1+C2=>E1, C1+C3=>E2,
    // C1+C4=>E3. A dispose Err returns before this status gate and therefore
    // has the same retained-owner effect as C3.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, "status-retirement").await;
    let mut owner = resident("status-retirement", environment);
    let retirement = owner
        .begin_retirement(
            SessionEnvironmentRetirementCause::Terminal {
                effect_id: "terminal-effect".into(),
            },
            RetirementSelection::Current,
        )
        .unwrap()
        .unwrap();

    assert!(
        !owner
            .observe_retirement_status::<()>(
                &retirement,
                Ok(awaken_provisioning_contract::SandboxStatus::Provisioning),
            )
            .unwrap(),
        "R19/E1"
    );
    assert!(
        !owner
            .observe_retirement_status::<()>(
                &retirement,
                Ok(awaken_provisioning_contract::SandboxStatus::Ready),
            )
            .unwrap(),
        "R19/E1"
    );
    assert!(
        owner
            .observe_retirement_status(&retirement, Err("unknown"))
            .is_err(),
        "R19/E2"
    );
    assert!(
        matches!(owner, SessionEnvironmentOwner::Retiring(_)),
        "R19/E1+E2"
    );
    assert!(
        owner
            .observe_retirement_status::<()>(
                &retirement,
                Ok(awaken_provisioning_contract::SandboxStatus::Terminated),
            )
            .unwrap(),
        "R21/E3"
    );
    assert!(matches!(owner, SessionEnvironmentOwner::Vacant), "R21/E3");
}

#[tokio::test]
async fn terminal_takeover_is_highest_priority_and_other_causes_cannot_overwrite() {
    // Cause/effect decision table: C1 an exact owner is already Retiring for
    // Unpublished/RecoveryDiscard/Revocation/Checkpoint; C2 Terminal arrives;
    // C3 a nonterminal or different Terminal cause arrives instead. Effects:
    // E1 C1+C2 preserves identity+binding+Arc and replaces only the cause;
    // E2 C1+C3 is rejected with the original cause unchanged. Rules R19/R21:
    // each lower cause + Terminal => E1; every other different-cause pair => E2.
    let thread = "terminal-takeover";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let generation = generation(thread);
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        7,
        None,
        None,
    );
    let lower_causes = [
        SessionEnvironmentRetirementCause::UnpublishedCandidate,
        SessionEnvironmentRetirementCause::RecoveryDiscard,
        SessionEnvironmentRetirementCause::RealizationRevocation,
        SessionEnvironmentRetirementCause::CheckpointSource {
            operation,
            generation,
        },
    ];
    for lower in lower_causes {
        let mut owner = resident(thread, environment.clone());
        let prior = owner
            .begin_retirement(lower, RetirementSelection::Current)
            .unwrap()
            .unwrap();
        let terminal = owner
            .begin_retirement(
                SessionEnvironmentRetirementCause::Terminal {
                    effect_id: "terminal-effect".into(),
                },
                RetirementSelection::Current,
            )
            .unwrap()
            .unwrap();
        assert!(prior.owned.exact_matches(&terminal.owned), "R21/E1");
        assert!(matches!(
            terminal.cause,
            SessionEnvironmentRetirementCause::Terminal { ref effect_id }
                if effect_id == "terminal-effect"
        ));
        let before = terminal.clone();
        assert!(
            owner
                .begin_retirement(
                    SessionEnvironmentRetirementCause::RealizationRevocation,
                    RetirementSelection::Current,
                )
                .is_err(),
            "R19/E2"
        );
        assert!(matches!(
            &owner,
            SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&before)
        ));
    }
}

#[tokio::test]
async fn only_recovery_and_revocation_retirements_can_reactivate() {
    // Cause/effect decision table: C1 exact Bound Retiring owner reports Ready;
    // C2 cause is RecoveryDiscard/Revocation; C3 cause is Terminal,
    // CheckpointSource, or Unpublished. Effects: E1 C1+C2 returns the same
    // Resident Arc; E2 C1+C3 fails and retains Retiring. Rules R20:
    // authorized recovery causes => E1; all other causes => E2.
    let thread = "retirement-reactivation";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    for cause in [
        SessionEnvironmentRetirementCause::RecoveryDiscard,
        SessionEnvironmentRetirementCause::RealizationRevocation,
    ] {
        let mut owner = resident(thread, environment.clone());
        let retiring = owner
            .begin_retirement(cause, RetirementSelection::Current)
            .unwrap()
            .unwrap();
        owner.reactivate_retiring(&retiring).expect("R20/E1");
        assert!(
            matches!(&owner, SessionEnvironmentOwner::Resident(owned)
            if Arc::ptr_eq(&owned.environment, &environment)),
            "R20/E1"
        );
    }
    let generation = generation(thread);
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        8,
        None,
        None,
    );
    for cause in [
        SessionEnvironmentRetirementCause::Terminal {
            effect_id: "terminal-effect".into(),
        },
        SessionEnvironmentRetirementCause::CheckpointSource {
            operation,
            generation,
        },
        SessionEnvironmentRetirementCause::UnpublishedCandidate,
    ] {
        let mut owner = resident(thread, environment.clone());
        let retiring = owner
            .begin_retirement(cause, RetirementSelection::Current)
            .unwrap()
            .unwrap();
        assert!(owner.reactivate_retiring(&retiring).is_err(), "R20/E2");
        assert!(
            matches!(&owner, SessionEnvironmentOwner::Retiring(current)
            if current.exact_matches(&retiring)),
            "R20/E2"
        );
    }
}

#[tokio::test]
async fn recovery_status_gates_frozen_provider_validation_after_owner_retirement() {
    use crate::host::worker_resolver::test_support::{AdoptionModel, test_activation};

    // Cause/effect decision table: C1 an exact ordinary Direct Resident is
    // recovered with rebuild enabled; C2 its provider status is Ready; C3 it is
    // Terminated; C4 an exact frozen provider exists; C5 it is absent. Effects:
    // E1 C1+C2 reactivates the same Environment and Runtime Arcs without provider
    // validation, adoption, or replacement; E2 C1+C3+C5 fails only after entering
    // Retiring, keeps that exact hidden owner, and never restores the Runtime;
    // E3 C1+C3+C4 clears the exact Retiring fence, leaves Runtime absent, and
    // requests one rebuild. Rules R21: Ready=>E1 regardless of C4/C5;
    // Terminated+C5=>E2; Terminated+C4=>E3. Other/unknown statuses retain
    // Retiring under the generic retirement-status table above.
    let ready_thread = "ordinary-ready-recovery";
    let ready_storage = tempfile::tempdir().unwrap();
    let ready_host =
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(ready_storage.path());
    let ready_runtime = ready_host
        .ctx_for(ready_thread, None)
        .await
        .expect("R21/C1 ordinary direct Runtime");
    let ready = ready_runtime.env.clone().expect("R21/C2 Ready Environment");
    let ready_binding = serde_json::to_string(&ready.handle()).unwrap();
    assert!(
        ready_host
            .session_slots
            .read(ready_thread, |slot| slot.published_snapshot.is_none())
            .unwrap_or(false),
        "R21/C5 ordinary direct Runtime has no frozen provider projection"
    );
    let (adopted, rebuild) = ready_host
        .adopt_bound_session_environment(
            ready_thread,
            Some(&ready_binding),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            true,
        )
        .await
        .expect("R21/E1 Ready direct owner reactivates without frozen provider");
    assert!(adopted.is_none(), "R21/E1 no by-value replacement");
    assert!(!rebuild, "R21/E1 no rebuild");
    let resident = ready_host
        .session_environment(ready_thread)
        .await
        .expect("R21/E1 Resident restored");
    assert!(Arc::ptr_eq(&resident, &ready), "R21/E1 exact Arc reused");
    let restored_runtime = ready_host
        .session_slots
        .read(ready_thread, |slot| slot.runtime.clone())
        .flatten()
        .expect("R21/E1 Runtime restored");
    assert!(
        Arc::ptr_eq(&restored_runtime, &ready_runtime),
        "R21/E1 exact Runtime Arc reused"
    );

    let missing_thread = "terminated-recovery-missing-provider";
    let missing_storage = tempfile::tempdir().unwrap();
    let missing_host =
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(missing_storage.path());
    let missing_runtime = missing_host
        .ctx_for(missing_thread, None)
        .await
        .expect("R21/C1 missing-provider Runtime");
    let terminated = missing_runtime
        .env
        .clone()
        .expect("R21/C3 terminated Environment");
    let terminated_binding = serde_json::to_string(&terminated.handle()).unwrap();
    terminated.dispose().await.expect("C3 terminated provider");
    let error = match missing_host
        .adopt_bound_session_environment(
            missing_thread,
            Some(&terminated_binding),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            true,
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("R21/E2 Terminated owner rebuilt without exact frozen provider"),
    };
    assert!(
        error
            .to_string()
            .contains("exact frozen Environment provider")
    );
    assert!(
        matches!(
            missing_host
                .session_slots
                .read(missing_thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RecoveryDiscard,
                owned,
            })) if owned.binding() == terminated_binding
                && Arc::ptr_eq(&owned.environment(), &terminated)
        ),
        "R21/E2 validation follows retirement and preserves the exact owner"
    );
    assert!(
        missing_host
            .session_slots
            .read(missing_thread, |slot| slot.runtime.is_none())
            .unwrap_or(false),
        "R21/E2 non-Ready owner never restores its Runtime"
    );

    let exact_thread = "terminated-recovery-exact-provider";
    let exact_storage = tempfile::tempdir().unwrap();
    let exact_host =
        SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(exact_storage.path());
    let exact_runtime = exact_host
        .ctx_for(exact_thread, None)
        .await
        .expect("R21/C1 exact-provider Runtime");
    let exact = exact_runtime
        .env
        .clone()
        .expect("R21/C3 exact-provider Environment");
    let exact_binding = serde_json::to_string(&exact.handle()).unwrap();
    exact.dispose().await.expect("C3 terminated exact provider");
    exact_host.session_slots.update(exact_thread, |slot| {
        slot.published_snapshot = Some(test_activation(exact_thread, "provider-fence").snapshot);
    });
    let (adopted, rebuild) = exact_host
        .adopt_bound_session_environment(
            exact_thread,
            Some(&exact_binding),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            true,
        )
        .await
        .expect("R21/E3 exact provider authorizes terminated rebuild");
    assert!(adopted.is_none(), "R21/E3 no by-value owner");
    assert!(rebuild, "R21/E3 replacement requested");
    assert!(
        exact_host.session_environment_owner_is_vacant(exact_thread),
        "R21/E3 exact Retiring owner cleared"
    );
    assert!(
        exact_host
            .session_slots
            .read(exact_thread, |slot| slot.runtime.is_none())
            .unwrap_or(false),
        "R21/E3 Terminated owner never restores its Runtime"
    );
}

#[tokio::test]
async fn stale_same_binding_retirement_cannot_clear_an_aba_replacement() {
    // Cause/effect graph: C1 two wrappers carry the same durable identity,
    // handle, and binding; C2 their Arc identities differ; C3 stale owner
    // reports Terminated after replacement entered Retiring. Effects: E1 C3
    // cannot clear replacement; E2 replacement's exact observation clears.
    // Decision rules R17/R19: C1+C2+C3=>E1; exact replacement=>E2.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let first = environment(&provider, "aba-retirement").await;
    let second = Arc::new(
        provider
            .adopt(
                &crate::provisioning::agent_run_sandbox_spec("aba-retirement"),
                &first.handle(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(first.handle(), second.handle(), "R17/C1");
    assert!(!Arc::ptr_eq(&first, &second), "R17/C2");

    let mut owner = resident("aba-retirement", first);
    let stale = owner
        .begin_retirement(
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            RetirementSelection::Current,
        )
        .unwrap()
        .unwrap();
    owner = resident("aba-retirement", second);
    let replacement = owner
        .begin_retirement(
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            RetirementSelection::Current,
        )
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

#[tokio::test]
async fn checkpoint_cleanup_cannot_relabel_a_different_durable_owner() {
    // Cause/effect graph: C1 Resident carries durable effect+generation G1;
    // C2 source cleanup asserts effect+generation G2 for the same binding;
    // C3 the suspend operation is otherwise exact. Effects: E1 cleanup fails
    // before the Resident->Retiring transition or provider I/O; E2 the exact
    // G1 identity, binding, and Arc remain Resident. Decision rule R19/R20:
    // C1+C2+C3 => E1+E2 zero effect.
    let thread = "checkpoint-owner-fence";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.handle()).unwrap();
    let source_effect_id = format!("effect-{thread}");
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.session_slots.update(thread, |slot| {
        // Test fixture write remains inside the sole lifecycle-owner module.
        slot.environment_owner = resident(thread, environment.clone());
    });
    let asserted_generation = awaken_session_contract::SandboxGeneration::new(
        thread,
        2,
        u64::MAX,
        "other-environment",
        "other-image",
    );
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &asserted_generation,
        7,
        None,
        None,
    );

    let error = host
        .dispose_checkpoint_source_environment(
            thread,
            &operation,
            &source_effect_id,
            &asserted_generation,
            &binding,
        )
        .await
        .expect_err("R19/E1 mismatched identity is denied");
    assert!(error.to_string().contains("exact resident owner"));
    let retained = host
        .session_slots
        .read(thread, |slot| match &slot.environment_owner {
            SessionEnvironmentOwner::Resident(owned) => Some(owned.clone()),
            _ => None,
        })
        .flatten()
        .expect("R20/E2 exact owner remains Resident");
    assert_eq!(retained.identity, identity(thread), "R20/E2");
    assert!(Arc::ptr_eq(&retained.environment, &environment), "R20/E2");
}

#[test]
fn legacy_suspending_projection_keeps_exact_binding_provenance_without_fabrication() {
    // Cause/effect rule: C1 an upgrade reads a legacy Suspending row whose
    // source effect was not serialized; C2 its generation and binding remain
    // exact. Effect E1 project one `LegacyDirect` pending owner containing the
    // exact durable binding and an explicit absent effect, never a fabricated
    // `Durable` generation identity. New suspend rows take the sibling Durable
    // branch because their source effect is non-empty.
    let thread = "legacy-suspending-projection";
    let generation = generation(thread);
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        7,
        None,
        None,
    );
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Suspending {
            operation,
            source_effect_id: Box::new(String::new()),
            source_binding: "legacy-binding".into(),
            generation,
            suspend_phase: awaken_session_contract::SuspendPhase::ReadyToDispose,
            checkpoint: None,
        },
    )
    .expect("E1 legacy projection");
    assert!(matches!(
        host.session_slots
            .read(thread, |slot| slot.environment_owner.clone()),
        Some(SessionEnvironmentOwner::Preparing(
            SessionEnvironmentPreparation::AwaitingAdoption {
                identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::DurableBinding {
                        binding,
                        effect: crate::session_slot::LegacyEnvironmentEffect::Absent,
                    }
                ),
                binding: owner_binding,
            }
        )) if binding == "legacy-binding" && owner_binding == binding
    ));
}

#[tokio::test]
async fn durable_adoption_without_an_owning_sink_preserves_exact_projected_identity() {
    use awaken_session_contract::SessionRuntime as _;

    // Cause/effect decision table: C1 a legacy durable Resident projection
    // supplies one exact binding identity; C2 provider adoption returns the
    // matching hidden candidate; C3 the binding sink is absent; C4 a configured
    // sink explicitly does not own this Thread. Effects: E1 C1+C2+(C3|C4)
    // returns the unchanged projected identity; E2 no new Direct receipt is
    // fabricated and a non-owner never receives persist. Rules D1=C3=>E1+E2,
    // D2=C4=>E1+E2. An owning sink remains the separate Store-read authority.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    for (suffix, install_non_owner) in [("absent", false), ("non-owner", true)] {
        let thread = format!("durable-adoption-{suffix}");
        let environment = environment(&provider, &thread).await;
        let binding = serde_json::to_string(&environment.handle()).unwrap();
        let expected = BoundSessionEnvironmentIdentity::LegacyDirect(
            LegacyDirectEnvironmentProvenance::DurableBinding {
                binding: binding.clone(),
                effect: crate::session_slot::LegacyEnvironmentEffect::Absent,
            },
        );
        let host = Arc::new(SharedHost::new(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        ));
        host.install_session_environment_owner_projection(
            &thread,
            "workspace",
            &awaken_session_contract::SessionEnvironmentState::Resident {
                binding,
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
        )
        .expect("C1 exact durable projection");
        if install_non_owner {
            crate::ManagedHost::new(host.clone())
                .install_environment_binding_sink(Arc::new(NonOwningEnvironmentBindingSink));
        }
        let candidate = host
            .begin_session_environment_adoption(&thread, environment)
            .expect("C2 hidden adoption candidate");
        assert_eq!(
            candidate.origin,
            UnboundSessionEnvironmentOrigin::DurableAdoption(expected.clone()),
            "C1+C2"
        );
        let actual = host
            .persist_environment_before_publish(&thread, &candidate)
            .await
            .expect("D1/D2 direct adoption fallback");
        assert_eq!(actual, expected, "E1/E2 {suffix}");
    }
}

#[tokio::test]
async fn ordinary_candidate_without_a_sink_gets_an_explicit_direct_receipt() {
    // Cause/effect decision table: C1 a direct Thread has no binding sink; C2
    // its hidden candidate originated from Create; C3 it originated from plain
    // Adoption without durable projected identity. Effects: E1 C1+C2 emits one
    // LegacyDirect Create receipt; E2 C1+C3 emits one LegacyDirect Adopt
    // receipt; both retain the exact Session and physical binding and claim no
    // durable generation. Rules O1=C1+C2=>E1, O2=C1+C3=>E2.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    for (thread, kind) in [
        (
            "ordinary-create-fallback",
            awaken_session_contract::SessionEnvironmentEffectKind::Create,
        ),
        (
            "ordinary-adopt-fallback",
            awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
        ),
    ] {
        let environment = environment(&provider, thread).await;
        let binding = serde_json::to_string(&environment.handle()).unwrap();
        let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
        let candidate = match kind {
            awaken_session_contract::SessionEnvironmentEffectKind::Create => host
                .begin_session_environment_preparation(thread, environment)
                .expect("O1 create candidate"),
            awaken_session_contract::SessionEnvironmentEffectKind::Adopt => host
                .begin_session_environment_adoption(thread, environment)
                .expect("O2 adoption candidate"),
        };
        let actual = host
            .persist_environment_before_publish(thread, &candidate)
            .await
            .expect("O1/O2 direct fallback");
        let BoundSessionEnvironmentIdentity::LegacyDirect(
            LegacyDirectEnvironmentProvenance::Direct(receipt),
        ) = actual
        else {
            panic!("ordinary candidate acquired a fabricated durable identity")
        };
        assert_eq!(receipt.session_id, thread, "E1/E2 Session");
        assert_eq!(receipt.kind, kind, "E1/E2 effect kind");
        assert_eq!(receipt.binding, binding, "E1/E2 binding");
        assert!(receipt.realization.is_none(), "E1/E2 direct authority");
    }
}

#[tokio::test]
async fn durable_projection_cannot_publish_a_hidden_candidate() {
    // Cause/effect rule: C1 provider return is held as one hidden Candidate;
    // C2 a matching durable Resident projection is installed before the binding
    // sink returns; C3 the sink later supplies its exact Store-read identity.
    // Effects: E1 C1+C2 remains the same hidden Candidate with zero tool exposure;
    // E2 only C3 publishes the exact Arc as Resident. This fences projection
    // replay from becoming a second publication authority.
    let thread = "candidate-projection-fence";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let candidate = host
        .begin_session_environment_preparation(thread, environment.clone())
        .expect("C1 Candidate");
    let generation = generation(thread);
    let effect_id = "store-read-effect".to_string();
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: candidate.binding.clone(),
            effect_id: Some(effect_id.clone()),
            generation: Some(generation.clone()),
            idle_since_unix_ms: None,
        },
    )
    .expect("C2 matching projection");
    let retained = host
        .prepared_session_environment(thread)
        .expect("E1 Candidate remains hidden");
    assert!(candidate.exact_matches(&retained), "E1");
    assert!(host.session_environment(thread).await.is_none(), "E1");

    let published = host
        .publish_prepared_session_environment(
            thread,
            &candidate,
            BoundSessionEnvironmentIdentity::Durable {
                effect_id,
                generation,
            },
        )
        .expect("C3 Store-read publication");
    assert!(Arc::ptr_eq(&published, &environment), "E2");
}

#[tokio::test]
async fn cold_checkpoint_source_seeds_exact_pending_owner_before_adoption() {
    // Cause/effect rule: C1 durable suspend source tuple is exact; C2 local
    // owner is Vacant; C3 frozen provider exists; C4 provider adoption fails.
    // Effects: E1 seed one Durable pending owner before the provider await;
    // E2 return Err and retain that exact effect+generation+binding for retry.
    // R20: C1+C2+C3+C4=>E1+E2, never a successful Vacant no-op.
    let thread = "cold-checkpoint-source";
    let storage = tempfile::tempdir().unwrap();
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub")
        .with_store_dir(storage.path());
    host.session_slots.update(thread, |slot| {
        slot.published_snapshot = Some(
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("checkpoint-source")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "provider", "model", "backend",
                ))
                .build(),
        );
    });
    let generation = generation(thread);
    let source_effect_id = format!("effect-{thread}");
    let binding = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
        "non-resumable",
        thread,
    ))
    .unwrap();
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        thread,
        "suspend",
        &generation,
        9,
        None,
        None,
    );
    assert!(
        host.dispose_checkpoint_source_environment(
            thread,
            &operation,
            &source_effect_id,
            &generation,
            &binding,
        )
        .await
        .is_err(),
        "R20/E2"
    );
    assert!(matches!(
        host.session_slots
            .read(thread, |slot| slot.environment_owner.clone()),
        Some(SessionEnvironmentOwner::Preparing(
            SessionEnvironmentPreparation::AwaitingAdoption {
                identity: BoundSessionEnvironmentIdentity::Durable {
                    effect_id,
                    generation: retained_generation,
                },
                binding: retained_binding,
            }
        )) if effect_id == source_effect_id
            && retained_generation == generation
            && retained_binding == binding
    ));
}

#[tokio::test]
async fn pending_durable_binding_is_not_a_successful_terminal_noop() {
    // Cause/effect graph: C1 no local Arc; C2 durable legacy binding remains
    // pending; C3 exact frozen provider authority is absent; C4 terminal
    // cleanup runs. Effects: E1 it fails closed before adopting through a Host
    // fallback; E2 it retains the pending binding for an authorized retry and
    // never returns success. Rule R20/R21: C1+C2+C3+C4=>E1+E2.
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    host.install_session_environment_owner_projection(
        "pending-terminal",
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: "not-a-sandbox-handle".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .unwrap();
    let lifecycle = host
        .session_slots
        .read("pending-terminal", |slot| slot.lifecycle.clone())
        .unwrap();
    let _lifecycle = lifecycle.lock().await;
    let error = host
        .end_session("pending-terminal", "terminal-effect")
        .await
        .expect_err("R20/E1 missing provider fails closed");
    assert!(
        error
            .to_string()
            .contains("exact frozen Environment provider")
    );
    assert_eq!(
        host.durable_session_environment_binding("pending-terminal")
            .as_deref(),
        Some("not-a-sandbox-handle"),
        "R21/E2"
    );
}

#[tokio::test]
async fn failed_revocation_retains_provider_mount_and_pending_owner_for_retry() {
    // Cause/effect table: C1 durable pending binding has no local Arc; C2 exact
    // frozen provider and SandboxSpec mount inputs exist; C3 provider adoption
    // cannot be completed; C4 revocation strips ordinary runtime authority.
    // Effects: E1 return Err; E2 retain pending binding; E3 retain the exact
    // provider publication and mount/environment projection needed by the next
    // terminal/revocation retry. Rule R19/R20: C1+C2+C3+C4=>E1+E2+E3.
    let thread = "revocation-retains-retry-authority";
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("revocation-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "provider", "model", "backend",
        ))
        .build();
    host.session_slots.update(thread, |slot| {
        slot.published_snapshot = Some(snapshot.clone());
    });
    host.register_thread_resources(
        thread,
        crate::provisioning::StagedResources {
            mounts: vec![retry_input_mount()],
            ..Default::default()
        },
    );
    let environment_projection = awaken_session_contract::EnvironmentSnapshot {
        environment_id: thread.into(),
        revision: awaken_session_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
            "revocation-environment".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::None,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    host.install_environment_projection(thread, &environment_projection)
        .expect("C2 Environment projection");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: "not-a-sandbox-handle".into(),
            effect_id: Some("durable-effect".into()),
            generation: Some(generation(thread)),
            idle_since_unix_ms: None,
        },
    )
    .expect("C1 pending owner");
    let spec_before = host.sandbox_spec(thread);
    let lifecycle = host
        .session_slots
        .read(thread, |slot| slot.lifecycle.clone())
        .unwrap();
    let _lifecycle = lifecycle.lock().await;
    assert!(
        host.retire_session_environment_for_revocation(thread)
            .await
            .is_err(),
        "R19/E1"
    );
    assert_eq!(
        host.durable_session_environment_binding(thread).as_deref(),
        Some("not-a-sandbox-handle"),
        "R20/E2"
    );
    assert_eq!(
        host.sandbox_spec(thread),
        spec_before,
        "R20/E3 mounts/projection"
    );
    assert_eq!(
        host.session_slots
            .read(thread, |slot| slot.published_snapshot.clone())
            .flatten()
            .map(|snapshot| snapshot.id),
        Some(snapshot.id),
        "R20/E3 provider publication"
    );
}

#[tokio::test]
async fn live_revocation_retains_retiring_owner_and_its_retry_inputs() {
    // Cause/effect rule: C1 revocation selects one exact Resident Arc; C2 stop
    // succeeds but status remains live; C3 ordinary runtime projections are
    // revoked. Effects: E1 retain the exact hidden Revocation Retiring owner;
    // E2 retain its frozen provider publication and SandboxSpec mount inputs for
    // terminal/revocation retry. Only an exact Terminated observation may clear
    // C1, so C2 is not a successful physical cleanup.
    let thread = "revocation-retiring-retry-authority";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SessionEnvironmentProvider::workdir(root.path());
    let environment = environment(&provider, thread).await;
    let host = SharedHost::new(Arc::new(crate::no_model::NoModelConfiguredExecutor), "stub");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("revocation-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "provider", "model", "backend",
        ))
        .build();
    host.session_slots.update(thread, |slot| {
        slot.published_snapshot = Some(snapshot.clone());
    });
    host.register_thread_resources(
        thread,
        crate::provisioning::StagedResources {
            mounts: vec![retry_input_mount()],
            ..Default::default()
        },
    );
    host.install_test_resident_session_environment(thread, environment.clone());
    let spec_before = host.sandbox_spec(thread);
    let lifecycle = host
        .session_slots
        .read(thread, |slot| slot.lifecycle.clone())
        .unwrap();
    let _lifecycle = lifecycle.lock().await;
    host.retire_session_environment_for_revocation(thread)
        .await
        .expect("C2 live revocation");
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                owned: RetiringEnvironmentOwner::Bound(owned),
            })) if Arc::ptr_eq(&owned.environment, &environment)
        ),
        "E1"
    );
    assert_eq!(host.sandbox_spec(thread), spec_before, "E2 SandboxSpec");
    assert_eq!(
        host.session_slots
            .read(thread, |slot| slot.published_snapshot.clone())
            .flatten()
            .map(|current| current.id),
        Some(snapshot.id),
        "E2 provider publication"
    );
}

#[tokio::test]
async fn cancelled_adoption_retries_the_same_hidden_candidate_without_readopting() {
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, BlockingBindingSink, test_activation,
    };
    use awaken_session_contract::SessionRuntime as _;

    // Cause/effect decision table: C1 a frozen provider adopts one durable
    // binding; C2 the Future is cancelled inside durable persistence after
    // provider return; C3 the authorized caller retries. Effects: E1 provider
    // return is captured immediately as one hidden Candidate Arc; E2 C2
    // retains that Arc and releases the lifecycle guard;
    // E3 C3 persists and publishes the same Arc without another provider
    // adoption or by-value owner. Rule R19/R20: C1+C2=>E1+E2;
    // C1+C2+C3=>E3.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-adoption-retry";
    let first = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
    let first_ctx = first.ctx_for(thread, None).await.expect("first Session");
    let handle = first_ctx.env.as_ref().expect("first Environment").handle();
    let binding = serde_json::to_string(&handle).unwrap();
    drop(first_ctx);
    drop(first);

    let host =
        Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()));
    host.session_slots.update(thread, |slot| {
        slot.published_snapshot =
            Some(test_activation(thread, "run-adoption-retry-publication").snapshot);
    });
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: Some("prior-effect".into()),
            generation: Some(awaken_session_contract::SandboxGeneration::new(
                thread,
                1,
                u64::MAX,
                "environment",
                "image",
            )),
            idle_since_unix_ms: None,
        },
    )
    .expect("durable pending adoption");
    let sink = Arc::new(BlockingBindingSink::blocked());
    crate::ManagedHost::new(host.clone()).install_environment_binding_sink(sink.clone());

    let adoption = {
        let host = host.clone();
        let binding = binding.clone();
        tokio::spawn(async move {
            host.adopt_bound_session_environment(
                thread,
                Some(&binding),
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                false,
            )
            .await
        })
    };
    sink.entered.notified().await;
    let candidate = host
        .prepared_session_environment(thread)
        .expect("R19/E1 hidden Candidate before cancellation");
    assert!(host.session_environment(thread).await.is_none(), "R19/E1");
    adoption.abort();
    let cancellation = match adoption.await {
        Err(error) => error,
        Ok(_) => panic!("R19/C2 adoption unexpectedly completed"),
    };
    assert!(cancellation.is_cancelled(), "R19/C2");
    let retained = host
        .prepared_session_environment(thread)
        .expect("R19/E2 hidden Candidate retained");
    assert!(candidate.exact_matches(&retained), "R19/E2");

    sink.unblock();
    let (adopted, rebuild) = host
        .adopt_bound_session_environment(
            thread,
            Some(&binding),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            false,
        )
        .await
        .expect("R20/E3 retry");
    assert!(adopted.is_none(), "R20/E3 no by-value duplicate");
    assert!(!rebuild, "R20/E3");
    let resident = host
        .session_environment(thread)
        .await
        .expect("R20/E3 published Resident");
    assert!(Arc::ptr_eq(&candidate.environment, &resident), "R20/E3");
    assert_eq!(sink.calls(), 2, "R19/R20 sink retries");
}
