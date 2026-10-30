// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
use super::*;
use awaken_session_execution_contract::RealizationRuntime as _;
#[cfg(target_os = "linux")]
use awaken_session_realization::peers::EnvironmentPeer as _;

// B1-B4: after the synchronous close, remove/recreate the SAME Session id with
// the SAME closed lease (or Closed(None)); its new owner lock may even be held.
// Effects: stale cleanup neither waits on new locks nor consumes the replacement
// claim/installed Session; its own later selection remains usable. Cross both captured owner
// locks with leased/uninstalled projections. Equality of lease bytes alone is
// deliberately insufficient here; real production installed Session allocations own the ABA.
#[tokio::test]
async fn realization_revocation_selection_fences_removed_and_recreated_installed_sessions() {
    for lifecycle in [false, true] {
        for leased in [false, true] {
            let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
                .build()
                .expect("validated Host dependencies");
            let lease = crate::session_resource::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/1".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            };
            host.run_assembly
                .installed_sessions
                .install_realization(SESSION, |r| {
                    if leased {
                        r.install_lease(
                            awaken_session_realization::test_support::test_observed_lease(
                                lease.clone(),
                            ),
                        );
                    }
                });
            let lock = host
                .run_assembly
                .installed_sessions
                .realizations()
                .read_realization(SESSION, |r| {
                    if lifecycle {
                        r.environment().lifecycle.clone()
                    } else {
                        r.execution().phase.clone()
                    }
                })
                .expect("installed Sessions")
                .unwrap();
            let held = lock.lock().await;
            let selection = host
                .run_assembly
                .begin_session_realization_revocation()
                .expect("installed Sessions");
            host.run_assembly.installed_sessions.remove(SESSION);
            let claim = awaken_dispatch_contract::RunClaim {
                run_id: RunId("replacement".into()),
                owner: "worker".into(),
                epoch: 2,
            };
            host.run_assembly
                .installed_sessions
                .install_with(SESSION, |installed| {
                    if leased {
                        installed.realization.install_lease(
                            awaken_session_realization::test_support::test_observed_lease(
                                lease.clone(),
                            ),
                        );
                    }
                    installed.run.dispatch_claim = Some(claim.clone());
                })
                .expect("installed Sessions");
            let replacement = host
                .run_assembly
                .begin_session_realization_revocation()
                .expect("installed Sessions");
            let replacement_lock = host
                .run_assembly
                .installed_sessions
                .realizations()
                .read_realization(SESSION, |r| r.environment().lifecycle.clone())
                .expect("installed Sessions")
                .unwrap();
            let replacement_held = replacement_lock.lock().await;
            drop(held);
            assert_eq!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    host.run_assembly
                        .finish_session_realization_revocation(selection)
                )
                .await
                .unwrap()
                .unwrap(),
                0,
                "old selection does not enter replacement locks or cleanup"
            );
            assert_eq!(
                host.run_assembly
                    .installed_sessions
                    .read_run(SESSION, |run| run.dispatch_claim.clone())
                    .expect("installed Sessions"),
                Some(Some(claim))
            );
            drop(replacement_held);
            assert_eq!(
                host.run_assembly
                    .finish_session_realization_revocation(replacement)
                    .await
                    .unwrap(),
                1,
                "replacement retains its own cleanup capability"
            );
        }
    }
}

