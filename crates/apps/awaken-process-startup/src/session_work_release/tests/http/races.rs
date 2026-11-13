//! Controlled HTTP request cuts on the selected original PostgreSQL owners.
use super::*;
use crate::worker_claimed_persistence::test_cut::{FinalAdmissionCutKind, FinalAdmissionTestCut};
use awaken_dispatch_contract::{SessionReservationAuthority as _, SessionRunReservations as _};
use awaken_session_execution_contract::{
    SessionRunActivityAdmission as Admission, SessionRunActivityAdmissionMode as Mode,
};

async fn reservation(f: &Fixture, owner: &str) -> (SessionId, RunClaim) {
    let (id, _) = f.owned_session(owner).await;
    let original = RunDispatch::new(awaken_runtime_contract::RunActivation::new(
        RunId(format!("http-receipt-{}", id.as_str())),
        ThreadId(id.to_string()),
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
            .model(
                awaken_runtime_contract::ModelBinding::parse("default", "parking-model", "default")
                    .unwrap(),
            )
            .build(),
        Vec::new(),
    ));
    f.dispatch.enqueue(original.clone()).await.unwrap();
    // Reuse the original explicit Pending adoption inlet. No timer, SQL update
    // or fabricated reservation lease is needed to establish this exact tenure.
    let fingerprint =
        awaken_session_types::SessionRunCommandFingerprint::from_sha256_hex(&"a".repeat(64))
            .unwrap();
    let claim = f
        .dispatch
        .adopt_pending_session_run(original, fingerprint, owner, 60_000)
        .await
        .unwrap()
        .unwrap();
    let guarded = f
        .dispatch
        .lock_session_run_reservation_epoch(&claim)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guarded.request().session_thread_id().0, id.as_str());
    assert!(guarded.request().session_activity_epoch.is_none());
    guarded.release().await.unwrap();
    (id, claim)
}

#[tokio::test]
#[ignore = "live PostgreSQL controlled actual-HTTP receipt race qualification"]
async fn live_postgres_http_same_claim_cold_absence_recovers_the_one_accepted_receipt() {
    // H9 causes: same exact reservation/mode; request A has completed its real
    // cold None receipt reads; B then accepts through real HTTP; A resumes final
    // validation. Effect: A returns B's exact canonical epoch, with full Root,
    // operation receipt and named-activity history unchanged after B. The cut
    // is before BEGIN and one-shot, so B uses the SAME cap1 product connection.
    // It grants no authority and cannot fake a receipt/claim. A claim-only
    // final check can otherwise be masked by a later admission recovery; the
    // per-instance typed Result observation must therefore report exactly one
    // read-basis move, beside the independent full-Root/receipt effect oracles.
    let f = Fixture::new(1, true, true).await;
    let cut = Arc::new(FinalAdmissionTestCut::new(
        FinalAdmissionCutKind::AbsentActivityReceipt,
    ));
    let http = HttpFixture::with_cut(&f, cut.clone()).await;
    let (id, claim) = reservation(&f, &http.identity.lease_owner()).await;
    let repository = f.managed.session_application().session_repository_handle();
    let operation =
        awaken_session_execution_contract::session_run_activity_operation_id(&id, &claim.run_id);
    let record = awaken_session_application::SessionActivities::activity_operation_record(
        id.as_str(),
        &operation,
    );
    assert!(
        repository
            .idempotency_receipt(&id, &record.key)
            .await
            .unwrap()
            .is_none()
    );
    let before = repository.get(&id).await.unwrap();
    let client = http.client.clone();
    let identity = http.identity.clone();
    let first_id = id.clone();
    let first_claim = claim.clone();
    let first = tokio::spawn(async move {
        client
            .admit_session_run_activity(
                &identity,
                &first_claim,
                first_id.as_str(),
                "assistant",
                &first_claim.run_id,
                Mode::RecoverOrAdmit,
            )
            .await
    });
    tokio::time::timeout(PROGRESS, cut.reached())
        .await
        .expect("A completed cold None before final admission");
    assert!(
        repository
            .idempotency_receipt(&id, &record.key)
            .await
            .unwrap()
            .is_none()
    );
    let accepted = tokio::time::timeout(
        PROGRESS,
        http.client.admit_session_run_activity(
            &http.identity,
            &claim,
            id.as_str(),
            "assistant",
            &claim.run_id,
            Mode::RecoverOrAdmit,
        ),
    )
    .await
    .expect("B must progress on A's released cap1 pool")
    .unwrap();
    let Admission::Admitted {
        session_activity_epoch: epoch,
    } = accepted
    else {
        panic!("B must accept the canonical original activity")
    };
    let winner = repository.get(&id).await.unwrap();
    let receipt = repository
        .idempotency_receipt(&id, &record.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.payload_hash, record.payload_hash);
    assert_eq!(receipt.committed_revision.0, epoch);
    assert!(winner.active_activity_epochs.contains(&epoch));
    assert_eq!(
        winner.active_activity_epochs.len(),
        before.active_activity_epochs.len() + 1
    );
    let (activities, _) = winner
        .run_activities()
        .expect("the original named activity is retained");
    assert_eq!(
        activities
            .iter()
            .filter(|item| item.operation_id() == operation.as_str())
            .count(),
        1
    );
    let named = activities
        .iter()
        .find(|item| item.operation_id() == operation.as_str())
        .unwrap();
    assert_eq!(named.activity_epoch().get(), epoch);
    assert_eq!(named.thread_id().0, id.as_str());
    assert_eq!(named.run_id(), &claim.run_id);

    cut.release();
    assert_eq!(
        tokio::time::timeout(PROGRESS, first)
            .await
            .expect("A must finish after the controlled cut")
            .unwrap()
            .unwrap(),
        Admission::Admitted {
            session_activity_epoch: epoch
        }
    );
    assert_eq!(
        cut.observed_read_basis_moves(),
        1,
        "A's actual final Result is the one typed read-basis Conflict, not a later fresh-admission recovery"
    );
    assert_eq!(
        repository.get(&id).await.unwrap(),
        winner,
        "no second revision or activity effect"
    );
    assert_eq!(
        repository
            .idempotency_receipt(&id, &record.key)
            .await
            .unwrap()
            .unwrap(),
        receipt,
        "the same receipt is the sole authority"
    );
    assert!(
        f.dispatch
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .is_none(),
        "neither admission request executes the Run"
    );
    drop(repository);
    drop(http);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "live PostgreSQL controlled actual-HTTP cold preparation takeover"]
