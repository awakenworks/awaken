//! Exact owner fences: restoring, pending adoption, reservations, and retiring owners.

use super::*;

#[tokio::test]
async fn revoked_owner_waits_for_exact_monotonic_resource_readoption() {
    // Causes: R1 revoked durable B1 with current Root revision advancing to
    // same-generation/same-substrate B2 by one owned File path; R2 ordinary
    // strict projection; R3 changed generation, foreign substrate, or a
    // different retirement cause. Effects: R1 permits only the pending
    // projection and retains the old physical Arc/cleanup owner; R2/R3 reject
    // before publication or owner replacement. Provider Ready/adopt is the
    // separate physical edge, not a consequence of this projection. The
    // canonical projection call uses reservation=false for R2/R3 and true
    // only for R1; the removed two-argument wrapper encoded only false.
    let thread = "revoked-file-resource";
    let root = tempfile::tempdir().unwrap();
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let previous = environment.lifecycle().handle();
    let mut next_json = serde_json::to_value(&previous).unwrap();
    next_json["payload"]["owned_paths"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("/workspace/confirmed-2.json"));
    let next: awaken_provisioning_contract::SandboxHandle =
        serde_json::from_value(next_json).unwrap();
    assert!(previous.owned_paths_are_monotonic_to(&next));
    let binding = serde_json::to_string(&next).unwrap();
    let initial = match resident(thread, environment.clone()) {
        SessionEnvironmentOwner::Resident(owned) => owned,
        _ => unreachable!(),
    };
    let retired = || {
        SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
            cause: SessionEnvironmentRetirementCause::RealizationRevocation,
            owned: RetiringEnvironmentOwner::Bound(initial.clone()),
        })
    };
    let projection =
        |identity, binding| ProjectedEnvironmentOwner::AwaitingAdoption { identity, binding };
    let mut pending = retired();
    assert!(
        pending
            .install_projection_with_resource_reservation(
                projection(identity(thread), binding.clone()),
                true,
                false,
            )
            .is_err(),
        "R2"
    );
    pending
        .install_projection_with_resource_reservation(
            projection(identity(thread), binding.clone()),
            true,
            true,
        )
        .expect("R1 pending projection");
    assert!(
        matches!(&pending,
        SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
            cause: SessionEnvironmentRetirementCause::RealizationRevocation,
            owned: RetiringEnvironmentOwner::Bound(owned),
        }) if owned.binding == initial.binding && Arc::ptr_eq(&owned.environment, &environment)),
        "R1"
    );
    let mut different_generation = retired();
    assert!(
        different_generation
            .install_projection_with_resource_reservation(
                projection(identity("other-generation"), binding.clone()),
                true,
                true,
            )
            .is_err(),
        "R3 generation"
    );
    let mut foreign = next;
    foreign.sandbox_id = "foreign-sandbox".into();
    let mut foreign_owner = retired();
    assert!(
        foreign_owner
            .install_projection_with_resource_reservation(
                projection(identity(thread), serde_json::to_string(&foreign).unwrap()),
                true,
                true,
            )
            .is_err(),
        "R3 substrate"
    );
    let mut terminal = retired();
    if let SessionEnvironmentOwner::Retiring(ref mut retiring) = terminal {
        retiring.cause = SessionEnvironmentRetirementCause::Terminal {
            effect_id: "terminal".into(),
        };
    }
    assert!(
        terminal
            .install_projection_with_resource_reservation(
                projection(identity(thread), binding),
                true,
                true,
            )
            .is_err(),
        "R3 terminal owner"
    );
}

