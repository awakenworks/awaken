// @formal evidence session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Hidden candidate adoption, root commit, and projection evidence.

use super::*;

struct NonOwningEnvironmentBindingSink;

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink
    for NonOwningEnvironmentBindingSink
{
    async fn authorize(
        &self,
        _intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        Ok(awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned)
    }

    async fn persist(
        &self,
        _receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentState,
        awaken_session_types::RunError,
    > {
        panic!("a non-owning binding sink must not persist")
    }
}

#[tokio::test]
async fn durable_adoption_without_an_owning_sink_preserves_exact_projected_identity() {
    // Cause/effect decision table: C1 a legacy durable Resident projection
    // supplies one exact binding identity; C2 provider adoption returns the
    // matching hidden candidate; C3 the binding sink is absent; C4 a configured
    // sink explicitly does not own this Thread. Effects: E1 C1+C2+(C3|C4)
    // returns the unchanged projected identity; E2 no new Direct receipt is
    // fabricated and a non-owner never receives persist. Rules D1=C3=>E1+E2,
    // D2=C4=>E1+E2. An owning sink remains the separate Store-read authority.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    for (suffix, install_non_owner) in [("absent", false), ("non-owner", true)] {
        let thread = format!("durable-adoption-{suffix}");
        let environment = environment(&provider, &thread).await;
        let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
        let expected = BoundSessionEnvironmentIdentity::LegacyDirect(
            LegacyDirectEnvironmentProvenance::DurableBinding {
                binding: binding.clone(),
                effect: crate::environment_owner::LegacyEnvironmentEffect::Absent,
            },
        );
        let host = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await)
            .build()
            .expect("validated Host dependencies");
        host.install_session_environment_owner_projection(
            &thread,
            "workspace",
            &awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding,
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
        )
        .expect("C1 exact durable projection");
        if install_non_owner {
            crate::ManagedHost::new(host.clone())
                .install_test_environment_binding_sink(Arc::new(NonOwningEnvironmentBindingSink));
        }
        let candidate = host
            .realizer
            .begin_session_environment_adoption(&thread, environment)
            .expect("C2 hidden adoption candidate");
        assert_eq!(
            candidate.origin,
            UnboundSessionEnvironmentOrigin::AdoptionFrom(expected.clone()),
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
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    for (thread, kind) in [
        (
            "ordinary-create-fallback",
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        ),
        (
            "ordinary-adopt-fallback",
            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt,
        ),
    ] {
        let environment = environment(&provider, thread).await;
        let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
        let host = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await)
            .build()
            .expect("validated Host dependencies");
        let candidate = match kind {
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create => host
                .realizer.begin_session_environment_preparation(thread, environment)
                .expect("O1 create candidate"),
            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt => host
                .realizer.begin_session_environment_adoption(thread, environment)
                .expect("O2 adoption candidate"),
            awaken_session_state_contract::SessionEnvironmentEffectKind::Rebuild { .. }
            | awaken_session_state_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                ..
            } => unreachable!("O1/O2 enumerate only direct Create and Adopt effects"),
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
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let candidate = host
        .realizer
        .begin_session_environment_preparation(thread, environment.clone())
        .expect("C1 Candidate");
    let generation = generation(thread);
    let effect_id = "store-read-effect".to_string();
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Resident {
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
    assert!(
        host.realizer.session_environment(thread).await.is_none(),
        "E1"
    );

    let published = host
        .realizer
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
async fn adopted_candidate_retains_successor_receipt_after_dispatch_fencing() {
    // Cause/effect sequence: C1 cold projection or warm revoked owner names
    // source receipt A; C2 adoption keeps its physical generation and Arc;
    // C3 root commits receipt B but derivative dispatch binding is fenced.
    // A1 C1+C2+C3 => retain B hidden, never confuse source A with committed B.
    // A2 replay B => unchanged owner; A3 a later different receipt/generation
    // after B => reject without mutation. Publication consumes only exact B.
    // This covers the omitted cross-boundary commit/readback/fence prefix;
    // existing terminal tests own disposal, not this source-to-receipt edge.
    for warm in [false, true] {
        let thread = "adopt-successor-receipt";
        let root = tempfile::tempdir().unwrap();
        let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
            .into_session_provider();
        let environment = environment(&provider, thread).await;
        let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
        let source = identity(thread);
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
            &awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding,
                effect_id: Some(format!("effect-{thread}")),
                generation: Some(generation(thread)),
                idle_since_unix_ms: None,
            },
        )
        .unwrap();
        let candidate = host
            .realizer
            .begin_session_environment_adoption(thread, environment.clone())
            .unwrap();
        if warm {
            host.realizer
                .publish_prepared_session_environment(thread, &candidate, source.clone())
                .unwrap();
            host.installed_sessions.update_realization(thread, |r| {
                let retiring = r
                    .environment_owner
                    .begin_retirement(
                        SessionEnvironmentRetirementCause::RealizationRevocation,
                        state::RetirementSelection::Current,
                    )
                    .unwrap()
                    .unwrap();
                r.environment_owner
                    .prepare_retiring_realization_adoption(&retiring, environment.clone())
                    .unwrap();
            });
        }
        let candidate = host.prepared_session_environment(thread).unwrap();
        let successor = BoundSessionEnvironmentIdentity::Durable {
            effect_id: "successor-adopt".into(),
            generation: generation(thread),
        };
        // A4 a source with a foreign physical generation cannot be relabelled,
        // whether dispatch succeeded (publish) or failed (retain).
        let foreign = BoundSessionEnvironmentIdentity::Durable {
            effect_id: "successor-adopt".into(),
            generation: generation("different-physical-owner"),
        };
        for publish in [false, true] {
            let result = if publish {
                host.realizer
                    .publish_prepared_session_environment(thread, &candidate, foreign.clone())
                    .map(|_| ())
            } else {
                host.realizer
                    .retain_committed_session_environment_candidate(
                        thread,
                        &candidate,
                        foreign.clone(),
                    )
                    .map(|_| ())
            };
            assert!(result.is_err(), "A4");
            assert!(
                host.prepared_session_environment(thread)
                    .unwrap()
                    .exact_matches(&candidate)
            );
        }
        let retained = host
            .realizer
            .retain_committed_session_environment_candidate(thread, &candidate, successor.clone())
            .expect("A1 source receipt is not this adoption's committed receipt");
        assert_eq!(retained.durable_identity(), Some(&successor));
        assert!(Arc::ptr_eq(&retained.environment, &environment));
        assert!(host.realizer.session_environment(thread).await.is_none());
        let replay = host
            .realizer
            .retain_committed_session_environment_candidate(thread, &retained, successor.clone())
            .unwrap();
        assert!(replay.exact_matches(&retained), "A2");
        assert!(
            host.realizer
                .retain_committed_session_environment_candidate(thread, &retained, source)
                .is_err(),
            "A3"
        );
        assert!(
            host.prepared_session_environment(thread)
                .unwrap()
                .exact_matches(&retained)
        );
        assert!(
            host.realizer
                .publish_prepared_session_environment(thread, &retained, foreign)
                .is_err(),
            "A3 publication uses the same receipt guard"
        );
        let published = host
            .realizer
            .publish_prepared_session_environment(thread, &retained, successor)
            .unwrap();
        assert!(Arc::ptr_eq(&published, &environment));
    }
}

#[tokio::test]
async fn adoption_root_commit_and_stale_dispatch_retry_use_one_candidate_identity() {
    use crate::host::worker_resolver::test_support::{BlockingBindingSink, claim};

    use awaken_session_execution_contract::SessionEnvironmentBindingSink;
    // Boundary decision table B: C1 source root receipt A; C2 owning sink
    // commits Adopt B; C3 SQLite dispatch claim current/stale; C4 retry count.
    // B1 C1+C2+stale => dispatch_claim_stale (never internal), same Arc hidden
    // with B. B2 repeat stale twice => exact B, no publication/new physical I/O.
    // B3 install current claim => exact same Arc becomes Resident with B and
    // dispatch binds the same handle. Cross B with direct success to require
    // identical durable identity irrespective of the derivative-write outcome.
    // Sink is the existing deterministic receipt fixture, not a root CAS proof;
    // the real SQLite queue owns claim replacement and binding admission here.
    for fenced in [false, true] {
        let thread = "adoption-dispatch-retry";
        let root = tempfile::tempdir().unwrap();
        let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
            .into_session_provider();
        let environment = environment(&provider, thread).await;
        let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .unwrap(),
        );
        let host = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await
        .with_ephemeral_dispatch(store.clone()))
        .build()
        .expect("validated Host dependencies");
        let sink = Arc::new(BlockingBindingSink::blocked());
        sink.unblock();
        crate::ManagedHost::new(host.clone()).install_test_environment_binding_sink(sink.clone());
        let source_intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
            thread,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
            None,
        );
        let source = sink
            .persist(
                awaken_session_state_contract::SessionEnvironmentReceipt::from_intent(
                    &source_intent,
                    binding.clone(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        host.install_session_environment_owner_projection(thread, "workspace", &source)
            .unwrap();
        let old = claim(&store, thread, "adoption-run", "worker-old", 1, &clock).await;
        let current = if fenced {
            clock.set(2_002);
            store
                .claim("worker-current", 60_000, &Default::default())
                .await
                .unwrap()
                .unwrap()
        } else {
            old.clone()
        };
        host.installed_sessions.update_run(thread, |run| {
            run.session_dispatch = true;
            run.dispatch_claim = Some(awaken_dispatch_contract::RunClaim::from(&old.lease));
        });
        let effect = host
            .authorize_environment_effect_before_io(
                thread,
                awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt,
                Some(&binding),
            )
            .await
            .unwrap();
        let result = host
            .publish_session_environment_under_lifecycle(
                thread,
                environment.clone(),
                &effect,
                crate::staged_resources::EnvironmentResourceReconciliation::Adopted,
            )
            .await;
        if fenced {
            let error = result.err().expect("B1 derivative fence");
            assert_eq!(error.code, "dispatch_claim_stale");
            let candidate = host.prepared_session_environment(thread).unwrap();
            let expected = candidate.durable_identity().cloned().unwrap();
            assert!(
                matches!(&expected, BoundSessionEnvironmentIdentity::Durable { effect_id, .. } if effect_id == effect.intent.effect_id())
            );
            for _ in 0..2 {
                let error = host
                    .retry_session_environment_candidate_under_lifecycle(thread)
                    .await
                    .err()
                    .expect("B2 repeated fence");
                assert_eq!(error.code, "dispatch_claim_stale");
                assert!(
                    host.prepared_session_environment(thread)
                        .unwrap()
                        .exact_matches(&candidate)
                );
                assert!(host.realizer.session_environment(thread).await.is_none());
            }
            host.installed_sessions.update_run(thread, |run| {
                run.dispatch_claim =
                    Some(awaken_dispatch_contract::RunClaim::from(&current.lease));
            });
            let published = host
                .retry_session_environment_candidate_under_lifecycle(thread)
                .await
                .unwrap()
                .unwrap();
            assert!(Arc::ptr_eq(&published, &environment), "B3");
            assert!(
                host.installed_sessions
                    .read_realization(thread, |r| matches!(&r.environment_owner,
                SessionEnvironmentOwner::Resident(owned) if owned.identity == expected))
                    .unwrap()
            );
        } else {
            assert!(
                Arc::ptr_eq(&result.unwrap(), &environment),
                "B direct success"
            );
        }
        assert!(host.installed_sessions.read_realization(thread, |r| matches!(&r.environment_owner,
            SessionEnvironmentOwner::Resident(owned) if matches!(&owned.identity,
                BoundSessionEnvironmentIdentity::Durable { effect_id, generation }
                    if effect_id == effect.intent.effect_id() && Some(generation) == source.generation()
            ) && owned.binding == binding
        )).unwrap(), "B direct/fenced paths publish the same current receipt");
        // Observe the persisted binding through the next claim; calling
        // bind_sandbox again here could repair a missing write and mask a bug.
        clock.set(62_003);
        let readback = store
            .claim("worker-readback", 60_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            readback.sandbox.as_deref(),
            Some(binding.as_str()),
            "B3 durable dispatch binding"
        );
    }
}

#[tokio::test]
async fn committed_hidden_create_candidate_is_terminally_retired_without_publication() {
    // Cause/effect decision table for the Dream cancel/create race: C1 one
    // Create Candidate owns the exact provider Arc; C2 the Session root commit
    // returns a durable identity; C3 the derivative dispatch binding is fenced
    // before Resident publication; C4 terminal cleanup takes over. Effects:
    // E1 C1+C2+C3 retains one hidden DurableCreate with the original Create
    // retry kind and exact binding; E2 no tool-visible Resident appears; E3
    // C4 converts that same Arc and durable identity into the sole Bound
    // terminal-retirement owner. Rule D1=C1+C2+C3=>E1+E2;
    // D2=D1+C4=>E3. A mismatched replay is rejected by the Kani-proved closed
    // Candidate commit reducer and cannot relabel Create as Adopt.
    let thread = "committed-candidate-terminal-takeover";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let candidate = host
        .realizer
        .begin_session_environment_preparation(thread, environment.clone())
        .expect("C1 hidden Create Candidate");
    let identity = identity(thread);
    let committed = host
        .realizer
        .retain_committed_session_environment_candidate(thread, &candidate, identity.clone())
        .expect("C2/C3 retain Store-read identity");
    assert!(matches!(
        &committed.origin,
        UnboundSessionEnvironmentOrigin::DurableCreate(current) if current == &identity
    ));
    assert_eq!(
        committed.effect_kind(),
        awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        "E1"
    );
    assert_eq!(
        host.realizer.durable_environment_binding(thread).as_deref(),
        Some(candidate.binding.as_str()),
        "E1"
    );
    assert!(
        host.realizer.session_environment(thread).await.is_none(),
        "E2"
    );

    let cause = SessionEnvironmentRetirementCause::Terminal {
        effect_id: "terminal-candidate-takeover".into(),
    };
    let retirement = host
        .installed_sessions
        .update_realization(thread, |r| {
            r.environment_owner
                .begin_retirement_with_committed_candidate(&committed, cause)
        })
        .expect("C4 terminal takeover");
    let RetiringEnvironmentOwner::Bound(owned) = retirement.owned else {
        panic!("E3 committed Candidate must become one Bound terminal owner")
    };
    assert_eq!(owned.identity, identity, "E3 identity");
    assert_eq!(owned.binding, candidate.binding, "E3 binding");
    assert!(Arc::ptr_eq(&owned.environment, &environment), "E3 Arc");
    assert!(
        host.realizer.session_environment(thread).await.is_none(),
        "E3 hidden"
    );
}

#[tokio::test]
async fn candidate_revocation_and_projection_preserve_exact_commit_evidence() {
    use awaken_session_state_contract::SessionEnvironmentEffectKind::{Adopt, Create};
    // Causes: C1 Create/Adopt origin; C2 Store-read identity retained or absent;
    // C3 selected Arc exact/foreign; C4 revocation/unpublished cleanup;
    // C5 projected identity exact/same-generation receipt/new generation;
    // C6 projected binding exact/foreign. Effects and decision rules:
    // CR1 !C3 => no transition, original Candidate unchanged.
    // CR2 C3+C2+revocation => same Arc/binding/identity becomes Bound Retiring.
    // CR3 C3+(!C2 or cleanup) => Unbound Retiring, no invented durable identity.
    // CP1 !C6 or (C2+new generation) => reject, same Candidate retained.
    // CP2 all remaining projections => accept but NEVER publish a Candidate.
    // Fully cross C1-C4 for retirement and C1+C2 with each independent identity
    // conflict for projection. Same-generation receipt stutter is intentional;
    // without Store readback a projection alone cannot establish commitment.
    // RP1 exact retired binding + revocation => confirm projection without
    // changing Bound/Unbound phase; RP2 wrong binding/cleanup cause => reject.
    // RA1 exact revocation + same physical rebound target => one new hidden
    // Adopt Candidate, preserving only known identity; RA2 foreign target,
    // stale retirement, wrong cause or replay => reject without owner change.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let owned = environment(&provider, "candidate-evidence").await;
    let foreign = environment(&provider, "candidate-foreign").await;
    let rebound = Arc::new(
        provider
            .adoption()
            .rebind_retained(
                &crate::provisioning::agent_run_sandbox_spec("candidate-evidence"),
                &owned.lifecycle().handle(),
                &owned,
                crate::session_environment::ProviderEffect::Unattached,
            )
            .await
            .unwrap(),
    );
    for kind in [Create, Adopt] {
        for committed in [false, true] {
            let mut base = SessionEnvironmentOwner::Vacant;
            let mut candidate = base.begin_preparing(owned.clone(), kind.clone()).unwrap();
            let identity = identity("candidate-evidence");
            if committed {
                candidate = base
                    .retain_committed_candidate(&candidate, identity.clone())
                    .unwrap();
            }
            for exact in [false, true] {
                for cause in [
                    SessionEnvironmentRetirementCause::RealizationRevocation,
                    SessionEnvironmentRetirementCause::UnpublishedCandidate,
                ] {
                    let mut owner = base.clone();
                    let retirement = owner
                        .begin_retirement(
                            cause.clone(),
                            RetirementSelection::Exact(if exact { &owned } else { &foreign }),
                        )
                        .unwrap();
                    if !exact {
                        assert!(retirement.is_none(), "CR1");
                        assert!(
                            owner
                                .unpublished_candidate()
                                .unwrap()
                                .exact_matches(&candidate)
                        );
                        continue;
                    }
                    let retirement = retirement.unwrap();
                    assert!(Arc::ptr_eq(&retirement.owned.environment(), &owned));
                    match &retirement.owned {
                        RetiringEnvironmentOwner::Bound(bound) => {
                            assert!(
                                committed
                                    && cause
                                        == SessionEnvironmentRetirementCause::RealizationRevocation,
                                "CR2"
                            );
                            assert_eq!(bound.identity, identity);
                            assert_eq!(bound.binding, candidate.binding);
                        }
                        RetiringEnvironmentOwner::Unbound(unbound) => {
                            assert!(
                                !committed
                                    || cause
                                        == SessionEnvironmentRetirementCause::UnpublishedCandidate,
                                "CR3"
                            );
                            assert!(unbound.exact_matches(&candidate));
                        }
                    }
                    for exact_binding in [false, true] {
                        let mut projected_owner = owner.clone();
                        let result = projected_owner.install_projection_with_resource_reservation(
                            ProjectedEnvironmentOwner::AwaitingAdoption {
                                identity: identity.clone(),
                                binding: if exact_binding {
                                    candidate.binding.clone()
                                } else {
                                    "foreign".into()
                                },
                            },
                            true,
                            false,
                        );
                        assert_eq!(
                            result.is_ok(),
                            exact_binding
                                && cause
                                    == SessionEnvironmentRetirementCause::RealizationRevocation,
                            "RP1/RP2"
                        );
                        assert!(
                            matches!(&projected_owner, SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement)),
                            "RP no promotion"
                        );
                    }
                    assert!(
                        owner
                            .prepare_retiring_realization_adoption(&retirement, foreign.clone())
                            .is_err(),
                        "RA2 wrong target"
                    );
                    assert!(
                        matches!(&owner, SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement))
                    );
                    let mut stale = retirement.clone();
                    stale.cause = SessionEnvironmentRetirementCause::RecoveryDiscard;
                    assert!(
                        owner
                            .prepare_retiring_realization_adoption(&stale, rebound.clone())
                            .is_err(),
                        "RA2 stale owner"
                    );
                    let result =
                        owner.prepare_retiring_realization_adoption(&retirement, rebound.clone());
                    if cause == SessionEnvironmentRetirementCause::RealizationRevocation {
                        result.expect("RA1");
                        let fresh = owner.unpublished_candidate().unwrap();
                        assert!(Arc::ptr_eq(&fresh.environment, &rebound));
                        assert_eq!(fresh.binding, candidate.binding);
                        assert_eq!(fresh.effect_kind(), Adopt);
                        assert_eq!(fresh.durable_identity(), committed.then_some(&identity));
                        let fresh = fresh.clone();
                        assert!(
                            owner
                                .prepare_retiring_realization_adoption(&retirement, rebound.clone())
                                .is_err(),
                            "RA2 replay"
                        );
                        assert!(owner.unpublished_candidate().unwrap().exact_matches(&fresh));
                    } else {
                        assert!(result.is_err(), "RA2 wrong cause");
                        assert!(
                            matches!(&owner, SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement))
                        );
                    }
                }
            }
            for rule in ["exact", "receipt", "generation", "binding"] {
                let mut owner = base.clone();
                let mut projected = identity.clone();
                let mut binding = candidate.binding.clone();
                if let BoundSessionEnvironmentIdentity::Durable {
                    effect_id,
                    generation,
                } = &mut projected
                {
                    match rule {
                        "receipt" => effect_id.push_str("-successor"),
                        "generation" => *generation = foreign_generation(generation),
                        "binding" => binding.push_str("-foreign"),
                        _ => {}
                    }
                }
                let result = owner.install_projection_with_resource_reservation(
                    ProjectedEnvironmentOwner::AwaitingAdoption {
                        identity: projected,
                        binding,
                    },
                    true,
                    false,
                );
                assert_eq!(
                    result.is_err(),
                    rule == "binding" || (committed && rule == "generation"),
                    "CP1/CP2 {kind:?} {committed} {rule}"
                );
                assert!(
                    owner
                        .unpublished_candidate()
                        .unwrap()
                        .exact_matches(&candidate),
                    "CP1/CP2 stays hidden"
                );
            }
        }
    }
}