// W1-W3: all installed Sessions are closed atomically BEFORE drain/lock waits. One Session
// holds its realization lock while another is free; independent cleanup must
// progress regardless of HashMap order. Caller cancellation retains the closed
// pending owner and a fresh selection finishes it. Empty retry is a no-op.
#[tokio::test]
async fn worker_revocation_closes_all_before_drain_and_joins_independent_cleanup() {
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let mut cancellations = Vec::new();
    for session in ["held", "free"] {
        host.run_assembly
            .installed_sessions
            .install_realization(session, |r| {
                r.install_lease(
                    awaken_session_realization::test_support::test_observed_lease(
                        crate::session_resource::SessionRealizationLease {
                            owner: "worker".into(),
                            runtime_incarnation: "worker/1".into(),
                            epoch: 1,
                            expires_at_unix_ms: u64::MAX,
                        },
                    ),
                );
                let cancellation = Arc::new(awaken_runtime_contract::CancellationToken::new());
                r.staging_mut().preparation = Some(cancellation.clone());
                cancellations.push(cancellation);
            });
    }
    // Observe the existing catalog order without inserting/removing keys.
    // Hold its FIRST selection so a serial-cleanup mutant cannot pass merely
    // because the free Session happened to be enumerated first.
    let ids = host
        .run_assembly
        .installed_sessions
        .session_ids()
        .expect("installed Sessions");
    let held_session = &ids[0];
    let free_session = &ids[1];
    let held_lock = host
        .run_assembly
        .installed_sessions
        .realizations()
        .realization_lock(held_session)
        .expect("installed Sessions");
    let held = held_lock.lock().await;
    let revocation = host
        .run_assembly
        .begin_session_realization_revocation()
        .expect("installed Sessions");
    assert!(
        host.run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")
            .is_empty(),
        "W1 before any await"
    );
    assert!(
        cancellations.iter().all(|c| c.is_cancelled()),
        "W1 cancels all preparation"
    );
    let task_host = host.clone();
    let cleanup = tokio::spawn(async move {
        task_host
            .run_assembly
            .finish_session_realization_revocation(revocation)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while host.run_assembly.installed_sessions.contains(free_session) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("W2 one held lock cannot block unrelated cleanup");
    assert!(
        !cleanup.is_finished(),
        "W2 pending owner is not a completion receipt"
    );
    cleanup.abort();
    assert!(cleanup.await.unwrap_err().is_cancelled());
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization(held_session, |r| r.execution().authority.is_closed())
            .expect("installed Sessions")
            .unwrap(),
        "W3"
    );
    drop(held);
    assert_eq!(
        host.run_assembly
            .revoke_all_session_realizations()
            .await
            .unwrap(),
        1,
        "W3 retry"
    );
    assert_eq!(
        host.run_assembly
            .revoke_all_session_realizations()
            .await
            .unwrap(),
        0,
        "W3 empty"
    );
}

// W4: one selected Session has no adopted physical owner and must return a
// retryable error; another is cleanable. Effects: retain the exact failed owner
// and its closed authority, finish the independent cleanup, then report error.
// Repeating the failed batch cannot manufacture a successful cleanup receipt.
#[tokio::test]
async fn worker_revocation_error_does_not_short_circuit_other_sessions() {
    use crate::environment_owner::{
        BoundSessionEnvironmentIdentity, SessionEnvironmentOwner, SessionEnvironmentPreparation,
    };
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    for id in ["left", "right"] {
        host.run_assembly
            .installed_sessions
            .install_with(id, |_| ())
            .expect("installed Sessions");
    }
    // As in W2, make the first selected installed Session fail, independently of the
    // HashMap seed. A short-circuit implementation must leave the peer behind.
    let ids = host
        .run_assembly
        .installed_sessions
        .session_ids()
        .expect("installed Sessions");
    let unknown = &ids[0];
    let free = &ids[1];
    host.run_assembly
        .installed_sessions
        .realizations()
        .update_realization(unknown, |r| {
            r.environment_mut().owner = SessionEnvironmentOwner::Preparing(
                SessionEnvironmentPreparation::AwaitingAdoption {
                    identity: BoundSessionEnvironmentIdentity::Durable {
                        effect_id: "unadopted-effect".into(),
                        generation: awaken_session_contract::SandboxGeneration::new(
                            &awaken_session_types::SessionId::from(unknown.as_str()),
                            1,
                            u64::MAX,
                            "environment",
                            "image",
                        ),
                    },
                    binding: "unadopted-binding".into(),
                },
            );
        })
        .expect("installed Sessions");
    for _ in 0..2 {
        let error = host
            .run_assembly
            .revoke_all_session_realizations()
            .await
            .expect_err("W4 unknown is not complete");
        assert_eq!(
            error.code,
            "session_environment_revocation_pending_adoption"
        );
        assert!(
            !host.run_assembly.installed_sessions.contains(free),
            "W4 independent cleanup completed"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization(unknown, |r| {
                    r.execution().authority.is_closed()
                        && matches!(
                            r.environment().owner,
                            SessionEnvironmentOwner::Preparing(
                                SessionEnvironmentPreparation::AwaitingAdoption { .. }
                            )
                        )
                })
                .expect("installed Sessions")
                .unwrap(),
            "W4 retry owner remains closed and available"
        );
    }
}

// Authority decision table A1-A8. Causes: C1 realization/lifecycle owner lock
// is held; C2 a successful renewal reply is in flight; C3 exact/Worker-wide
// revocation closes A; C4 cleanup is cancelled or B replaces A before unlock.
// Effects: E1 no lease is visible to admission/scheduling; E2 no MCP assertion
// fallback; E3 delayed renewal/reinstallation cannot reopen A; E4 cancellation
// leaves closure retryable; E5 stale cleanup leaves B and its claim untouched.
// Cross product {two locks} x {exact, all} x {cancel, replace} selects eight
// rules. Notify/Control semaphore barriers force the actual production edges;
// no sleep establishes ordering, and no test-only lease reducer is involved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn realization_revocation_closes_admission_before_waiting_for_owner_locks() {
    for lifecycle in [false, true] {
        for all in [false, true] {
            for replace in [false, true] {
                let control = Arc::new(RemoteTerminalCleanupControl::default());
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                *control.renewal_gate.lock().unwrap() = Some(gate.clone());
                let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
                    .await
                    .with_session_realization_control(control.clone())
                    .with_authority(
                        crate::UpstreamAuthority::default()
                            .with_terminal_cleanup_control(control.clone()),
                    ))
                .build()
                .expect("validated Host dependencies");
                let now = awaken_session_realization::test_support::wall_clock_now_ms();
                let lease = crate::session_resource::SessionRealizationLease {
                    owner: "worker".into(),
                    runtime_incarnation: "worker/1".into(),
                    epoch: 1,
                    expires_at_unix_ms: now + 5_000,
                };
                install_test_renewal_authority(&host, &control, SESSION, lease.clone());
                let generation = crate::session_mcp::McpGenerationRef::from_persisted(
                    SESSION.into(),
                    crate::session_mcp::McpGeneration::of_bare_target(
                        crate::session_mcp::McpAttachmentId("mcp".into()),
                        1,
                        &awaken_agent_contract::McpTarget::parse_http("https://fixture.test/mcp")
                            .unwrap(),
                    ),
                    lease.runtime_incarnation.clone(),
                    lease.epoch,
                    lease.expires_at_unix_ms,
                )
                .expect("MCP generation fence epoch is nonzero");
                assert!(
                    host.run_assembly
                        .mcp_generation_is_authorized_at(&generation, now)
                        .expect("healthy registry")
                );
                let h = host.clone();
                let renewal = tokio::spawn(async move {
                    h.run_assembly
                        .renew_due_session_realizations(
                            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(
                                30_000,
                            ),
                        )
                        .await
                });
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while control.renewal_sessions.lock().unwrap().is_empty() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("C2 request reached Control barrier");
                let (lock, changed) = host
                    .run_assembly
                    .installed_sessions
                    .realizations()
                    .read_realization(SESSION, |r| {
                        (
                            if lifecycle {
                                r.environment().lifecycle.clone()
                            } else {
                                r.execution().phase.clone()
                            },
                            r.execution().changed.clone(),
                        )
                    })
                    .expect("installed Sessions")
                    .unwrap();
                let held = lock.lock().await;
                let closed = changed.notified();
                tokio::pin!(closed);
                closed.as_mut().enable();
                let h = host.clone();
                let expected = lease.clone();
                let realization = host
                    .run_assembly
                    .installed_sessions
                    .realizations()
                    .realization_lock(SESSION)
                    .expect("installed Sessions");
                let mut revocation = tokio::spawn(async move {
                    if all {
                        h.run_assembly
                            .revoke_all_session_realizations()
                            .await
                            .map(|n| n != 0)
                    } else {
                        h.run_assembly
                            .revoke_session_realization(SESSION, Some((&expected, &realization)))
                            .await
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut closed)
                    .await
                    .expect("C3 close precedes async lock wait");
                assert!(!revocation.is_finished());
                assert!(
                    host.run_assembly
                        .installed_sessions
                        .realization_leases()
                        .expect("installed Sessions")
                        .is_empty(),
                    "E1"
                );
                assert!(
                    host.realization_executor
                        .environments()
                        .authorize_environment_effect_before_io(
                            host.realization_executor.peers(),
                            SESSION,
                            &host
                                .realization_executor
                                .authority()
                                .new_environment_effect_intent(
                                    SESSION,
                                    awaken_session_contract::SessionEnvironmentEffectKind::Create,
                                    None
                                )
                                .expect("installed Sessions"),
                        )
                        .await
                        .is_err(),
                    "E1 closed is not an unowned/direct Environment fallback"
                );
                assert_eq!(
                    host.run_assembly
                        .mcp_generation_root_authority(&generation)
                        .expect("healthy registry"),
                    Some(false),
                    "E2"
                );
                assert!(
                    !host
                        .run_assembly
                        .mcp_generation_is_authorized_at(&generation, now)
                        .expect("healthy registry"),
                    "E2"
                );
                gate.add_permits(1);
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), renewal)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap(),
                    0,
                    "E3 delayed successful reply discarded"
                );
                let mut extended = lease.clone();
                extended.expires_at_unix_ms += 120_000;
                assert!(
                    host.run_assembly
                        .install_session_realization_lease(SESSION, extended.clone())
                        .is_err()
                );
                assert!(
                    host.run_assembly
                        .installed_sessions
                        .realization_leases()
                        .expect("installed Sessions")
                        .is_empty(),
                    "E3 assertion cannot reopen"
                );
                let managed = host.clone();
                assert!(
                    awaken_session_execution_contract::RealizationRuntime::renew_session_realization_lease(
                        &*managed.run_assembly,
                        SESSION,
                        awaken_session_realization::test_support::test_observed_lease(extended.clone()),
                    )
                    .await
                    .is_err(),
                    "E3 public renewal port rejects closed authority"
                );
                if !replace {
                    revocation.abort();
                    assert!((&mut revocation).await.unwrap_err().is_cancelled());
                    assert!(
                        host.run_assembly
                            .installed_sessions
                            .realization_leases()
                            .expect("installed Sessions")
                            .is_empty(),
                        "E4"
                    );
                }
                extended.epoch += 1;
                host.run_assembly
                    .install_session_realization_lease(SESSION, extended.clone())
                    .unwrap();
                let claim = awaken_dispatch_contract::RunClaim {
                    run_id: RunId("replacement".into()),
                    owner: "worker".into(),
                    epoch: 2,
                };
                host.run_assembly
                    .installed_sessions
                    .install_run(SESSION, |run| run.dispatch_claim = Some(claim.clone()));
                drop(held);
                if replace {
                    assert!(
                        !tokio::time::timeout(std::time::Duration::from_secs(2), revocation)
                            .await
                            .unwrap()
                            .unwrap()
                            .unwrap(),
                        "E5 stale retirement discarded"
                    );
                }
                assert_eq!(
                    host.run_assembly
                        .installed_sessions
                        .read(SESSION, |installed| (
                            installed.realization.execution().authority.lease().cloned(),
                            installed.run.dispatch_claim.clone(),
                        ))
                        .expect("installed Sessions"),
                    Some((Some(extended), Some(claim))),
                    "E5"
                );
            }
        }
    }
}