#[tokio::test]
async fn revoked_resource_successor_reaches_only_the_canonical_adoption_edge() {
    // C1 a bound durable owner is revoked; C2 the current root names the same
    // Sandbox plus one File path and the same generation; C3 a foreign
    // generation claims those bytes. R1=C1+C2 => decode/validate B2 but do
    // not borrow old B1 as an exact Resident; the old Arc remains Retiring.
    // R2=C1+C3 => reject before provider observation or any slot mutation.
    // Exact physical equality or an authorized monotonic successor admits the
    // original target; neither condition admits a foreign generation. The
    // retained-source fallback is a pure precomputed reference, so eager
    // `Option::or` preserves the resident/candidate/revoked priority.
    let thread = "revoked-resource-adoption";
    let root = tempfile::tempdir().unwrap();
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let initial = match resident(thread, environment.clone()) {
        SessionEnvironmentOwner::Resident(owned) => owned,
        _ => unreachable!(),
    };
    let mut successor = serde_json::to_value(environment.lifecycle().handle()).unwrap();
    successor["payload"]["owned_paths"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("/workspace/confirmed-2.json"));
    let binding = serde_json::to_string(&successor).unwrap();
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
            r.environment.owner = SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                owned: RetiringEnvironmentOwner::Bound(initial.clone()),
            });
        });
    let (_, _, resident, candidate) = host
        .realizer
        .validated_session_environment_adoption(
            thread,
            &binding,
            &provider,
            None,
            Some(generation(thread).id()),
        )
        .expect("R1 trusted later Resource receipt");
    assert!(
        resident.is_none() && candidate.is_none(),
        "R1 old Arc is not B2"
    );
    assert!(
        host.realizer
            .validated_session_environment_adoption(
                thread,
                &binding,
                &provider,
                None,
                Some(generation("foreign").id()),
            )
            .is_err(),
        "R2 foreign generation"
    );
    assert!(
        matches!(host.installed_sessions.realizations().read_realization(thread, |r| r.environment.owner.clone()),
        Some(SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
            cause: SessionEnvironmentRetirementCause::RealizationRevocation,
            owned: RetiringEnvironmentOwner::Bound(owned),
        })) if owned.binding == initial.binding && Arc::ptr_eq(&owned.environment, &environment)),
        "R1/R2 retain the sole cleanup owner"
    );
}

fn restore_request(thread: &str) -> awaken_session_state_contract::SandboxRestoreRequest {
    let generation = generation(thread);
    let suspend = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &generation,
        6,
        None,
        None,
    );
    let checkpoint = awaken_session_state_contract::SandboxCheckpointRef {
        id: format!("checkpoint-{thread}"),
        format: "portable-test-v1".into(),
        digest: format!("digest-{thread}"),
        size_bytes: 7,
        created_at_unix_ms: 10,
        expires_at_unix_ms: u64::MAX,
        environment_fingerprint: generation.environment_fingerprint().to_owned(),
        base_image_fingerprint: generation.base_image_fingerprint().to_owned(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: suspend.effect_id,
    };
    let restore = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "restore",
        &generation,
        7,
        None,
        Some(&checkpoint),
    );
    awaken_session_state_contract::SandboxRestoreRequest {
        workspace_id: "workspace".into(),
        session_id: thread.into(),
        effect_id: restore.effect_id,
        generation_id: generation.id().to_owned(),
        checkpoint,
    }
}

#[test]
fn restoring_owner_is_only_an_exact_request_fence_until_durable_adoption() {
    // Cause/effect table: C1 Vacant receives exact Restoring request A; C2 A is
    // replayed; C3 request B or mismatched Resident authority arrives; C4 the
    // aggregate commits Resident for A. Effects: E1 one hidden Awaiting fence
    // with no Arc; E2 no-write replay; E3 reject and retain A; E4 move only to
    // spec-aware pending durable adoption, never Resident. Rules: F1=C1=>E1,
    // F2=C2=>E2, F3=C3=>E3, F4=C4=>E4. Terminal physical disposal deliberately
    // does not mutate this owner: aggregate acknowledgement is the sole final
    // projection-retirement edge and is covered by the Host terminal table.
    let request = restore_request("restore-owner");
    let mut owner = SessionEnvironmentOwner::Vacant;
    owner.begin_restore(&request).expect("F1/E1");
    assert!(!owner.has_local_environment(), "F1/E1 no local Arc");
    owner.begin_restore(&request).expect("F2/E2");

    let other = restore_request("other-restore-owner");
    assert!(owner.begin_restore(&other).is_err(), "F3/E3");

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
            .install_projection_with_resource_reservation(
                ProjectedEnvironmentOwner::AwaitingAdoption {
                    identity: mismatched_identity,
                    binding: "binding".into(),
                },
                false,
                false,
            )
            .is_err(),
        "F3/E3"
    );
    owner
        .install_projection_with_resource_reservation(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: exact_identity,
                binding: "binding".into(),
            },
            false,
            false,
        )
        .expect("F4/E4");
    assert!(matches!(
        owner,
        SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption { .. })
    ));
}