async fn live_postgres_http_cold_resume_takeover_refuses_before_claim_specific_root_write() {
    // H5 cause/effect table: original actual-HTTP cap1 gate, two existing cuts.
    // C1 takeover adds a later claim; C2 cancellation revokes the old lease
    // WITHOUT an explicit subsequent claim. Both lose admitted Begin authority
    // and return typed StaleOwnership409. C3 the same live claim gains a physical
    // slot => typed NotReady. C4 reaches exact Dispatch lease expiry on the
    // original ManualClock AFTER the cut => typed StaleOwnership, before Work.
    // Registry/target admission already happened before this final boundary. R1 validates the original early read; R2 reaches
    // the actual final Root writer after that read accepted. All rows retain
    // Root/Work as observed AT the cut and preserve the resulting physical row.
    // Optional continuation renewal is legal before Begin exclusion: these
    // assertions never claim whole-request zero mutation before such renewal.
    // R3 selects the fourth original ReadValidation: bootstrap repository read
    // and final directive (two matches), then unchanged continuation repository
    // read and final no-op directive. Assert the bootstrap count and unchanged
    // Root at the cut; this row really reaches the post-cold no-write return.
    // No production hook/order changes and no sleeps select this boundary.
    #[derive(Clone, Copy, Debug)]
    enum Change {
        Takeover,
        CancelOnly,
        Occupy,
        ExpireAtLeaseBoundary,
    }
    for (cut_kind, skip, final_noop, change) in [
        (
            FinalAdmissionCutKind::PreparedResumeReadValidation,
            0,
            false,
        ),
        (FinalAdmissionCutKind::PreparedResumeRootCommit, 0, false),
        (FinalAdmissionCutKind::PreparedResumeReadValidation, 3, true),
    ]
    .into_iter()
    .flat_map(|(cut, skip, final_noop)| {
        let changes = if final_noop {
            [
                Change::CancelOnly,
                Change::Occupy,
                Change::Takeover,
                Change::ExpireAtLeaseBoundary,
            ]
        } else {
            [
                Change::Takeover,
                Change::CancelOnly,
                Change::Occupy,
                Change::ExpireAtLeaseBoundary,
            ]
        };
        changes.map(move |change| (cut, skip, final_noop, change))
    }) {
        let f = Fixture::new(1, true, true).await;
        let cut = Arc::new(FinalAdmissionTestCut::after_matching(cut_kind, skip));
        let http = HttpFixture::with_cut(&f, cut.clone()).await;
        let mut entry = f.entry_for(&http.identity.lease_owner()).await;
        finish_fixture_attempt(&f, &entry).await;
        // Closing retires the old physical epoch permanently. Obtain a real
        // successor before the request; never reopen that retired attempt.
        let retired_epoch = entry.claim.epoch;
        // Relinquish preserves an uncancelled Run and its retry budget;
        // cancellation is absorbing and cannot prepare a fresh executable row.
        assert_eq!(
            f.dispatch.relinquish_claim(&entry.claim).await.unwrap(),
            awaken_dispatch::SettleOutcome::Applied
        );
        let fresh = f
            .dispatch
            .claim_run(
                &entry.claim.run_id,
                &http.identity.lease_owner(),
                60_000,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
        entry.claim = RunClaim::from(&fresh.lease);
        assert!(entry.claim.epoch > retired_epoch);
        let fresh_guard = f
            .dispatch
            .lock_commit_epoch(&entry.claim)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !fresh_guard.cancellation_requested(),
            "fresh execution is uncancelled"
        );
        assert!(
            fresh_guard.permits_settlement(),
            "fresh execution has no physical slot"
        );
        fresh_guard.release().await.unwrap();
        let receipt = http.receipt(&f, &entry).await;
        let (continuation, no_op_root) = if final_noop {
            let directive = tokio::time::timeout(
                PROGRESS,
                http.client.resume_session(
                    &http.identity,
                    &entry.claim,
                    entry.id.as_str(),
                    Some(&receipt),
                    None,
                ),
            )
            .await
            .expect("bootstrap progresses")
            .unwrap()
            .unwrap();
            assert_eq!(
                cut.remaining_skips(),
                1,
                "two original bootstrap validations"
            );
            let root = f
                .managed
                .session_application()
                .session_repository_handle()
                .get(&entry.id)
                .await
                .unwrap();
            (Some(directive.authority.lease), Some(root))
        } else {
            (None, None)
        };
        let upstream = http.upstream.clone();
        let identity = http.identity.clone();
        let old_claim = entry.claim.clone();
        let session = entry.id.clone();
        let request = tokio::spawn(async move {
            let path = "/v1/worker/session/resume";
            let url = format!("{}{}", upstream.base_url(), path);
            upstream.authorize("POST", path, upstream.client().post(url)).unwrap()
                .json(&serde_json::json!({"identity":identity, "claim":old_claim, "session_id":session, "session_work_authority":receipt, "continuation":continuation}))
                .send().await.unwrap()
        });
        tokio::time::timeout(PROGRESS, cut.reached())
            .await
            .expect("cold preparation reaches final gate without owning D");
        let repository = f.managed.session_application().session_repository_handle();
        let prepared = repository.get(&entry.id).await.unwrap();
        if let Some(before) = no_op_root {
            assert_eq!(
                prepared, before,
                "the continuation Begin at this cut made no Root write"
            );
        }
        let work = f.work_row(&entry.id).await;
        let successor = tokio::time::timeout(PROGRESS, async {
            match change {
                Change::Takeover => {
                    f.dispatch.cancel(&entry.claim.run_id).await.unwrap();
                    Some(
                        f.dispatch
                            .claim_run(
                                &entry.claim.run_id,
                                "http-successor",
                                60_000,
                                &Default::default(),
                            )
                            .await
                            .unwrap()
                            .unwrap(),
                    )
                }
                Change::CancelOnly => {
                    f.dispatch.cancel(&entry.claim.run_id).await.unwrap();
                    None
                }
                Change::Occupy => {
                    assert_eq!(
                        f.dispatch.begin_attempt(&entry.claim, None).await.unwrap(),
                        awaken_dispatch::AttemptAdmission::Applied
                    );
                    None
                }
                Change::ExpireAtLeaseBoundary => {
                    let guard = f
                        .dispatch
                        .lock_commit_epoch(&entry.claim)
                        .await
                        .unwrap()
                        .unwrap();
                    let expires_ms = guard.observation().expires_ms;
                    guard.release().await.unwrap();
                    f.clock.set(expires_ms);
                    None
                }
            }
        })
        .await
        .expect("the cut holds no D connection while the real row changes");
        let successor_claim = successor.as_ref().map(|next| RunClaim::from(&next.lease));
        if let Some(next) = &successor_claim {
            assert!(next.epoch > entry.claim.epoch);
        }
        let physical = f
            .dispatch
            .load_physical_attempt(&entry.claim.run_id)
            .await
            .unwrap();
        cut.release();
        let response = tokio::time::timeout(PROGRESS, request)
            .await
            .expect("stale A exits after the cut")
            .unwrap();
        assert_eq!(
            repository.get(&entry.id).await.unwrap(),
            prepared,
            "zero stale-claim Root assignment at {cut_kind:?}"
        );
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            status,
            axum::http::StatusCode::CONFLICT,
            "refused at {cut_kind:?}/{change:?}, final_noop={final_noop}, skip={skip}, body={body}"
        );
        {
            let expected = if matches!(change, Change::Occupy) {
                "not_ready"
            } else {
                "stale_ownership"
            };
            assert_eq!(
                body["realization_error"]["kind"], expected,
                "original typed wire cause"
            );
        }
        assert_eq!(
            f.work_row(&entry.id).await,
            work,
            "zero final Work release/renewal"
        );
        assert_eq!(
            f.dispatch
                .load_physical_attempt(&entry.claim.run_id)
                .await
                .unwrap(),
            physical
        );
        if let (Some(successor_claim), Some(successor)) = (&successor_claim, &successor) {
            let guard = f
                .dispatch
                .lock_commit_epoch(successor_claim)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(guard.request(), &successor.request);
            guard.release().await.unwrap();
        }
        drop(repository);
        drop(http);
        f.cleanup().await;
    }
}