#[derive(Default)]
struct GatedFileSource {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl crate::FileContentSource<crate::ResourceOperationFence> for GatedFileSource {
    async fn read(
        &self,
        _workspace: &str,
        file_id: &str,
        _purpose: &awaken_resource_contract::FileReadPurpose,
        _authority: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        Option<awaken_resource_contract::ResolvedFileContent>,
        awaken_resource_contract::FileContentSourceError,
    > {
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail.load(Ordering::SeqCst) {
            return Err(awaken_resource_contract::FileContentSourceError::new(
                "injected read failure",
            ));
        }
        let bytes = b"immutable teaching input".to_vec();
        Ok(Some(awaken_resource_contract::ResolvedFileContent {
            file_id: file_id.into(),
            content_id: awaken_resource_contract::content_id(&bytes),
            filename: "input.txt".into(),
            media_type: "text/plain".into(),
            bytes,
        }))
    }
}

struct StagingFixture {
    host: Arc<ExecutorComponents>,
    control: Arc<RemoteTerminalCleanupControl>,
    source: Arc<GatedFileSource>,
    projection: awaken_session_realization_contract::FrozenSessionProjection,
    lease: crate::session_resource::SessionRealizationLease,
    now: u64,
}

const SESSION: &str = "cold-resource-lease";

#[tokio::test]
async fn expired_session_receipt_cannot_publish_any_complete_projection_facts() {
    // Receiver migration rule O1: the same InstalledSessions receives the
    // original realizer and MCP owners directly. Waiting/expired proof causes
    // must retain the no-publication/no-I/O effects below without RunAssembly.

    // C1 a complete frozen projection waits on its canonical Resource lock;
    // C2 its initially positive local proof expires during that wait; C3
    // Resource revision is zero or nonzero. E1 both paths reject; E2 neither
    // publishes publication/Environment/baseline/dispatch/context coordinates;
    // E2 also leaves root-version admission unchanged, so a failed higher root
    // cannot fence a later retry before any complete projection was installed.
    // E3 no File read begins. R1 C1+C2+zero covers final publication, while R2
    // C1+C2+nonzero covers the preparation entrance before Resource I/O.
    // Polling once proves the awaited lock boundary; no scheduler-yield count
    // substitutes for reaching it. The real monotonic sleep only ages proof,
    // never manufactures a different durable lease or resets the request start.
    for resources in [false, true] {
        let f = StagingFixture::new().await;
        let mut projection = f.projection.clone();
        if !resources {
            projection.resource_revision = 0;
            projection.resources = Default::default();
        }
        let resource_lock = f
            .host
            .run_assembly
            .installed_sessions
            .install_realization(SESSION, |r| r.staging().projection.clone());
        let held = resource_lock.lock().await;
        let proof = awaken_session_realization_contract::SessionRealizationLeaseReceipt {
            lease: f.lease.clone(),
            remaining_ms: std::num::NonZeroU64::new(3).unwrap(),
        }
        .observed(awaken_authority_clock::authority_proof_started().unwrap());
        let install = f
            .host
            .run_assembly
            .installed_sessions
            .install_frozen_session_projection(
                (
                    &f.host.run_assembly.realization_executor,
                    &f.host.run_assembly.mcp,
                ),
                SESSION,
                projection,
                None,
                true,
                Some(proof.clone()),
            );
        tokio::pin!(install);
        assert!(
            futures_util::poll!(&mut install).is_pending(),
            "R1/R2 waits on Resource lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        assert!(
            proof
                .remaining_at(awaken_authority_clock::authority_proof_started().unwrap())
                .is_none()
        );
        drop(held);
        let error = install.await.unwrap_err();
        assert!(
            error.to_string().contains("authority") || error.to_string().contains("time proof")
        );
        assert!(
            f.host
                .run_assembly
                .installed_sessions
                .read(SESSION, |installed| installed.inputs.workspace.is_none()
                    && installed.inputs.agent_id.is_none()
                    && installed.inputs.published_snapshot.is_none()
                    && installed.inputs.environment_projection.is_none()
                    && installed.inputs.environment_snapshot.is_none()
                    && installed.inputs.baseline.is_none()
                    && installed.realization.execution().frozen_receipt.is_none()
                    && installed.realization.staging().transition.is_none()
                    && !installed.run.session_dispatch
                    && installed.inputs.request_context.is_empty()
                    && installed
                        .realization
                        .execution()
                        .authority
                        .lease()
                        .is_none())
                .expect("installed Sessions")
                .unwrap(),
            "R1/R2 no partial complete projection"
        );
        assert!(
            futures_util::poll!(Box::pin(f.source.entered.notified())).is_pending(),
            "E3"
        );
    }
}

#[tokio::test]
async fn projection_installation_coherent_default_source_keeps_every_zero_resource_mode() {
    // Cause/effect graph: C1 receiver and physical executor share the original
    // default registry; C2 mode is Dispatch, zero-resource Synchronize,
    // ValidateInstalled or Terminal; C3 no preparation/File I/O is required.
    // E1 every entry installs the exact frozen root through the same identity;
    // E2 only the supplied realization modes install the original live lease;
    // E3 no Runtime or File effect is manufactured. D1=C1+C2+C3 -> E1+E2+E3.
    // The ordinary nonzero staging rows below own actual preparation/File I/O.
    for variant in ["dispatch", "zero", "validate", "terminal"] {
        let f = StagingFixture::new().await;
        let installed = &f.host.run_assembly.installed_sessions;
        let mut projection = f.projection.clone();
        projection.resource_revision = 0;
        projection.resources = Default::default();
        let expected_receipt =
            awaken_session_realization_contract::FrozenProjectionReceipt::from_projection(
                &projection,
            )
            .unwrap();
        let physical = (&*f.host.realization_executor, &f.host.run_assembly.mcp);
        let proof = awaken_session_realization::test_support::test_observed_lease(f.lease.clone());
        let result = match variant {
            "dispatch" => {
                installed
                    .install_dispatch_frozen_session_projection(physical, SESSION, projection)
                    .await
            }
            "terminal" => {
                let assignment =
                    awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                        environment_creation_not_attempted: false,
                        session_id: SESSION.into(),
                        projection,
                        authority: awaken_session_realization::test_support::test_lease_receipt(
                            f.lease.clone(),
                        ),
                    };
                installed
                    .install_terminal_cleanup_projection(physical, &assignment, proof)
                    .await
            }
            _ => {
                installed
                    .install_frozen_session_projection(
                        physical,
                        SESSION,
                        projection,
                        None,
                        variant != "validate",
                        Some(proof),
                    )
                    .await
            }
        };
        result.unwrap_or_else(|error| panic!("D1/{variant}: {error}"));
        let observed = installed
            .read(SESSION, |s| {
                (
                    s.realization.execution().frozen_receipt.clone(),
                    s.realization.execution().authority.lease().cloned(),
                    s.inputs.baseline.is_some(),
                    s.run.context.is_none(),
                    s.realization.staging().preparation.is_none(),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(observed.0, Some(expected_receipt), "{variant}/E1");
        assert_eq!(
            observed.1,
            (variant != "dispatch").then(|| f.lease.clone()),
            "{variant}/E2"
        );
        assert!(observed.2 && observed.3 && observed.4, "{variant}/E1+E3");
        assert!(
            futures_util::poll!(Box::pin(f.source.entered.notified())).is_pending(),
            "{variant}/E3"
        );
    }
}

#[tokio::test]
async fn projection_installation_refuses_foreign_registration_before_resource_effects() {
    // Cause/effect graph: C1 receiver A and physical source B are independent;
    // C2 B is present with the exact same frozen facts under another original
    // phase Arc (!C2 means absent); C3 mode is Dispatch, Synchronize with File,
    // zero-resource Synchronize, ValidateInstalled, or Terminal. Effects: E1
    // absent -> session_realization_absent; present -> generation_changed;
    // E2 neither owner publishes correlated facts or a lease/preparation;
    // E3 no original File read starts, and B is never installed by observation.
    // Decision table: F1=C1+!C2 -> E1(absent)+E2+E3; F2=C1+C2 ->
    // E1(changed)+E2+E3, crossed with every C3. Identical facts intentionally
    // cannot stand in for allocation identity. The File row advances the root
    // revision so a missing gate would reach actual staging, not receipt refusal.
    for variant in ["dispatch", "file", "zero", "validate", "terminal"] {
        for foreign_present in [false, true] {
            let a = StagingFixture::new().await;
            let b = StagingFixture::new().await;
            let installed = &a.host.run_assembly.installed_sessions;
            let foreign = &b.host.run_assembly.installed_sessions;
            let mut baseline = a.projection.clone();
            baseline.resource_revision = 0;
            baseline.resources = Default::default();
            installed
                .install_dispatch_frozen_session_projection(
                    (&a.host.realization_executor, &a.host.run_assembly.mcp),
                    SESSION,
                    baseline.clone(),
                )
                .await
                .unwrap();
            if foreign_present {
                foreign
                    .install_dispatch_frozen_session_projection(
                        (&b.host.realization_executor, &b.host.run_assembly.mcp),
                        SESSION,
                        baseline.clone(),
                    )
                    .await
                    .unwrap();
                let original = installed
                    .realizations()
                    .installed_projection(SESSION)
                    .unwrap()
                    .unwrap();
                let other = foreign
                    .realizations()
                    .installed_projection(SESSION)
                    .unwrap()
                    .unwrap();
                assert!(
                    !Arc::ptr_eq(&original.installation, &other.installation),
                    "F2 distinct actual registrations"
                );
                assert_eq!(
                    installed
                        .read(SESSION, |s| s.inputs.baseline.clone())
                        .unwrap(),
                    foreign
                        .read(SESSION, |s| s.inputs.baseline.clone())
                        .unwrap(),
                    "F2 identical frozen baseline facts",
                );
                assert_eq!(
                    installed
                        .read(SESSION, |s| s
                            .realization
                            .execution()
                            .frozen_receipt
                            .clone())
                        .unwrap(),
                    foreign
                        .read(SESSION, |s| s
                            .realization
                            .execution()
                            .frozen_receipt
                            .clone())
                        .unwrap(),
                    "F2 identical complete root receipts",
                );
            }
            let a_inputs = installed
                .read(SESSION, |s| s.inputs.clone())
                .unwrap()
                .unwrap();
            let b_inputs = foreign.read(SESSION, |s| s.inputs.clone()).unwrap();
            let mut incoming = if variant == "file" {
                a.projection.clone()
            } else {
                baseline.clone()
            };
            incoming.revision = awaken_session_types::SessionRevision(baseline.revision.0 + 1);
            let physical = (&*b.host.realization_executor, &b.host.run_assembly.mcp);
            let proof =
                awaken_session_realization::test_support::test_observed_lease(a.lease.clone());
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                match variant {
                    "dispatch" => {
                        installed
                            .install_dispatch_frozen_session_projection(physical, SESSION, incoming)
                            .await
                    }
                    "terminal" => {
                        let assignment =
                            awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                                environment_creation_not_attempted: false,
                                session_id: SESSION.into(),
                                projection: incoming,
                                authority:
                                    awaken_session_realization::test_support::test_lease_receipt(
                                        a.lease.clone(),
                                    ),
                            };
                        installed
                            .install_terminal_cleanup_projection(physical, &assignment, proof)
                            .await
                    }
                    _ => {
                        installed
                            .install_frozen_session_projection(
                                physical,
                                SESSION,
                                incoming,
                                None,
                                variant != "validate",
                                Some(proof),
                            )
                            .await
                    }
                }
            })
            .await
            .expect("F1/F2 must refuse before any blocked File read");
            let error = result.expect_err("foreign physical registration is never admitted");
            assert_eq!(
                error.kind,
                crate::SessionExecutionErrorKind::Unavailable,
                "{variant}"
            );
            assert_eq!(
                error.code,
                if foreign_present {
                    "session_realization_generation_changed"
                } else {
                    "session_realization_absent"
                },
                "{variant}"
            );
            assert!(
                installed
                    .read(SESSION, |s| {
                        Arc::ptr_eq(&a_inputs, &s.inputs)
                            && s.realization
                                .execution()
                                .frozen_receipt
                                .as_ref()
                                .is_some_and(|r| r.revision == baseline.revision)
                            && s.realization.execution().authority.lease().is_none()
                            && s.realization.staging().preparation.is_none()
                            && s.realization
                                .staging()
                                .transition
                                .as_ref()
                                .is_some_and(|tr| tr.desired().revision == 0)
                            && s.run.context.is_none()
                    })
                    .unwrap()
                    .unwrap(),
                "{variant}/E2 canonical state unchanged"
            );
            assert!(
                match (
                    &b_inputs,
                    foreign
                        .read(SESSION, |s| (
                            s.inputs.clone(),
                            s.realization.execution().authority.lease().is_none(),
                            s.realization.staging().preparation.is_none()
                        ))
                        .unwrap()
                ) {
                    (None, None) => true,
                    (Some(before), Some((after, no_lease, no_preparation))) =>
                        Arc::ptr_eq(before, &after) && no_lease && no_preparation,
                    _ => false,
                },
                "{variant}/E2/E3 foreign source unchanged and no observation install"
            );
            for source in [&a.source, &b.source] {
                assert!(
                    futures_util::poll!(Box::pin(source.entered.notified())).is_pending(),
                    "{variant}/E3 no File I/O"
                );
            }
        }
    }
}

impl StagingFixture {
    async fn new() -> Self {
        Self::with_authority(None).await
    }