#[test]
fn closed_pending_adoption_discards_only_the_pre_observation_identity() {
    // Cause/effect table: C1 pending durable owner A is captured before a
    // provider observation; C2 same-binding identity B replaces A; C3 closed
    // evidence returns for A. E1 stale A cannot clear B. With C1 still current,
    // E2 the same evidence clears only A so rebuild can proceed.
    // | Rule | Current owner | Observed identity | Effect |
    // | P1 | B, same binding | A | retain B |
    // | P2 | A | A | Vacant |
    let thread = "closed-pending-adoption-fence";
    let binding = "exact-binding";
    let observed = identity(thread);
    let replacement = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "replacement-effect".into(),
        generation: generation(thread),
    };
    let mut owner = SessionEnvironmentOwner::Vacant;
    owner
        .install_projection_with_resource_reservation(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: replacement.clone(),
                binding: binding.into(),
            },
            false,
            false,
        )
        .expect("P1 replacement projection");
    assert!(
        !owner.discard_closed_pending_adoption(&observed, binding),
        "P1/E1"
    );
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
            identity,
            binding: current,
        }) if identity == &replacement && current == binding
    ));

    owner = SessionEnvironmentOwner::Vacant;
    owner
        .install_projection_with_resource_reservation(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: observed.clone(),
                binding: binding.into(),
            },
            false,
            false,
        )
        .expect("P2 observed projection");
    assert!(
        owner.discard_closed_pending_adoption(&observed, binding),
        "P2/E2"
    );
    assert!(matches!(owner, SessionEnvironmentOwner::Vacant), "P2/E2");
}

#[tokio::test]
async fn resident_owner_absorbs_only_its_exact_monotonic_resource_reservation() {
    // Resident reservation cause/effect graph. Causes: C1 a durable Resident
    // retains one exact Arc, identity and V2 binding; C2 the provider Arc has
    // actually reserved a superset of owned paths; C3 the root projection keeps
    // the same durable generation; C4 the projected handle names the same or a
    // different substrate. Effects: E1 C1+C2+C3+same substrate atomically
    // advances identity+binding on that one owner while retaining the Arc; C5 a
    // delayed root observation carries another effect identity after the same
    // handle was already projected. E2 a foreign substrate is rejected with the
    // owner unchanged; E3 C5 is absorbed without rolling the locally published
    // identity backward. Exact replay is already covered by
    // ordinary projection tests, and the provisioning contract owns
    // path-loss/legacy/different-generation negatives.
    //
    // | Rule | Arc reserved | generation | substrate | Effect |
    // | P1 | yes | same | same | E1 |
    // | P2 | yes | same | different | E2 |
    // | P3 | already projected | same | same | E3 |
    let thread = "resident-resource-reservation";
    let root = tempfile::tempdir().unwrap();
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let resident_environment = environment(&provider, thread).await;
    let initial_binding =
        serde_json::to_string(&resident_environment.lifecycle().handle()).unwrap();
    let initial_identity = identity(thread);
    let mut owner = SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
        identity: initial_identity,
        binding: initial_binding,
        environment: resident_environment.clone(),
    });

    resident_environment
        .projection()
        .reserve_owned_path("/workspace/live.txt")
        .expect("P1 reserve provider-owned path");
    let reserved_binding =
        serde_json::to_string(&resident_environment.lifecycle().handle()).unwrap();
    let reserved_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "resource-reservation-effect".into(),
        generation: generation(thread),
    };
    owner
        .install_projection_with_resource_reservation(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: reserved_identity.clone(),
                binding: reserved_binding.clone(),
            },
            false,
            false,
        )
        .expect("P1 absorb exact root reservation");
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Resident(owned)
            if owned.identity == reserved_identity
                && owned.binding == reserved_binding
                && Arc::ptr_eq(&owned.environment, &resident_environment)
    ));

    let same_binding_identity = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "same-binding-resource-receipt".into(),
        generation: generation(thread),
    };
    owner
        .install_projection_with_resource_reservation(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: same_binding_identity.clone(),
                binding: reserved_binding.clone(),
            },
            false,
            false,
        )
        .expect("P3 absorb an unordered receipt for the same physical handle");
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Resident(owned)
            if owned.identity == reserved_identity
                && owned.binding == reserved_binding
                && Arc::ptr_eq(&owned.environment, &resident_environment)
    ));

    let foreign = environment(&provider, "foreign-resource-reservation").await;
    foreign
        .projection()
        .reserve_owned_path("/workspace/foreign.txt")
        .expect("P2 reserve foreign path");
    let foreign_binding = serde_json::to_string(&foreign.lifecycle().handle()).unwrap();
    assert!(
        owner
            .install_projection_with_resource_reservation(
                ProjectedEnvironmentOwner::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "foreign-resource-reservation-effect".into(),
                        generation: generation(thread),
                    },
                    binding: foreign_binding,
                },
                false,
                false,
            )
            .is_err(),
        "P2 foreign substrate remains fenced"
    );
    assert!(matches!(
        &owner,
        SessionEnvironmentOwner::Resident(owned)
            if owned.identity == reserved_identity
                && owned.binding == reserved_binding
                && Arc::ptr_eq(&owned.environment, &resident_environment)
    ));
}