#[tokio::test]
async fn unacknowledged_candidate_adoption_requires_aggregate_authority() {
    // UA1 exact retained V2 Candidate + no installed aggregate lease/sink =>
    // reject before Provider observation/adoption and keep exact Retiring owner.
    // A raw local handle is provenance for recovery, never authority by itself.
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let thread = "unacknowledged-no-authority";
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let candidate = host
        .realizer
        .begin_session_environment_preparation(thread, environment.clone())
        .unwrap();
    let retirement = host
        .realizer
        .retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::RealizationRevocation,
            RetirementSelection::Exact(&environment),
        )
        .unwrap()
        .unwrap();
    let result = host
        .adopt_bound_session_environment(thread, Some(&candidate.binding), &provider, None, false)
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("UA1 unowned adoption admitted"),
    };
    assert!(
        error
            .to_string()
            .contains("requires current aggregate adoption authority"),
        "UA1: {error}"
    );
    host.installed_sessions
        .read_realization(thread, |r| {
            assert!(matches!(&r.environment_owner,
        SessionEnvironmentOwner::Retiring(current) if current.exact_matches(&retirement.owner)))
        })
        .unwrap();
    assert!(host.realizer.session_environment(thread).await.is_none());
}

#[tokio::test]
async fn stale_unmaterialized_projection_cannot_unbind_a_candidate_or_resident() {
    // Projection-order cause/effect decision table: C1 an Unmaterialized
    // FrozenSessionProjection is read before the lifecycle guard; C2 the exact
    // create Candidate is installed while C1 waits; C3 the binding sink commits
    // and the same Candidate becomes Resident before C1 installs; C4 durable
    // retirement has already moved the owner to Retiring/Vacant. Effects: E1
    // C1+C2 retains the exact hidden Candidate and returns success; E2 C1+C3
    // retains the exact Resident Arc/identity and returns success; E3 C1+C4 may
    // confirm Vacant but never disposes a live owner. Rules S1=C1+C2=>E1,
    // S2=C1+C3=>E2; the existing retirement tests own S3=C1+C4=>E3. This makes
    // stale projection replay a no-write operation instead of an unrecoverable
    // rescheduling loop.
    let thread = "stale-unmaterialized-projection";
    let root = tempfile::tempdir().unwrap();
    let provider = crate::session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let environment = environment(&provider, thread).await;
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let candidate = host
        .realizer
        .begin_session_environment_preparation(thread, environment.clone())
        .expect("S1 Candidate");

    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
    )
    .expect("S1 stale projection is a no-write replay");
    let retained = host
        .prepared_session_environment(thread)
        .expect("S1 Candidate retained");
    assert!(candidate.exact_matches(&retained), "S1 exact Candidate");
    assert!(
        host.realizer.session_environment(thread).await.is_none(),
        "S1 hidden"
    );

    let published = host
        .realizer
        .publish_prepared_session_environment(thread, &candidate, identity(thread))
        .expect("S2 publish exact Candidate");
    host.install_session_environment_owner_projection(
        thread,
        "workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
    )
    .expect("S2 stale projection is a no-write replay");
    let resident = host
        .realizer
        .session_environment(thread)
        .await
        .expect("S2 Resident retained");
    assert!(Arc::ptr_eq(&published, &resident), "S2 exact Resident Arc");
    assert!(Arc::ptr_eq(&environment, &resident), "S2 provider Arc");
}

/// The same Sandbox coordinates minted for another Session: a generation whose
/// id cannot match the aggregate's.
fn foreign_generation(
    generation: &awaken_session_state_contract::SandboxGeneration,
) -> awaken_session_state_contract::SandboxGeneration {
    awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from("foreign-session"),
        generation.created_at_unix_ms(),
        generation.expires_at_unix_ms(),
        generation.environment_fingerprint(),
        generation.base_image_fingerprint(),
    )
}

use awaken_dispatch_contract::DispatchClaims as _;