    async fn with_authority(
        authority: Option<Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>>,
    ) -> Self {
        let source = Arc::new(GatedFileSource::default());
        let control = Arc::new(RemoteTerminalCleanupControl::default());
        let mut host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_file_content_source(source.clone())
            .with_session_realization_control(control.clone())
            .with_authority(
                crate::UpstreamAuthority::default().with_terminal_cleanup_control(control.clone()),
            );
        if let Some(authority) = authority {
            host = host.with_execution_authority(authority);
        }
        let host = (host).build().expect("validated Host dependencies");
        let _managed = host.clone();
        let mut projection = remote_terminal_cleanup_projection();
        projection.resource_revision = 1;
        projection.resources = effective_resources(vec![TestInput {
            kind: "file".into(),
            id: "file-staging".into(),
            mount_path: "/mnt/input.txt".into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
            initial_branch: None,
            initial_commit: None,
        }]);
        let now = awaken_session_realization::test_support::wall_clock_now_ms();
        let lease = crate::session_resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 2,
            expires_at_unix_ms: now + 5_000,
        };
        control
            .renewals
            .lock()
            .unwrap()
            .insert(SESSION.into(), lease.clone());
        Self {
            host,
            control,
            source,
            projection,
            lease,
            now,
        }
    }

    async fn start(&self) -> tokio::task::JoinHandle<Result<(), crate::SessionExecutionError>> {
        self.start_for(SESSION).await
    }