#[tokio::test]
async fn realization_revocation_absorbs_only_a_compatible_durable_receipt() {
    // Revoked-owner cause/effect graph. C1 a durable Resident enters the sole
    // RealizationRevocation phase; C2 Control has committed a later Resource
    // receipt for the same generation and exact physical Arc; C3 a projection
    // instead names a different generation. E1 C1+C2 absorbs the unordered
    // receipt while preserving the exact Retiring fence and Arc; E2 C1+C3
    // rejects and preserves that revoked owner.
    //
    // | Rule | cause | generation/handle | Effect |
    // | V1 | realization revocation | same | E1, retain fence |
    // | V2 | realization revocation | different | E2 |
    let thread = "revoked-resource-receipt";
    let root = tempfile::tempdir().unwrap();
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
    let mut owner = resident(thread, environment.clone());
    let retirement = owner
        .begin_retirement(
            SessionEnvironmentRetirementCause::RealizationRevocation,
            RetirementSelection::Current,
        )
        .unwrap()
        .expect("V1 revoked owner");

    let replacement = BoundSessionEnvironmentIdentity::Durable {
        effect_id: "committed-resource-receipt".into(),
        generation: generation(thread),
    };
    owner
        .install_projection_with_resource_reservation(
            ProjectedEnvironmentOwner::AwaitingAdoption {
                identity: replacement.clone(),
                binding: binding.clone(),
            },
            false,
            false,
        )
        .expect("V1 same physical generation absorbs its receipt");
    let retained = owner.clone();
    assert!(
        matches!(&owner, SessionEnvironmentOwner::Retiring(current)
            if current.exact_matches(&retirement)),
        "V1 unordered same-binding observation retains the exact retirement fence"
    );

    assert!(
        owner
            .install_projection_with_resource_reservation(
                ProjectedEnvironmentOwner::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "foreign-generation-receipt".into(),
                        generation: awaken_session_state_contract::SandboxGeneration::new(
                            &awaken_session_types::SessionId::from(thread),
                            2,
                            u64::MAX,
                            "environment",
                            "image",
                        ),
                    },
                    binding,
                },
                false,
                false,
            )
            .is_err(),
        "V2 different generation is rejected"
    );
    assert!(matches!(
        (&owner, &retained),
        (SessionEnvironmentOwner::Retiring(current), SessionEnvironmentOwner::Retiring(before))
            if current.exact_matches(before)
    ));
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
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
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
    tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
        .await
        .expect("timed out waiting for entered.notified()");
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

