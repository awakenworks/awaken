use super::*;

#[derive(Default)]
struct GatedFileSource {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl crate::FileContentSource<awaken_dispatch::ResourceOperationFence> for GatedFileSource {
    async fn read(
        &self,
        _workspace: &str,
        file_id: &str,
        _purpose: &awaken_resource_contract::FileReadPurpose,
        _authority: Option<&awaken_dispatch::ResourceOperationFence>,
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
    host: Arc<SharedHost>,
    control: Arc<RemoteTerminalCleanupControl>,
    source: Arc<GatedFileSource>,
    projection: awaken_session_contract::FrozenSessionProjection,
    lease: awaken_session_contract::SessionRealizationLease,
    now: u64,
}

const SESSION: &str = "cold-resource-lease";

impl StagingFixture {
    fn new() -> Self {
        let source = Arc::new(GatedFileSource::default());
        let control = Arc::new(RemoteTerminalCleanupControl::default());
        let host = Arc::new(
            SharedHost::new(Arc::new(OkModel), "stub")
                .with_file_content_source(source.clone())
                .with_session_control(control.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
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
        let now = crate::terminal_repository_publication::runtime_unix_now_ms();
        let lease = awaken_session_contract::SessionRealizationLease {
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

    async fn start(&self) -> tokio::task::JoinHandle<Result<(), crate::HostError>> {
        self.start_for(SESSION).await
    }

    async fn start_for(
        &self,
        session: &str,
    ) -> tokio::task::JoinHandle<Result<(), crate::HostError>> {
        let session = session.to_owned();
        let host = self.host.clone();
        let projection = self.projection.clone();
        let lease = self.lease.clone();
        let task = tokio::spawn(async move {
            let lock = host.session_slots.realization_lock(&session);
            let _guard = lock.lock().await;
            host.install_frozen_session_projection(&session, projection, None, true, Some(lease))
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

    fn assert_unpublished(&self) {
        assert!(
            self.host
                .session_slots
                .read(SESSION, |slot| {
                    slot.baseline.is_none()
                        && slot.runtime.is_none()
                        && !slot.session_dispatch
                        && slot.resource_transition.is_none()
                })
                .unwrap_or(true),
            "preparation does not publish an executable projection"
        );
        assert!(self.host.thread_resource_manifest(SESSION).is_none());
    }
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
    let f = StagingFixture::new();
    let task = f.start().await;
    f.assert_unpublished();
    let renewed = f
        .host
        .renew_due_session_realizations(
            f.now,
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(30_000),
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
            .session_slots
            .read(SESSION, |slot| slot.realization_lease.clone())
            .flatten()
            .unwrap()
            .expires_at_unix_ms,
        f.now + 30_000,
        "S1 renewed expiry survives publication"
    );
}

// S2/S3: C1 File failure / caller cancellation (exclusive); C2 the cold lease
// was introduced here. Effects: no executable or staged projection, no orphan
// renewable lease, no held realization lock. Abort is awaited, not detached.
#[tokio::test]
async fn failed_or_cancelled_resource_preparation_drops_only_its_unpublished_lease() {
    for cancel in [false, true] {
        let f = StagingFixture::new();
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
                .session_slots
                .read(SESSION, |slot| {
                    slot.realization_lease.is_none()
                        && slot.resource_preparation.is_none()
                        && slot.workspace.is_none()
                        && slot.skills.is_none()
                        && slot.staged_resource_effect_key.is_none()
                })
                .unwrap()
        );
        let lock = f.host.session_slots.realization_lock(SESSION);
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
        let f = StagingFixture::new();
        if resident_lease {
            f.host
                .install_session_realization_lease(SESSION, f.lease.clone());
        }
        let task = f.start().await;
        *f.control.renewal_failure.lock().unwrap() =
            Some(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        let renewal = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            f.host.renew_due_session_realizations(
                f.now,
                awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(30_000),
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
        assert!(f.host.session_slots.realization_leases().is_empty());
    }
}

// S6: temporary Control failure + still-live proof => keep reading and retry
// through the SAME scheduler; success then preserves its extended expiry.
#[tokio::test]
async fn temporary_renewal_failure_does_not_abort_live_resource_preparation() {
    let f = StagingFixture::new();
    let task = f.start().await;
    *f.control.renewal_failure.lock().unwrap() = Some(
        awaken_session_contract::SessionRealizationControlFailure::Unavailable(
            "injected outage".into(),
        ),
    );
    let timing =
        awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(30_000);
    assert!(
        f.host
            .renew_due_session_realizations(f.now, timing)
            .await
            .is_err()
    );
    assert!(!task.is_finished());
    f.assert_unpublished();
    *f.control.renewal_failure.lock().unwrap() = None;
    assert_eq!(
        f.host
            .renew_due_session_realizations(f.now, timing)
            .await
            .unwrap(),
        1
    );
    f.source.release.notify_one();
    task.await.unwrap().unwrap();
    assert_eq!(
        f.host.session_slots.realization_leases()[0]
            .1
            .expires_at_unix_ms,
        f.now + 30_000
    );
}

// S7: no scheduler progress until expiry => local read lifetime still ends.
// This deliberately uses a short real deadline: the production authority clock
// is wall time, so Tokio's paused clock alone would not model expiry correctly.
#[tokio::test]
async fn resource_preparation_expires_even_without_another_renewal_sweep() {
    let mut f = StagingFixture::new();
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
    assert!(f.host.session_slots.realization_leases().is_empty());
}

// S8/S9/S10: generation replacement / exact invalidation / slot removal while
// File I/O is blocked => cancel without a late publication. Dropping the old
// preparation must neither erase the replacement nor recreate a removed slot.
#[tokio::test]
async fn obsolete_resource_preparation_cannot_publish_or_erase_a_successor() {
    for mutation in ["replace", "invalidate", "remove"] {
        let f = StagingFixture::new();
        let mut task = f.start().await;
        let mut successor = f.lease.clone();
        successor.epoch += 1;
        match mutation {
            "replace" => f
                .host
                .install_session_realization_lease(SESSION, successor.clone()),
            "invalidate" => f
                .host
                .invalidate_session_realization_lease(SESSION, &f.lease),
            _ => {
                f.host.session_slots.remove(SESSION);
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
            .session_slots
            .read(SESSION, |slot| slot.realization_lease.clone())
            .flatten();
        assert_eq!(current, (mutation == "replace").then_some(successor));
        if mutation == "remove" {
            assert!(!f.host.session_slots.contains(SESSION));
            f.host
                .invalidate_session_realization_lease(SESSION, &f.lease);
            assert!(
                !f.host.session_slots.contains(SESSION),
                "invalidation cannot resurrect a removed slot"
            );
        }
    }
}

// S11: malformed preflight never reaches Resource I/O or introduces a lease.
// S12: an existing longer same-generation lease survives both failed staging
// and installation of a shorter snapshot (renewal response races preparation).
#[tokio::test]
async fn preflight_failure_has_no_lease_and_failed_replay_preserves_existing_renewal() {
    let mut f = StagingFixture::new();
    f.projection.baseline.fingerprint.0.clear();
    assert!(
        f.host
            .install_frozen_session_projection(
                SESSION,
                f.projection.clone(),
                None,
                true,
                Some(f.lease.clone())
            )
            .await
            .is_err()
    );
    assert!(f.host.session_slots.realization_leases().is_empty());
    let f = StagingFixture::new();
    let mut renewed = f.lease.clone();
    renewed.expires_at_unix_ms += 30_000;
    f.host
        .install_session_realization_lease(SESSION, renewed.clone());
    let task = f.start().await;
    assert_eq!(f.host.session_slots.realization_leases()[0].1, renewed);
    f.source.fail.store(true, Ordering::SeqCst);
    f.source.release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert_eq!(f.host.session_slots.realization_leases()[0].1, renewed);
}

// S13: 16 distinct Sessions blocked in real Resource staging remain visible to
// the one existing bounded scheduler. Renewal never publishes any baseline.
// Existing 64/512-Session tests own scheduler capacity/sort/no-due rules; this
// case adds actual cold I/O rather than duplicating those synthetic lease tests.
#[tokio::test]
async fn many_cold_resource_reads_share_the_existing_renewal_scheduler() {
    let f = StagingFixture::new();
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
        .renew_due_session_realizations(
            f.now,
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(30_000),
        )
        .await;
    let unpublished = (0..16).all(|index| {
        f.host
            .session_slots
            .read(&format!("cold-{index}"), |slot| {
                slot.baseline.is_none() && !slot.session_dispatch
            })
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
    let f = StagingFixture::new();
    let task = f.start().await;
    let revoked = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        f.host.revoke_all_session_realizations(),
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
    assert!(f.host.session_slots.realization_leases().is_empty());
}

// S15: crossing the initial deadline after a successful renewal is not lease
// loss. The preparation watchdog follows notifications from the current slot,
// not a frozen timeout copied from the original Control response.
#[tokio::test]
async fn renewed_preparation_survives_its_original_deadline() {
    let mut f = StagingFixture::new();
    f.lease.expires_at_unix_ms = f.now + 400;
    f.control
        .renewals
        .lock()
        .unwrap()
        .insert(SESSION.into(), f.lease.clone());
    let task = f.start().await;
    f.host
        .renew_due_session_realizations(
            f.now,
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(30_000),
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
    // The canonical slot must neither regress nor publish the obsolete input.
    for takeover in [false, true] {
        let f = StagingFixture::new();
        let preparation = f.start().await;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *f.control.renewal_response_gate.lock().unwrap() = Some(gate.clone());
        let host = f.host.clone();
        let now = f.now;
        let renewal = tokio::spawn(async move {
            host.renew_due_session_realizations(
                now,
                awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(30_000),
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
                .install_session_realization_lease(SESSION, expected.clone());
        } else {
            assert_eq!(
                f.host
                    .renew_due_session_realizations(
                        f.now,
                        awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(
                            60_000
                        )
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
        assert_eq!(f.host.session_slots.realization_leases()[0].1, expected);
        if takeover {
            f.assert_unpublished();
        } else {
            assert!(
                f.host
                    .session_slots
                    .read(SESSION, |slot| slot.baseline.is_some()
                        && slot.session_dispatch
                        && slot.resource_preparation.is_none())
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
    let mut f = StagingFixture::new();
    let ttl = 600;
    f.lease.expires_at_unix_ms = f.now + ttl;
    let timing = awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(ttl);
    let mut tasks = Vec::new();
    for index in 0..16 {
        let session = format!("soak-{index:02}");
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
        let now = crate::terminal_repository_publication::runtime_unix_now_ms();
        f.host
            .renew_due_session_realizations(now, timing)
            .await
            .unwrap();
        for (index, task) in tasks.iter().enumerate() {
            assert!(!task.is_finished(), "L1 session {index}");
            assert!(
                f.host
                    .session_slots
                    .read(&format!("soak-{index:02}"), |slot| slot.baseline.is_none()
                        && slot.resource_preparation.is_some()
                        && slot.realization_lease.as_ref().unwrap().expires_at_unix_ms
                            > now)
                    .unwrap()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    f.control
        .renewal_delay_ms
        .lock()
        .unwrap()
        .insert("soak-00".into(), ttl * 10);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !tasks[0].is_finished() {
            let _ = f
                .host
                .renew_due_session_realizations(
                    crate::terminal_repository_publication::runtime_unix_now_ms(),
                    timing,
                )
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
    assert!(f.host.thread_resource_manifest("soak-00").is_none());
}