    async fn start_for(
        &self,
        session: &str,
    ) -> tokio::task::JoinHandle<Result<(), crate::SessionExecutionError>> {
        let session = session.to_owned();
        let host = self.host.clone();
        let projection = self.projection.clone();
        let lease = self.lease.clone();
        let task = tokio::spawn(async move {
            host.run_assembly
                .installed_sessions
                .install(&session)
                .expect("installed Sessions");
            let lock = host
                .run_assembly
                .installed_sessions
                .realizations()
                .realization_lock(&session)
                .expect("installed Sessions");
            let _guard = lock.lock().await;
            host.run_assembly
                .installed_sessions
                .install_frozen_session_projection(
                    (
                        &host.run_assembly.realization_executor,
                        &host.run_assembly.mcp,
                    ),
                    &session,
                    projection,
                    None,
                    true,
                    Some(awaken_session_realization::test_support::test_observed_lease(lease)),
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.source.entered.notified(),
        )
        .await
        .expect("resource read enters while its realization driver is held");
        task
    }

    // R3/R5: blocked, failed, cancelled or obsolete compilation leaves every
    // correlated Frozen/Resource/Memory/Skill/key field unpublished. The
    // existing S1-S13 cases exercise the actual IO and lease causes; this
    // assertion observes the one installed Session, never a parallel cache.
    fn assert_unpublished(&self) {
        assert!(
            self.host
                .run_assembly
                .installed_sessions
                .read(SESSION, |installed| {
                    installed.inputs.baseline.is_none()
                        && installed.realization.execution().frozen_receipt.is_none()
                        && installed.run.context.is_none()
                        && !installed.run.session_dispatch
                        && installed.realization.staging().transition.is_none()
                        && installed.inputs.workspace.is_none()
                        && installed.realization.staging().resources.mounts.is_empty()
                        && installed.realization.staging().resources.prompts.is_empty()
                        && installed
                            .realization
                            .staging()
                            .resources
                            .resource_prompts
                            .is_empty()
                        && installed
                            .realization
                            .staging()
                            .resources
                            .binding_checks
                            .is_empty()
                        && installed
                            .realization
                            .staging()
                            .resources
                            .repositories
                            .is_empty()
                        && installed.realization.staging().memory_bindings.is_empty()
                        && installed.realization.staging().memory.is_none()
                        && installed.realization.staging().skills.is_none()
                        && installed.realization.staging().effect_key.is_none()
                })
                .expect("installed Sessions")
                .unwrap_or(true),
            "preparation does not publish an executable projection"
        );
        assert!(
            self.host
                .realization_executor
                .staging()
                .thread_resource_manifest(SESSION)
                .expect("installed Sessions")
                .is_none()
        );
    }
}

struct WorkerProofGate(std::sync::Mutex<bool>);

#[tokio::test]
async fn claimed_reassignment_stages_resources_after_same_process_retirement() {
    // R1 normal Work retirement closed a still-live predecessor generation;
    // R2 authenticated same-process reassignment uses the Session root's one
    // assignment kernel; R3 a stale retirement response arrives after staging
    // starts. E1 File preparation can publish under the successor; E2 the
    // closed predecessor never reopens; E3 old revocation cannot cancel new I/O.
    // The existing ordinary-replay and forced-close tests own their negative
    // cases; this crosses the previously omitted retirement/continuation edge.
    let mut f = StagingFixture::new().await;
    let old = f.lease.clone();
    f.host
        .run_assembly
        .install_session_realization_lease(SESSION, old.clone())
        .unwrap();
    f.host
        .run_assembly
        .installed_sessions
        .realizations()
        .update_realization(SESSION, |r| {
            assert!(r.close_authority(Some(&old)), "R1");
        })
        .expect("installed Sessions");
    assert!(
        f.host
            .run_assembly
            .install_session_realization_lease(SESSION, old.clone())
            .is_err(),
        "E2"
    );
    let epoch = match awaken_session_realization_contract::session_realization_assignment(
        Some(old.epoch),
        true,
        true,
        true,
        true,
    ) {
        awaken_session_realization_contract::SessionRealizationAssignment::Assign { epoch } => {
            epoch
        }
        other => panic!("R2 requires a successor, got {other:?}"),
    };
    f.lease.epoch = epoch;
    let task = f.start().await;
    let lock = f
        .host
        .run_assembly
        .installed_sessions
        .realizations()
        .realization_lock(SESSION)
        .expect("installed Sessions");
    assert!(
        !f.host
            .run_assembly
            .revoke_session_realization(SESSION, Some((&old, &lock)))
            .await
            .unwrap(),
        "R3/E3"
    );
    assert!(!task.is_finished(), "R3/E3 new File read stays live");
    f.source.release.notify_one();
    task.await
        .unwrap()
        .expect("E1 successor publishes Resource projection");
    assert_eq!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")[0]
            .1
            .lease
            .epoch,
        epoch,
        "E1"
    );
    // This installer stages the desired transition; only its later physical
    // activation publishes the active manifest. Do not confuse these phases.
    assert_eq!(
        f.host
            .run_assembly
            .installed_sessions
            .read(SESSION, |installed| {
                assert!(
                    installed.inputs.baseline.is_some()
                        && installed.realization.execution().frozen_receipt.is_some(),
                    "E1"
                );
                installed
                    .realization
                    .staging()
                    .transition
                    .as_ref()
                    .map(|transition| transition.desired().revision)
            })
            .expect("installed Sessions")
            .flatten(),
        Some(f.projection.resource_revision),
        "E1 staged desired revision"
    );
}

#[tokio::test]
async fn local_runtime_renewal_port_selects_exact_owner_without_requiring_mcp_or_http_control() {
    // Local supervision decision table, through the actual SessionRuns port:
    // L1 exact owner/incarnation + due, no MCP -> renew; L2 same with MCP ->
    // renew; L3 foreign owner, L4 foreign incarnation, L5 closed, L6 not due ->
    // no Control call and no mutation. No HTTP client is installed on Host:
    // the borrowed application Control must reach the one existing scheduler.
    // Resource/phase projections are unchanged: this port owns lease-only work.
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let now = awaken_session_realization::test_support::wall_clock_now_ms();
    let mut initial = Vec::new();
    for rule in ["L1", "L2", "L3", "L4", "L5", "L6"] {
        let lease = crate::session_resource::SessionRealizationLease {
            owner: if rule == "L3" { "foreign" } else { "local" }.into(),
            runtime_incarnation: if rule == "L4" { "foreign" } else { "boot" }.into(),
            epoch: 1,
            expires_at_unix_ms: now + if rule == "L6" { 90_000 } else { 5_000 },
        };
        control
            .renewals
            .lock()
            .unwrap()
            .insert(rule.into(), lease.clone());
        host.run_assembly
            .install_session_realization_lease(rule, lease.clone())
            .unwrap();
        host.run_assembly
            .installed_sessions
            .realizations()
            .update_realization(rule, |r| {
                r.staging_mut().mcp.set_has_frozen_attachment(rule == "L2");
                if rule == "L5" {
                    assert!(r.close_authority(Some(&lease)));
                }
            })
            .expect("installed Sessions");
        initial.push((rule, lease));
    }
    assert!(host.capabilities.worker.session_control.is_none());
    let runtime = host.clone();
    let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000);
    assert_eq!(
        awaken_session_execution_contract::RealizationRuntime::renew_session_realization_projections(
            runtime.run_assembly.as_ref(),
            control.as_ref(),
            "local",
            "boot",
            timing,
        )
        .await
        .unwrap(),
        2
    );
    let mut called = control.renewal_sessions.lock().unwrap().clone();
    called.sort();
    assert_eq!(called, ["L1", "L2"]);
    for (rule, mut lease) in initial {
        if matches!(rule, "L1" | "L2") {
            lease.expires_at_unix_ms += 10_000;
        }
        host.run_assembly
            .installed_sessions
            .read(rule, |installed| {
                assert_eq!(
                    installed.realization.execution().authority.lease(),
                    (rule != "L5").then_some(&lease),
                    "{rule}"
                );
                assert_eq!(
                    installed.realization.staging().mcp.has_frozen_attachment(),
                    rule == "L2"
                );
                assert!(
                    installed.inputs.baseline.is_none()
                        && installed.realization.staging().transition.is_none()
                        && !installed.run.session_dispatch,
                    "{rule}/lease only"
                );
            })
            .expect("installed Sessions")
            .unwrap();
    }
}

#[tokio::test]
async fn missing_control_is_required_only_for_due_leases_without_skipping_refresh_lane() {
    // D1 no lease + no Control -> no renewal; D2 live/not-due + no Control ->
    // no renewal; D3 expired/due + no Control -> explicit error, retain the
    // exact root for cleanup. Physical refresh is independently exercised by
    // the real scoped-candidate test below, including a Closed/empty cohort.
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000);
    assert_eq!(
        host.run_assembly
            .renew_due_session_realizations(timing)
            .await
            .unwrap(),
        0,
        "D1"
    );
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "boot".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms() + 90_000,
    };
    host.run_assembly
        .install_session_realization_lease(SESSION, lease.clone())
        .unwrap();
    assert_eq!(
        host.run_assembly
            .renew_due_session_realizations(timing)
            .await
            .unwrap(),
        0,
        "D2"
    );
    host.run_assembly
        .installed_sessions
        .realizations()
        .update_realization(SESSION, |r| {
            r.execution_mut().authority =
                crate::realization_state::SessionRealizationAuthority::Live(
                    awaken_session_realization::test_support::test_expired_observed_lease(
                        lease.clone(),
                    ),
                );
        })
        .expect("installed Sessions");
    assert!(
        host.run_assembly
            .renew_due_session_realizations(timing)
            .await
            .is_err(),
        "D3"
    );
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization(SESSION, |r| r.execution().authority.lease().cloned())
            .expect("installed Sessions"),
        Some(Some(lease)),
        "D3 retained cleanup identity"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "required real Linux helper supplied by the portability supervision gate"]