#[tokio::test]
async fn source_free_projection_does_not_reopen_mcp_before_retiring_owner_is_disposed() {
    // Cause/effect table for checkpoint-expiry ordering: Q1 a quiescence fence
    // is closed while its source owner is Resident; Q2 cleanup has moved that
    // owner to Retiring but physical disposal is incomplete; Q3 the durable
    // projection is already source-free Unmaterialized; Q4 exact cleanup later
    // proves Terminated/Vacant. Effects: E1 Q2+Q3 retains the fence; E2 only
    // Q3+Q4 atomically consumes it, admitting the fresh realization generation.
    let thread = "expiry-before-source-dispose";
    let root = tempfile::tempdir().unwrap();
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    host.install_test_resident_session_environment(thread, environment.clone());
    let generation = generation(thread);
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &generation,
        2,
        None,
        None,
    );
    host.installed_sessions
        .realizations()
        .close_mcp_realization_admission(
            thread,
            awaken_session_mcp::McpQuiescenceAdmissionFence::new(
                &operation,
                "source-effect",
                "source-binding",
                &generation,
            ),
        )
        .unwrap();

    assert!(
        !host.discard_session_environment(thread, &environment).await,
        "Q2 keeps a live source in Retiring"
    );
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
    )
    .unwrap();
    assert!(
        !host
            .installed_sessions
            .realizations()
            .mcp_realization_admitted(thread),
        "E1 expiry before disposal cannot reopen"
    );

    dispose_current_environment(&environment, thread).await;
    assert!(
        host.discard_session_environment(thread, &environment).await,
        "Q4 exact retry confirms Terminated/Vacant"
    );
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
    )
    .unwrap();
    assert!(
        host.installed_sessions
            .realizations()
            .mcp_realization_admitted(thread),
        "E2 exact source-free+Vacant projection admits a new realization"
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
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    host.install_test_resident_session_environment(thread, environment.clone());

    assert!(
        !host.discard_session_environment(thread, &environment).await,
        "R19/E1 live owner is retained"
    );
    assert!(
        host.realizer.session_environment(thread).await.is_none(),
        "R19/E1 hidden"
    );
    assert!(
        matches!(
            host.installed_sessions
                .realizations()
                .read_realization(thread, |r| r.environment.owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(_))
        ),
        "R19/E1"
    );

    dispose_current_environment(&environment, thread).await;
    assert!(
        host.discard_session_environment(thread, &environment).await,
        "R21/E2 exact retry clears"
    );
    assert!(
        host.realizer.session_environment_owner_is_vacant(thread),
        "R21/E2"
    );
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
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
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
    let provider = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
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
async fn legacy_revocation_rebuild_consumes_only_exact_typed_observation() {
    // Legacy rebuild cause/effect table: C1 an aggregate-Unowned LegacyDirect
    // owner is Retiring after realization revocation; C2 the caller has no
    // claimed-realization authority; C3 the exact provider observation is Ready
    // or DefinitivelyUnavailable. Effects: E1 C2 retains the exact Retiring
    // owner and performs no I/O; E2 Ready disposes the exact old root and clears
    // only that owner; E3 Unavailable consumes its exact closed evidence and
    // clears only that owner. Rules CP1=C1+C2=>E1, CP2=C1+Ready=>E2,
    // CP3=C1+Unavailable=>E3. The claimed end-to-end creation edge is owned by
    // `claimed_rebuild_disposes_legacy_v1_before_projection_install`; this table
    // deliberately tests only the reusable typed-observation owner. Provisioning,
    // incompatible, indeterminate, and foreign-incarnation rows remain
    // fail-closed under the provider observation/closed-evidence tables.
    for (thread, preclosed) in [
        ("claimed-legacy-revocation-ready", false),
        ("claimed-legacy-revocation-absent", true),
    ] {
        assert_legacy_rebuild_observation(thread, preclosed).await;
    }
}

async fn assert_legacy_rebuild_observation(thread: &str, preclosed: bool) {
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, install_complete_projection_for_snapshot, test_activation,
    };

    let root = tempfile::tempdir().unwrap();
    let host = (crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub")
        .await
        .with_store_dir(root.path())
        .await)
        .build()
        .expect("validated Host dependencies");
    // Fixture constraint: the complete projection uses the production Managed
    // runtime only to select the same frozen provider. This test then calls the
    // typed lifecycle owner directly; claimed end-to-end ordering remains in the
    // synchronizer test named above.
    let managed = crate::ManagedHost::new(host.clone());
    let snapshot = test_activation(thread, "claimed-legacy-publication").snapshot;
    install_complete_projection_for_snapshot(
        &managed,
        thread,
        "workspace",
        eager_environment(),
        &snapshot,
    )
    .await;
    // The retained wrapper and rebuild observation must use the same provider
    // selected by the frozen projection. A second LocalProvider root would be
    // a parallel physical authority and could not observe this exact marker.
    let provider = host
        .realizer
        .projected_session_environment_provider(thread, None)
        .expect("C1 frozen Environment provider");
    let spec = host
        .realizer
        .staging
        .sandbox_spec_for_provider(thread, provider);
    // LegacyDirect is the aggregate-Unowned creation row. Reuse the single
    // unfenced test entry into production creation so the handle is the exact
    // marker-free V1 that this compatibility path owns; a fenced V2 fixture
    // would require aggregate disposal authorization that does not exist here.
    let environment = Arc::new(
        host.realizer
            .create_session_environment(provider, &spec)
            .await
            .expect("C1 exact LegacyDirect Environment"),
    );
    let retired_sentinel = root
        .path()
        .join("sandboxes")
        .join(thread)
        .join("retired-owner-sentinel");
    std::fs::write(&retired_sentinel, b"owned by the retiring Environment")
        .expect("C1 retiring root sentinel");
    host.install_test_resident_session_environment(thread, environment.clone());

    let lifecycle = host
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| r.environment.lifecycle.clone())
        .unwrap();
    {
        let _lifecycle = lifecycle.lock().await;
        assert!(
            host.realizer
                .retire_session_environment_for_revocation(
                    thread,
                    (&host.mcp, host.installed_sessions.as_ref())
                )
                .await
                .unwrap(),
            "C2 exact revocation"
        );
    }
    let retired = host
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| r.environment.owner.clone())
        .unwrap();
    assert!(
        matches!(
            &retired,
            SessionEnvironmentOwner::Retiring(RetiringSessionEnvironment {
                cause: SessionEnvironmentRetirementCause::RealizationRevocation,
                owned: RetiringEnvironmentOwner::Bound(owned),
            }) if matches!(
                owned.identity,
                BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::Direct(_)
                )
            ) && Arc::ptr_eq(&owned.environment, &environment)
        ),
        "C2 exact legacy/direct owner"
    );

    let unclaimed = match host
        .ctx_for_snapshot(
            thread,
            Some(snapshot.root_agent_id.0.as_str()),
            Some(snapshot.clone()),
        )
        .await
    {
        Ok(_) => panic!("CP1 unclaimed lookup rebuilt the Environment"),
        Err(error) => error,
    };
    assert!(
        unclaimed
            .to_string()
            .contains("Environment transition must be recovered"),
        "CP1/E1"
    );
    assert!(
        matches!(
            host.installed_sessions.realizations()
                .read_realization(thread, |r| r.environment.owner.clone()),
            Some(SessionEnvironmentOwner::Retiring(current)) if current.exact_matches(
                match &retired {
                    SessionEnvironmentOwner::Retiring(current) => current,
                    _ => unreachable!(),
                }
            )
        ),
        "CP1/E1 exact Retiring owner retained"
    );
    if preclosed {
        environment
            .lifecycle()
            .dispose()
            .await
            .expect("CP3 exact root already unavailable");
        assert_eq!(
            environment.sandbox_environment().status().await.unwrap(),
            awaken_provisioning_contract::SandboxStatus::Terminated,
            "CP3 unavailable observation precondition"
        );
    }

    {
        let _lifecycle = lifecycle.lock().await;
        host.realizer
            .rebuild_claimed_legacy_environment_after_revocation(thread, provider)
            .await
            .expect("CP2/CP3 exact typed observation consumes the retirement");
    }
    assert!(
        !retired_sentinel.exists(),
        "CP2/E2 or CP3/E3 exact retired root is absent"
    );
    assert_eq!(
        environment.sandbox_environment().status().await.unwrap(),
        awaken_provisioning_contract::SandboxStatus::Terminated,
        "CP2/E2 or CP3/E3 old wrapper remains terminal"
    );
    assert!(
        matches!(
            host.installed_sessions
                .realizations()
                .read_realization(thread, |r| r.environment.owner.clone()),
            Some(SessionEnvironmentOwner::Vacant)
        ),
        "CP2/E2 or CP3/E3 exact Retiring fence is consumed"
    );
}