async fn trusted_host_scoped_session_candidate_binds_and_refreshes_without_due_lease() {
    use awaken_provisioning_contract::Sandbox as _;
    // S22-S28 end-to-end: typed deployment -> actual probe/install -> unbound candidate (both spawn
    // seams denied) -> existing publication owner binds -> real tree starts ->
    // idempotent preparation retry -> existing sweep with NO due Session lease
    // refreshes (no installed Control) -> Worker close causes refresh and launch denial -> exact tree
    // stop. A real workload File survives denial/stop. No vendor/cloud claim.
    // S29 the helper is Cargo's exact artifact, independent of the test binary's
    // intermediate directory; its startup validation and probe are the
    // execution composition's startup rows.
    let base = tempfile::tempdir().unwrap();
    let gate = Arc::new(WorkerProofGate(std::sync::Mutex::new(true)));
    let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
    deployment.sandbox_dir = Some(base.path().into());
    deployment.sandbox.trusted_host_scoped_execution = true;
    // The TrustedHost backend exactly as startup selection configures it:
    // the installed Hand and scoped execution discovered from the helper.
    let helper = crate::acp_backend::deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER");
    let mut trusted = awaken_session_environment::SandboxEnvironmentProvider::
        trusted_host_workdir_with_agent_stderr(base.path().join("trusted-host"), false);
    trusted.install_workdir_hand(
        Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
        helper.to_string_lossy().into_owned(),
        std::time::Duration::ZERO,
    );
    trusted
        .install_scoped_execution(
            awaken_sandbox_local::LocalExecutionDeployment::discover(helper)
                .expect("discover the scoped helper"),
        )
        .await
        .expect("install scoped execution");
    let host = (crate::SessionExecutorBuilder::ephemeral_with_deployment(
        Arc::new(OkModel),
        "stub",
        deployment,
    )
    .await
    .with_backend_owned_environment_provider(trusted.into_provider_roles())
    .with_execution_authority(gate.clone()))
    .build()
    .expect("validated Host dependencies");
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "runtime".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    host.run_assembly
        .install_session_realization_lease(SESSION, lease.clone())
        .unwrap();
    let provider = host
        .realization_executor
        .environments()
        .providers()
        .backend_owned_session_provider
        .as_ref()
        .unwrap();
    let mut spec = awaken_session_environment::test_support::spec();
    spec.scope = SESSION.into();
    let fence = lease.sandbox_effect_fence("scoped-candidate").unwrap();
    let environment = provider
        .realizer()
        .create_effective_for_effect(
            &spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &fence,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &fence,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .unwrap();
    let environment = Arc::new(environment);
    let awaken_session_environment::SandboxEnvironment::Workdir { sandbox, .. } =
        environment.sandbox_environment()
    else {
        unreachable!()
    };
    let command = || {
        let mut command = awaken_provisioning_contract::Command::new([
            "/bin/sh",
            "-c",
            "printf exact > ready; setsid /bin/sleep 60 & wait",
        ]);
        command.cwd = "/workspace".into();
        command.stdio = awaken_provisioning_contract::Stdio::Null;
        command
    };
    assert!(sandbox.spawn(command()).await.is_err());
    assert!(sandbox.spawn_agent(command()).await.is_err());
    let first = host
        .realization_executor
        .environments()
        .begin_session_environment_preparation(
            host.realization_executor.peers(),
            SESSION,
            environment.clone(),
            None,
        )
        .unwrap();
    assert!(!sandbox.needs_execution_authority());
    let retry = host
        .realization_executor
        .environments()
        .begin_session_environment_preparation(
            host.realization_executor.peers(),
            SESSION,
            environment.clone(),
            None,
        )
        .unwrap();
    assert!(Arc::ptr_eq(&first.environment, &retry.environment));
    let process = sandbox.spawn(command()).await.unwrap();
    let ready = sandbox.workspace_path().join("workspace/ready");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while std::fs::read(&ready).ok().as_deref() != Some(b"exact") {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000);
    assert_eq!(
        host.run_assembly
            .renew_due_session_realizations(timing)
            .await
            .unwrap(),
        0
    );
    assert!(process.poll().await.unwrap().is_none());
    *gate.0.lock().unwrap() = false;
    assert!(
        host.run_assembly
            .renew_due_session_realizations(timing)
            .await
            .is_err(),
        "no-due early return must not bypass physical refresh"
    );
    assert!(host.capabilities.worker.session_control.is_none());
    host.run_assembly
        .installed_sessions
        .realizations()
        .update_realization(SESSION, |r| {
            assert!(r.close_authority(Some(&lease)));
        })
        .expect("installed Sessions");
    assert!(
        host.run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")
            .is_empty()
    );
    assert!(
        host.run_assembly
            .renew_due_session_realizations(timing)
            .await
            .is_err(),
        "no Control and an empty live-lease cohort must still refresh the retained tree"
    );
    assert!(sandbox.spawn(command()).await.is_err());
    environment
        .lifecycle()
        .stop_bound_processes()
        .await
        .unwrap();
    assert!(process.poll().await.unwrap().is_some());
    assert_eq!(std::fs::read(ready).unwrap(), b"exact");
}

impl awaken_provisioning_contract::CurrentExecutionAuthority for WorkerProofGate {
    fn with_current(
        &self,
        effect: &mut dyn FnMut(std::time::Duration) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let open = self.0.lock().unwrap();
        if !*open {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        effect(std::time::Duration::from_secs(30))
    }
}

#[tokio::test]
async fn worker_root_denies_fresh_and_replaced_projection_before_resource_io() {
    // Receiver migration keeps the original Worker -> installed Session callback boundary:
    // closed Worker proof denies both fresh/replaced publication before IO,
    // while a direct local Host keeps its existing absence-of-registry meaning.
    // No closure or error rule moves to another authority during consolidation.

    // Cause/effect table: root closed; absent/existing installed Session; File/no Resource.
    // | A1 | absent | File | deny preparation; no read or installed lease |
    // | A2 | absent | none | deny complete publication |
    // | A3 | existing old generation | File | cannot install replacement |
    // | A4 | existing old generation | none | cannot publish replacement |
    // Every case invokes the production complete installer, not just the test
    // lease helper. Control's late valid Session claim cannot reopen this Worker.
    for existing in [false, true] {
        for resource in [false, true] {
            let gate = Arc::new(WorkerProofGate(std::sync::Mutex::new(true)));
            let mut f = StagingFixture::with_authority(Some(gate.clone())).await;
            if existing {
                f.host
                    .run_assembly
                    .install_session_realization_lease(SESSION, f.lease.clone())
                    .unwrap();
                f.lease.epoch += 1;
            }
            if !resource {
                f.projection = remote_terminal_cleanup_projection();
            }
            *gate.0.lock().unwrap() = false;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                f.host
                    .run_assembly
                    .installed_sessions
                    .install_frozen_session_projection(
                        (
                            &f.host.run_assembly.realization_executor,
                            &f.host.run_assembly.mcp,
                        ),
                        SESSION,
                        f.projection.clone(),
                        None,
                        true,
                        Some(
                            awaken_session_realization::test_support::test_observed_lease(
                                f.lease.clone(),
                            ),
                        ),
                    ),
            )
            .await;
            assert!(
                result.unwrap().is_err(),
                "A1-A4 denied before gated File read"
            );
            f.assert_unpublished();
            assert_eq!(
                f.host
                    .run_assembly
                    .installed_sessions
                    .realization_leases()
                    .expect("installed Sessions")
                    .len(),
                usize::from(existing)
            );
            if existing {
                assert_eq!(
                    f.host
                        .run_assembly
                        .installed_sessions
                        .realization_leases()
                        .expect("installed Sessions")[0]
                        .1
                        .lease
                        .epoch,
                    f.lease.epoch - 1
                );
            }
        }
    }
}

#[tokio::test]
async fn worker_root_close_during_preparation_prevents_completion_and_reentry() {
    // A5 admission wins first: the SAME preparation appears in cohort selection.
    // Root closes -> cohort closes/cancels -> File finishes late. Effects: no
    // executable projection, no lease reentry; cleanup remains separately owned.
    let gate = Arc::new(WorkerProofGate(std::sync::Mutex::new(true)));
    let f = StagingFixture::with_authority(Some(gate.clone())).await;
    let task = f.start().await;
    *gate.0.lock().unwrap() = false;
    let revocation = f
        .host
        .run_assembly
        .begin_session_realization_revocation()
        .expect("installed Sessions");
    f.source.release.notify_one();
    assert!(task.await.unwrap().is_err());
    f.assert_unpublished();
    assert!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")
            .is_empty()
    );
    let mut replacement = f.lease.clone();
    replacement.epoch += 1;
    assert!(
        f.host
            .run_assembly
            .install_session_realization_lease(SESSION, replacement)
            .is_err()
    );
    f.host
        .run_assembly
        .finish_session_realization_revocation(revocation)
        .await
        .unwrap();
}

#[tokio::test]
async fn worker_root_denies_renewal_even_before_cohort_close() {
    // A6 root proof is closed while the installed Session still has a live lease. A successful
    // Control response is not sufficient to apply renewal; the canonical due
    // scheduler rejects and revokes only this projection. No global close here,
    // so this test detects a scheduler that consults ONLY the Session lease.
    let gate = Arc::new(WorkerProofGate(std::sync::Mutex::new(true)));
    let f = StagingFixture::with_authority(Some(gate.clone())).await;
    f.host
        .run_assembly
        .install_session_realization_lease(SESSION, f.lease.clone())
        .unwrap();
    *gate.0.lock().unwrap() = false;
    // A7 the public SessionRuns renewal delivery entrance has the same root
    // guard. Unlike the scheduler it reports denial without owning revocation.
    let managed = f.host.clone();
    let mut renewed = f.lease.clone();
    renewed.expires_at_unix_ms += 10_000;
    assert!(
        managed
            .run_assembly
            .renew_session_realization_lease(
                SESSION,
                awaken_session_realization::test_support::test_observed_lease(renewed)
            )
            .await
            .is_err()
    );
    assert_eq!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")[0]
            .1
            .lease,
        f.lease,
        "A7 no mutation"
    );
    assert_eq!(
        f.host
            .run_assembly
            .renew_due_session_realizations(
                awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000)
            )
            .await
            .unwrap(),
        0
    );
    assert!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")
            .is_empty()
    );
}

// Cause/effect graph: C1 a cold exact lease is assigned; C2 immutable File I/O
// is blocked before any complete projection; C3 renewal is due; C4 it succeeds;
// C5 I/O succeeds. E1 the existing renewal scheduler sees that lease during C2;
// E2 baseline/runtime/manifest remain unpublished; E3 completion preserves the
// renewed expiry instead of reinstalling its shorter original assertion.
// Decision S1=C1+C2+C3+C4=>E1/E2, then C5=>E3. This exercises the production
// complete installer and scheduler, not a manually preinstalled lease fixture.
#[tokio::test]
async fn cold_resource_staging_is_renewable_before_projection_publication() {
    let f = StagingFixture::new().await;
    let task = f.start().await;
    f.assert_unpublished();
    let renewed = f
        .host
        .run_assembly
        .renew_due_session_realizations(
            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000),
        )
        .await
        .unwrap();
    // Always release/await the in-flight read before the assertion, including
    // RED, so a failed test never leaves an unobserved task behind.
    f.source.release.notify_one();
    task.await.unwrap().unwrap();
    assert_eq!(
        renewed, 1,
        "S1 the new lease must be visible before File I/O finishes"
    );
    assert_eq!(
        f.control.renewal_sessions.lock().unwrap().as_slice(),
        [SESSION]
    );
    assert_eq!(
        f.host
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(SESSION, |r| r.execution().authority.lease().cloned())
            .expect("installed Sessions")
            .flatten()
            .unwrap()
            .expires_at_unix_ms,
        f.lease.expires_at_unix_ms + 10_000,
        "S1 renewed expiry survives publication"
    );
}

// S2/S3: C1 File failure / caller cancellation (exclusive); C2 the cold lease
// was introduced here. Effects: no executable or staged projection, no orphan
// renewable lease, no held realization lock. Abort is awaited, not detached.
#[tokio::test]
async fn failed_or_cancelled_resource_preparation_drops_only_its_unpublished_lease() {
    for cancel in [false, true] {
        let f = StagingFixture::new().await;
        let task = f.start().await;
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            f.source.fail.store(true, Ordering::SeqCst);
            f.source.release.notify_one();
            assert!(task.await.unwrap().is_err());
        }
        f.assert_unpublished();
        assert!(
            f.host
                .run_assembly
                .installed_sessions
                .read(SESSION, |installed| {
                    installed
                        .realization
                        .execution()
                        .authority
                        .lease()
                        .is_none()
                        && installed.realization.staging().preparation.is_none()
                        && installed.inputs.workspace.is_none()
                        && installed.realization.staging().skills.is_none()
                        && installed.realization.staging().effect_key.is_none()
                })
                .expect("installed Sessions")
                .unwrap()
        );
        let lock = f
            .host
            .run_assembly
            .installed_sessions
            .realizations()
            .realization_lock(SESSION)
            .expect("installed Sessions");
        assert!(lock.try_lock().is_ok());
    }
}

// S4: explicit ownership loss while a read holds the realization lock must
// cancel that read BEFORE fenced revocation waits for its lock. S5: retain a
// pre-existing lease until normal revocation can clean its resident projection.
// Neither case may publish late File bytes or require releasing the File gate.
#[tokio::test]
async fn renewal_rejection_cancels_resource_io_before_waiting_for_realization_lock() {
    for resident_lease in [false, true] {
        let f = StagingFixture::new().await;
        if resident_lease {
            f.host
                .run_assembly
                .install_session_realization_lease(SESSION, f.lease.clone())
                .unwrap();
        }
        let task = f.start().await;
        *f.control.renewal_failure.lock().unwrap() = Some(
            awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership,
        );
        let renewal = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            f.host.run_assembly.renew_due_session_realizations(
                awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000),
            ),
        )
        .await;
        if renewal.is_err() {
            task.abort();
        }
        let result = task.await;
        assert_eq!(renewal.expect("S4/S5 no lock cycle").unwrap(), 0);
        assert!(result.unwrap().is_err());
        f.assert_unpublished();
        assert!(
            f.host
                .run_assembly
                .installed_sessions
                .realization_leases()
                .expect("installed Sessions")
                .is_empty()
        );
    }
}

// S6: temporary Control failure + still-live proof => keep reading and retry
// through the SAME scheduler; success then preserves its extended expiry.
#[tokio::test]
async fn temporary_renewal_failure_does_not_abort_live_resource_preparation() {
    let f = StagingFixture::new().await;
    let task = f.start().await;
    *f.control.renewal_failure.lock().unwrap() = Some(
        awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
            "injected outage".into(),
        ),
    );
    let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000);
    assert!(
        f.host
            .run_assembly
            .renew_due_session_realizations(timing)
            .await
            .is_err()
    );
    assert!(!task.is_finished());
    f.assert_unpublished();
    *f.control.renewal_failure.lock().unwrap() = None;
    assert_eq!(
        f.host
            .run_assembly
            .renew_due_session_realizations(timing)
            .await
            .unwrap(),
        1
    );
    f.source.release.notify_one();
    task.await.unwrap().unwrap();
    assert_eq!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")[0]
            .1
            .lease
            .expires_at_unix_ms,
        f.lease.expires_at_unix_ms + 10_000
    );
}

// S7: no scheduler progress until expiry => local read lifetime still ends.
// This deliberately uses a short real deadline: the production authority clock
// is wall time, so Tokio's paused clock alone would not model expiry correctly.
#[tokio::test]
async fn resource_preparation_expires_even_without_another_renewal_sweep() {
    let mut f = StagingFixture::new().await;
    f.lease.expires_at_unix_ms = f.now + 150;
    let mut task = f.start().await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), &mut task).await;
    if result.is_err() {
        task.abort();
        let _ = task.await;
    }
    assert!(
        result
            .expect("S7 expiry cancels blocked read")
            .unwrap()
            .is_err()
    );
    f.assert_unpublished();
    assert!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")
            .is_empty()
    );
}

// S8/S9/S10: generation replacement / exact invalidation / installed Session removal while
// File I/O is blocked => cancel without a late publication. Dropping the old
// preparation must neither erase the replacement nor recreate a removed installed Session.
#[tokio::test]
async fn obsolete_resource_preparation_cannot_publish_or_erase_a_successor() {
    for mutation in ["replace", "invalidate", "remove"] {
        let f = StagingFixture::new().await;
        let mut task = f.start().await;
        let mut successor = f.lease.clone();
        successor.epoch += 1;
        match mutation {
            "replace" => f
                .host
                .run_assembly
                .install_session_realization_lease(SESSION, successor.clone())
                .unwrap(),
            "invalidate" => f
                .host
                .run_assembly
                .invalidate_session_realization_lease(SESSION, &f.lease)
                .expect("healthy registry"),
            _ => {
                f.host.run_assembly.installed_sessions.remove(SESSION);
            }
        }
        // Make success and cancellation ready together: cancellation/fencing
        // must win; this also guards the post-I/O publication boundary.
        f.source.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut task).await;
        if result.is_err() {
            task.abort();
            let _ = task.await;
        }
        assert!(
            result
                .expect("obsolete preparation exits")
                .unwrap()
                .is_err()
        );
        f.assert_unpublished();
        let current = f
            .host
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(SESSION, |r| r.execution().authority.lease().cloned())
            .expect("installed Sessions")
            .flatten();
        assert_eq!(current, (mutation == "replace").then_some(successor));
        if mutation == "remove" {
            assert!(!f.host.run_assembly.installed_sessions.contains(SESSION));
            f.host
                .run_assembly
                .invalidate_session_realization_lease(SESSION, &f.lease)
                .expect("healthy registry");
            assert!(
                !f.host.run_assembly.installed_sessions.contains(SESSION),
                "invalidation cannot resurrect a removed installed Session"
            );
        }
    }
}

// S11: malformed preflight never reaches Resource I/O or introduces a lease.
// S12: an existing longer same-generation lease survives both failed staging
// and installation of a shorter snapshot (renewal response races preparation).
#[tokio::test]
async fn preflight_failure_has_no_lease_and_failed_replay_preserves_existing_renewal() {
    let mut f = StagingFixture::new().await;
    f.projection.baseline.fingerprint.0.clear();
    assert!(
        f.host
            .run_assembly
            .installed_sessions
            .install_frozen_session_projection(
                (
                    &f.host.run_assembly.realization_executor,
                    &f.host.run_assembly.mcp
                ),
                SESSION,
                f.projection.clone(),
                None,
                true,
                Some(
                    awaken_session_realization::test_support::test_observed_lease(f.lease.clone())
                )
            )
            .await
            .is_err()
    );
    assert!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")
            .is_empty()
    );
    let f = StagingFixture::new().await;
    let mut renewed = f.lease.clone();
    renewed.expires_at_unix_ms += 30_000;
    f.host
        .run_assembly
        .install_session_realization_lease(SESSION, renewed.clone())
        .unwrap();
    let task = f.start().await;
    assert_eq!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")[0]
            .1
            .lease,
        renewed
    );
    f.source.fail.store(true, Ordering::SeqCst);
    f.source.release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert_eq!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")[0]
            .1
            .lease,
        renewed
    );
}