#[test]
fn owner_after_projection_derives_a_new_owner_and_keeps_the_held_one() {
    // Decision table over a vacant held owner. C1 the durable Environment
    // state; effects: A1 Unmaterialized projects Vacant; A2 a Resident with
    // effect and generation projects an exact durable adoption; A3 a
    // generation without its effect is refused (fail closed); A4 in every row
    // the held owner is unchanged, because installing waits for the slot lock.
    let thread = "after-projection";
    let held = SessionEnvironmentOwner::Vacant;
    let unmaterialized = awaken_session_state_contract::SessionEnvironmentState::Unmaterialized;
    let projected = held
        .after_projection(thread, "ws", &unmaterialized, true, false)
        .expect("A1");
    assert!(matches!(projected, SessionEnvironmentOwner::Vacant), "A1");
    let resident = |effect_id: Option<&str>| {
        awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "binding-a".into(),
            effect_id: effect_id.map(str::to_owned),
            generation: Some(generation(thread)),
            idle_since_unix_ms: None,
        }
    };
    let projected = held
        .after_projection(thread, "ws", &resident(Some("effect-a")), true, false)
        .expect("A2");
    assert!(
        matches!(&projected,
            SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::AwaitingAdoption {
                identity: BoundSessionEnvironmentIdentity::Durable { effect_id, .. },
                binding,
            }) if effect_id == "effect-a" && binding == "binding-a"),
        "A2"
    );
    assert!(
        held.after_projection(thread, "ws", &resident(None), true, false)
            .is_err(),
        "A3"
    );
    assert!(matches!(held, SessionEnvironmentOwner::Vacant), "A4");
}