// S13: 16 distinct Sessions blocked in real Resource staging remain visible to
// the one existing bounded scheduler. Renewal never publishes any baseline.
// Existing 64/512-Session tests own scheduler capacity/sort/no-due rules; this
// case adds actual cold I/O rather than duplicating those synthetic lease tests.
#[tokio::test]
async fn many_cold_resource_reads_share_the_existing_renewal_scheduler() {
    let f = StagingFixture::new().await;
    let mut tasks = Vec::new();
    for index in 0..16 {
        let session = format!("cold-{index}");
        f.control
            .renewals
            .lock()
            .unwrap()
            .insert(session.clone(), f.lease.clone());
        tasks.push(f.start_for(&session).await);
    }
    let renewed = f
        .host
        .run_assembly
        .renew_due_session_realizations(
            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000),
        )
        .await;
    let unpublished = (0..16).all(|index| {
        f.host
            .run_assembly
            .installed_sessions
            .read(&format!("cold-{index}"), |installed| {
                installed.inputs.baseline.is_none() && !installed.run.session_dispatch
            })
            .expect("installed Sessions")
            .unwrap()
    });
    f.source.release.notify_waiters();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(renewed.unwrap(), 16);
    assert!(unpublished);
    assert_eq!(f.control.renewal_sessions.lock().unwrap().len(), 16);
}

// S14: Worker-wide revocation uses the same cancellation-before-lock edge as
// one rejected renewal. A cold read must not pin worker shutdown indefinitely.
#[tokio::test]
async fn worker_revocation_cancels_blocked_resource_preparation() {
    let f = StagingFixture::new().await;
    let task = f.start().await;
    let revoked = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        f.host.run_assembly.revoke_all_session_realizations(),
    )
    .await;
    if revoked.is_err() {
        task.abort();
    }
    let result = task.await;
    revoked
        .expect("S14 revocation does not wait for unbounded File I/O")
        .unwrap();
    assert!(result.unwrap().is_err());
    f.assert_unpublished();
    assert!(
        f.host
            .run_assembly
            .installed_sessions
            .realization_leases()
            .expect("installed Sessions")
            .is_empty()
    );
}

// S15: crossing the initial deadline after a successful renewal is not lease
// loss. The preparation watchdog follows notifications from the current installed Session,
// not a frozen timeout copied from the original Control response.
#[tokio::test]
async fn renewed_preparation_survives_its_original_deadline() {
    let mut f = StagingFixture::new().await;
    f.lease.expires_at_unix_ms = f.now + 400;
    f.control
        .renewals
        .lock()
        .unwrap()
        .insert(SESSION.into(), f.lease.clone());
    let task = f.start().await;
    f.host
        .run_assembly
        .renew_due_session_realizations(
            awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let finished_early = task.is_finished();
    f.source.release.notify_one();
    task.await.unwrap().unwrap();
    assert!(
        !finished_early,
        "S15 still owned after its original deadline"
    );
}

#[tokio::test]
async fn cold_staging_discards_a_renewal_response_delivered_after_newer_authority() {
    // R16 old renewal commits at Control but its response stalls during File
    // I/O. R17 a second renewal returns first => preserve its longer expiry;
    // R18 takeover installs a different generation => cancel old staging.
    // Deliver the original successful response last, then release File bytes.
    // The canonical installed Session must neither regress nor publish the obsolete input.
    for takeover in [false, true] {
        let f = StagingFixture::new().await;
        let preparation = f.start().await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *f.control.renewal_response_gate.lock().unwrap() = Some(gate.clone());
        let host = f.host.clone();
        let renewal = tokio::spawn(async move {
            host.run_assembly
                .renew_due_session_realizations(
                    awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(30_000),
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            f.control.renewal_response_ready.notified(),
        )
        .await
        .unwrap();
        *f.control.renewal_response_gate.lock().unwrap() = None;
        let mut expected = f.lease.clone();
        expected.expires_at_unix_ms = f.now + 60_000;
        if takeover {
            expected.epoch += 1;
            expected.runtime_incarnation = "worker-a/boot-2".into();
            f.control
                .renewals
                .lock()
                .unwrap()
                .insert(SESSION.into(), expected.clone());
            f.host
                .run_assembly
                .install_session_realization_lease(SESSION, expected.clone())
                .unwrap();
        } else {
            // R17 extends the asserted Control expiry, not Worker wall time.
            // The stalled first response has not installed its newer expiry.
            expected.expires_at_unix_ms = f.lease.expires_at_unix_ms
                + awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(60_000)
                    .renew_interval()
                    .as_millis() as u64;
            assert_eq!(
                f.host
                    .run_assembly
                    .renew_due_session_realizations(
                        awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(60_000)
                    )
                    .await
                    .unwrap(),
                1
            );
        }
        gate.add_permits(1);
        assert_eq!(
            renewal.await.unwrap().unwrap(),
            0,
            "R16 discard obsolete reply"
        );
        f.source.release.notify_one();
        assert_eq!(preparation.await.unwrap().is_err(), takeover);
        assert_eq!(
            f.host
                .run_assembly
                .installed_sessions
                .realization_leases()
                .expect("installed Sessions")[0]
                .1
                .lease,
            expected
        );
        if takeover {
            f.assert_unpublished();
        } else {
            assert!(
                f.host
                    .run_assembly
                    .installed_sessions
                    .read(SESSION, |installed| installed.inputs.baseline.is_some()
                        && installed.run.session_dispatch
                        && installed.realization.staging().preparation.is_none())
                    .expect("installed Sessions")
                    .unwrap()
            );
        }
    }
}

#[tokio::test]
async fn cold_staging_renews_for_ten_ttls_with_mixed_control_latency() {
    // L1 16 blocked resource reads + mixed 0/10/25ms Control responses over
    // >=10 real TTLs => every generation remains live, unpublished and renewable.
    // L2 exceed the request budget for one session => healthy sessions continue;
    // that session expires and cancels its blocked read without a late publish.
    // Existing scheduler tests own capacity/order; this adds repeated complete
    // staging lifetimes and subsequent overload to that same scheduler.
    let mut f = StagingFixture::new().await;
    let ttl = 600;
    f.lease.expires_at_unix_ms = f.now + ttl;
    let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(ttl);
    // Each independent Session receives its initial Control assertion when it
    // starts. Renewal runs during setup as it does in the real supervisor;
    // constructing sixteen blocked reads must not consume the first lease
    // before the test starts its renewal driver.
    let stop_renewal = Arc::new(tokio::sync::Notify::new());
    let driver_stop = stop_renewal.clone();
    let host = f.host.clone();
    let renewal = tokio::spawn(async move {
        loop {
            host.run_assembly
                .renew_due_session_realizations(timing)
                .await
                .unwrap();
            tokio::select! {
                _ = driver_stop.notified() => break,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }
    });
    let mut tasks = Vec::new();
    for index in 0..16 {
        let session = format!("soak-{index:02}");
        f.lease.expires_at_unix_ms =
            awaken_session_realization::test_support::wall_clock_now_ms() + ttl;
        f.control
            .renewals
            .lock()
            .unwrap()
            .insert(session.clone(), f.lease.clone());
        f.control
            .renewal_delay_ms
            .lock()
            .unwrap()
            .insert(session.clone(), [0, 10, 25][index % 3]);
        tasks.push(f.start_for(&session).await);
    }
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_millis(ttl * 10) {
        for (index, task) in tasks.iter().enumerate() {
            assert!(!task.is_finished(), "L1 session {index}");
            assert!(
                f.host
                    .run_assembly
                    .installed_sessions
                    .read(&format!("soak-{index:02}"), |installed| installed
                        .inputs
                        .baseline
                        .is_none()
                        && installed.realization.staging().preparation.is_some()
                        && installed
                            .realization
                            .execution()
                            .authority
                            .remaining()
                            .is_some())
                    .expect("installed Sessions")
                    .unwrap()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Join the same driver before the deliberate L2 overload; no two renewal
    // loops overlap and every L1 Control failure propagates through this join.
    stop_renewal.notify_one();
    renewal.await.unwrap();
    f.control
        .renewal_delay_ms
        .lock()
        .unwrap()
        .insert("soak-00".into(), ttl * 10);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !tasks[0].is_finished() {
            let _ = f
                .host
                .run_assembly
                .renew_due_session_realizations(timing)
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("L2 overloaded session loses authority");
    assert!(
        tasks.iter().skip(1).all(|task| !task.is_finished()),
        "L2 healthy sessions survive"
    );
    f.source.release.notify_waiters();
    for (index, task) in tasks.into_iter().enumerate() {
        assert_eq!(task.await.unwrap().is_err(), index == 0);
    }
    assert_eq!(f.control.renewals_active.load(Ordering::SeqCst), 0);
    assert!(
        f.host
            .realization_executor
            .staging()
            .thread_resource_manifest("soak-00")
            .expect("installed Sessions")
            .is_none()
    );
}
