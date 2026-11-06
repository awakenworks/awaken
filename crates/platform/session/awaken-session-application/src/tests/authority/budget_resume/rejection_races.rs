// @formal evidence session_execution.activity_receipts_are_unique_durable_and_exactly_settled
//! Actual SQLite receipt/root races. Existing scalar Kani and TLA results do
//! not model mutable budget + negative receipt + root revision and are not
//! evidence that these I/O interleavings are correct.
use super::*;
use awaken_session_execution_contract::{AdmittedSessionRun, ChildRunCoordination as _};
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

struct BudgetAdmissionRace {
    durable: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    repository: Arc<FaultingSessionRepository>,
    runtime: Arc<EffectCountingRuntime>,
    reads: Arc<crate::tests::event_batches::EventBatchRuntime>,
    app: Arc<SessionApplication>,
}

async fn race_fixture(id: &str) -> BudgetAdmissionRace {
    let durable = authority_repository("budget receipt race SQLite").await;
    create(durable.as_ref(), reached_session(id)).await;
    let repository = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let runtime = Arc::new(EffectCountingRuntime::default());
    runtime.set_budget_tickets(Vec::new());
    let reads = Arc::new(crate::tests::event_batches::EventBatchRuntime::default());
    let selected = crate::SessionExecutor::colocated(runtime.clone(), Arc::new(NoopMcpRealizer));
    let mut roles = (*selected).clone();
    roles.thread_reads = reads.clone();
    let app = Arc::new(
        SessionApplicationBuilder::new(
            roles,
            SessionPersistence::new(repository.clone(), system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            {
                let probe = std::sync::Arc::new(crate::ExecutionObservationProbe::default());
                crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
            },
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .build()
        .expect("same original roles with the existing read gate"),
    );
    // Recover the original execution projection before arming the read gate.
    // admit_run_session has no independent budget refusal; the activity owner
    // alone decides this reached budget after Runtime reservation.
    app.run_admissions
        .admit_run_session("workspace", id, "agent")
        .await
        .unwrap();
    let before = durable.get(&SessionId::from(id)).await.unwrap();
    assert!(!before.budget.can_admit_model_request());
    assert!(before.running_interval.is_none() && before.active_activity_epochs.is_empty());
    BudgetAdmissionRace {
        durable,
        repository,
        runtime,
        reads,
        app,
    }
}

async fn at_admission_cursor<F: Future>(
    reads: &crate::tests::event_batches::EventBatchRuntime,
    mut pending: Pin<&mut F>,
) {
    // Poll the real future through its actual SQLite reads until the existing
    // one-shot ThreadReads barrier is consumed. The pending future then
    // owns the old root immediately before open_activity_on_snapshot. This is
    // a causal scheduling cut, not an outcome oracle or an Nth-get hook.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        std::future::poll_fn(|cx| {
            if pending.as_mut().poll(cx).is_ready() {
                panic!("activity finished before the admission cursor cut");
            }
            if reads.admission_barrier.lock().unwrap().is_none() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }),
    )
    .await
    .expect("actual old-root cursor cut");
}

async fn raced_activity(
    fixture: &BudgetAdmissionRace,
    command: &AdmitSessionRun,
    projected: bool,
) -> Result<u64, RunError> {
    let thread = ThreadId(command.session_id.to_string());
    let operation = awaken_session_execution_contract::session_run_activity_operation_id(
        &command.session_id,
        &command.run_id,
    );
    if projected {
        let revision = fixture
            .durable
            .get(&command.session_id)
            .await
            .unwrap()
            .revision;
        fixture
            .app
            .activities
            .begin_run_activity_at_revision(
                &command.session_id,
                &operation,
                revision,
                (&thread, &command.run_id),
            )
            .await
    } else {
        fixture
            .app
            .activities
            .begin_activity_for_operation_with_policy(
                &command.session_id,
                &operation,
                false,
                None,
                None,
                Some((&thread, &command.run_id)),
            )
            .await
            .map(|(_, epoch)| epoch)
            .map_err(|error| crate::SessionActivities::activity_operation_error(&operation, error))
    }
}

fn captured_record(
    capture: &Arc<PausedSessionMutation>,
) -> awaken_session_contract::IdempotencyRecord {
    capture
        .candidate
        .lock()
        .unwrap()
        .as_ref()
        .expect("actual canonical receipt CAS was reached")
        .idempotency
        .clone()
}

fn accepted_epoch(admitted: AdmittedSessionRun) -> u64 {
    match admitted {
        AdmittedSessionRun::Reserved(delivery) | AdmittedSessionRun::AlreadyReserved(delivery) => {
            delivery.session_activity_epoch
        }
        other => panic!("expected one reserved original Run: {other:?}"),
    }
}

#[tokio::test]
async fn stale_budget_decision_replays_exact_accepted_activity_before_and_after_settlement() {
    // Decision rules A1-A4: old reached root is held at the natural cursor cut;
    // the cap rises and the exact same reserved Run commits one activity; the
    // stale caller is fast/projected or direct repair, with that epoch active
    // or already settled. All four rows return the accepted original epoch,
    // preserve the complete current root and exact receipt, and add no refusal,
    // activation, model call or second activity. Root equality detects a false
    // rejection CAS because every repository Replace advances revision.
    for projected in [false, true] {
        for settled in [false, true] {
            let id = format!("budget-accepted-{projected}-{settled}");
            let fixture = race_fixture(&id).await;
            let command = bypass_run(&id);
            fixture
                .runtime
                .reserve_session_run(command.clone())
                .await
                .unwrap();
            fixture
                .runtime
                .already_reserved
                .store(true, Ordering::SeqCst);
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            *fixture.reads.admission_barrier.lock().unwrap() = Some(barrier.clone());
            let mut pending = Box::pin(raced_activity(&fixture, &command, projected));
            at_admission_cursor(fixture.reads.as_ref(), pending.as_mut()).await;
            fixture
                .app
                .update_commands
                .update_session(
                    &command.session_id,
                    raise(CONSUMED_MINOR + 1, "accepted-raise"),
                )
                .await
                .unwrap();
            let capture = fixture
                .repository
                .pause_next_mutation("managed:session-activity:");
            capture.release.notify_one();
            let epoch = accepted_epoch(
                fixture
                    .app
                    .run_admissions
                    .admit_session_run(command.clone())
                    .await
                    .unwrap(),
            );
            let record = captured_record(&capture);
            if settled {
                fixture
                    .app
                    .lifecycle
                    .settle_activity_observed(&command.session_id, epoch, None)
                    .await
                    .unwrap();
            }
            let expected = fixture.durable.get(&command.session_id).await.unwrap();
            let receipt = fixture
                .durable
                .idempotency_receipt(&command.session_id, &record.key)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(receipt.payload_hash, record.payload_hash);
            assert_eq!(receipt.committed_revision.0, epoch);
            let (_, result) = tokio::join!(barrier.wait(), pending);
            assert_eq!(
                result.unwrap(),
                epoch,
                "A1-A4 accepted receipt wins, not a budget error"
            );
            assert_eq!(
                fixture.durable.get(&command.session_id).await.unwrap(),
                expected,
                "A1-A4 no false negative or reopened epoch"
            );
            assert_eq!(
                fixture
                    .durable
                    .idempotency_receipt(&command.session_id, &record.key)
                    .await
                    .unwrap(),
                Some(receipt),
                "A1-A4 exact original receipt"
            );
            assert_eq!(activity_record_count(&expected), 1);
            assert_eq!(
                expected.active_activity_epochs,
                if settled {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([epoch])
                }
            );
            assert_eq!(fixture.runtime.counts().activations, 0);
            assert_eq!(fixture.runtime.counts().model_calls, 0);
        }
    }
}

#[tokio::test]
async fn raised_cap_without_an_accepted_receipt_leaves_the_old_admission_pending() {
    // U1/U2: old reached decision + cap raise alone + fast/direct repair =>
    // Pending, no refusal receipt, no epoch and full root equality. A later
    // fresh-projection retry of that exact reserved Run succeeds once; that
    // positive control also detects any false durable rejection latch.
    for projected in [false, true] {
        let id = format!("budget-raised-only-{projected}");
        let fixture = race_fixture(&id).await;
        let command = bypass_run(&id);
        fixture
            .runtime
            .reserve_session_run(command.clone())
            .await
            .unwrap();
        fixture
            .runtime
            .already_reserved
            .store(true, Ordering::SeqCst);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        *fixture.reads.admission_barrier.lock().unwrap() = Some(barrier.clone());
        let mut pending = Box::pin(raced_activity(&fixture, &command, projected));
        at_admission_cursor(fixture.reads.as_ref(), pending.as_mut()).await;
        fixture
            .app
            .update_commands
            .update_session(&command.session_id, raise(CONSUMED_MINOR + 1, "only-raise"))
            .await
            .unwrap();
        let raised = fixture.durable.get(&command.session_id).await.unwrap();
        let (_, result) = tokio::join!(barrier.wait(), pending);
        assert_eq!(
            result.unwrap_err().code,
            "session_run_admission_pending",
            "U1/U2 undecided, not stale admission/refusal"
        );
        assert_eq!(
            fixture.durable.get(&command.session_id).await.unwrap(),
            raised
        );
        assert!(raised.active_activity_epochs.is_empty());
        assert_eq!(activity_record_count(&raised), 0);
        // Q0: absent positive/negative receipts keep the fresh-root future
        // unpolled. Observe actual repository get entries, not a new hook.
        let root_reads = fixture.repository.observed_gets.lock().unwrap().len();
        let operation = awaken_session_execution_contract::session_run_activity_operation_id(
            &command.session_id,
            &command.run_id,
        );
        let thread = ThreadId(id.clone());
        assert!(
            fixture
                .app
                .activities
                .recover_activity_for_operation(
                    &command.session_id,
                    &operation,
                    Some((&thread, &command.run_id)),
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fixture.repository.observed_gets.lock().unwrap().len(),
            root_reads,
            "Q0 no root read without a receipt"
        );
        let epoch = accepted_epoch(
            fixture
                .app
                .run_admissions
                .admit_session_run(command.clone())
                .await
                .unwrap(),
        );
        let admitted = fixture.durable.get(&command.session_id).await.unwrap();
        assert_eq!(admitted.active_activity_epochs, BTreeSet::from([epoch]));
        assert_eq!(
            activity_record_count(&admitted),
            1,
            "U1/U2 exact retry remains repairable"
        );
        assert_eq!(fixture.runtime.counts().activations, 0);
        assert_eq!(fixture.runtime.counts().model_calls, 0);
    }
}

#[tokio::test]
async fn fast_and_worker_budget_refusals_stay_closed_after_a_cap_raise() {
    // R1 fast admission / R2 Worker RecoverOrAdmit themselves must persist the
    // same negative receipt before BudgetReached/Rejected. Raise afterwards:
    // direct positive-policy retry and both repair modes retain refusal with
    // byte-equal root/receipt and no admitted epoch. R2 cannot pass by calling
    // the fast wrapper first: its only first entrance is the Worker role.
    for worker in [false, true] {
        let id = format!("budget-negative-first-{worker}");
        let fixture = race_fixture(&id).await;
        let command = bypass_run(&id);
        let capture = fixture
            .repository
            .pause_next_mutation("managed:session-run-rejection:");
        capture.release.notify_one();
        if worker {
            assert!(matches!(
                fixture
                    .app
                    .coordination
                    .admit_session_run_activity(
                        &command.session_id,
                        "agent",
                        &command.run_id,
                        SessionRunActivityAdmissionMode::RecoverOrAdmit
                    )
                    .await
                    .unwrap(),
                SessionRunActivityAdmission::Rejected
            ));
        } else {
            assert_eq!(
                fixture
                    .app
                    .run_admissions
                    .admit_session_run(command.clone())
                    .await
                    .unwrap_err()
                    .code,
                "budget_reached"
            );
        }
        let record = captured_record(&capture);
        let receipt = fixture
            .durable
            .idempotency_receipt(&command.session_id, &record.key)
            .await
            .unwrap()
            .unwrap();
        fixture
            .app
            .update_commands
            .update_session(
                &command.session_id,
                raise(CONSUMED_MINOR + 1, "negative-raise"),
            )
            .await
            .unwrap();
        let raised = fixture.durable.get(&command.session_id).await.unwrap();
        let thread = ThreadId(id.clone());
        let operation = awaken_session_execution_contract::session_run_activity_operation_id(
            &command.session_id,
            &command.run_id,
        );
        assert_eq!(
            fixture
                .app
                .activities
                .begin_activity_for_operation_with_policy(
                    &command.session_id,
                    &operation,
                    false,
                    None,
                    None,
                    Some((&thread, &command.run_id))
                )
                .await
                .unwrap_err(),
            SessionActivityError::BudgetReached,
            "R1/R2 the positive loop respects a durable negative"
        );
        for mode in [
            SessionRunActivityAdmissionMode::RecoverOnly,
            SessionRunActivityAdmissionMode::RecoverOrAdmit,
        ] {
            assert!(matches!(
                fixture
                    .app
                    .coordination
                    .admit_session_run_activity(&command.session_id, "agent", &command.run_id, mode)
                    .await
                    .unwrap(),
                SessionRunActivityAdmission::Rejected
            ));
        }
        assert_eq!(
            fixture.durable.get(&command.session_id).await.unwrap(),
            raised
        );
        assert_eq!(
            fixture
                .durable
                .idempotency_receipt(&command.session_id, &record.key)
                .await
                .unwrap(),
            Some(receipt)
        );
        assert!(raised.active_activity_epochs.is_empty());
        assert_eq!(activity_record_count(&raised), 0);
        assert_eq!(fixture.runtime.counts().activations, 0);
    }
}

#[tokio::test]
async fn refusal_cas_conflict_and_store_outage_remain_pending_in_both_entrances() {
    // E1-E4: fast/Worker x before-write outage/committed-response conflict.
    // Every first answer is Pending, never Rejected. Outage leaves the full
    // root and receipt absent; lost response retains the real negative receipt
    // but reports uncertainty. A subsequent healthy Worker retry closes the
    // same refusal, with no activity/model/activation in any row.
    for worker in [false, true] {
        for outage in [false, true] {
            let id = format!("budget-refusal-fault-{worker}-{outage}");
            let fixture = race_fixture(&id).await;
            let command = bypass_run(&id);
            let before = fixture.durable.get(&command.session_id).await.unwrap();
            let capture = fixture
                .repository
                .pause_next_mutation("managed:session-run-rejection:");
            capture.release.notify_one();
            if outage {
                fixture
                    .repository
                    .fail_once("managed:session-run-rejection:");
            } else {
                fixture
                    .repository
                    .commit_then_conflict_once("managed:session-run-rejection:");
            }
            let error = if worker {
                fixture
                    .app
                    .coordination
                    .admit_session_run_activity(
                        &command.session_id,
                        "agent",
                        &command.run_id,
                        SessionRunActivityAdmissionMode::RecoverOrAdmit,
                    )
                    .await
                    .unwrap_err()
            } else {
                fixture
                    .app
                    .run_admissions
                    .admit_session_run(command.clone())
                    .await
                    .unwrap_err()
            };
            assert_eq!(
                error.code, "session_run_admission_pending",
                "E1-E4 uncertainty cannot become rejected"
            );
            let record = captured_record(&capture);
            let after = fixture.durable.get(&command.session_id).await.unwrap();
            let receipt = fixture
                .durable
                .idempotency_receipt(&command.session_id, &record.key)
                .await
                .unwrap();
            if outage {
                assert_eq!(after, before);
                assert!(receipt.is_none());
            } else {
                assert!(after.revision > before.revision);
                assert_eq!(receipt.unwrap().payload_hash, record.payload_hash);
            }
            assert!(matches!(
                fixture
                    .app
                    .coordination
                    .admit_session_run_activity(
                        &command.session_id,
                        "agent",
                        &command.run_id,
                        SessionRunActivityAdmissionMode::RecoverOrAdmit
                    )
                    .await
                    .unwrap(),
                SessionRunActivityAdmission::Rejected
            ));
            let closed = fixture.durable.get(&command.session_id).await.unwrap();
            assert!(closed.active_activity_epochs.is_empty());
            assert_eq!(activity_record_count(&closed), 0);
            assert_eq!(fixture.runtime.counts().activations, 0);
            assert_eq!(fixture.runtime.counts().model_calls, 0);
        }
    }
}

#[tokio::test]
async fn accepted_and_rejected_receipt_payload_mismatches_fail_closed_without_admitting() {
    // H1/H2: capture the actual positive/negative CAS key, then let the same
    // canonical mutation owner persist that key with another payload first.
    // The interrupted command and later RecoverOnly/RecoverOrAdmit must fail
    // closed through the shared hash checker as a known permanent authority
    // fault; no receipt is treated as absence and no activity/root write follows.
    // There is no copied key/hash compiler or transient retry classification.
    let assert_corruption = |error: awaken_runtime_contract::runs::RunError| {
        assert_eq!(
            error.kind,
            awaken_runtime_contract::runs::RunErrorKind::Internal
        );
        assert_eq!(error.code, "session_application_invalid");
        assert!(!error.is_unclassified_internal());
    };
    for positive in [false, true] {
        let id = format!("budget-receipt-hash-{positive}");
        let fixture = race_fixture(&id).await;
        let command = bypass_run(&id);
        if positive {
            fixture
                .app
                .update_commands
                .update_session(&command.session_id, raise(CONSUMED_MINOR + 1, "hash-raise"))
                .await
                .unwrap();
        }
        let prefix = if positive {
            "managed:session-activity:"
        } else {
            "managed:session-run-rejection:"
        };
        let capture = fixture.repository.pause_next_mutation(prefix);
        let mut pending = Box::pin(raced_activity(&fixture, &command, true));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                _ = capture.entered.notified() => {},
                _ = pending.as_mut() => panic!("CAS did not reach actual receipt capture"),
            }
        })
        .await
        .unwrap();
        let record = captured_record(&capture);
        let mut wrong = record.clone();
        wrong.payload_hash = format!("different-{}", wrong.payload_hash);
        let original = fixture.durable.get(&command.session_id).await.unwrap();
        let poisoned_receipt = fixture
            .app
            .mutations
            .commit_session_snapshot_with_record(
                "workspace",
                SessionAggregate::try_from(original).unwrap(),
                wrong.clone(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(
            poisoned_receipt.transitioned(),
            "H1/H2 the conflicting payload is durable before recovery"
        );
        let expected = fixture.durable.get(&command.session_id).await.unwrap();
        let receipt = fixture
            .durable
            .idempotency_receipt(&command.session_id, &record.key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.payload_hash, wrong.payload_hash);
        capture.release.notify_one();
        assert_corruption(pending.await.unwrap_err());
        let thread = ThreadId(id.clone());
        let operation = awaken_session_execution_contract::session_run_activity_operation_id(
            &command.session_id,
            &command.run_id,
        );
        // Q1: the shared hash check precedes polling the snapshot source for
        // both receipt classes; corruption cannot trigger another root read.
        let root_reads = fixture.repository.observed_gets.lock().unwrap().len();
        assert!(
            matches!(
                fixture
                    .app
                    .activities
                    .recover_activity_for_operation(
                        &command.session_id,
                        &operation,
                        Some((&thread, &command.run_id))
                    )
                    .await,
                Err(SessionActivityError::Internal(detail))
                    if detail == "Session activity operation id was reused with another payload"
            ),
            "H1/H2 mismatch is a permanent fault, never absence or pending admission"
        );
        assert_eq!(
            fixture.repository.observed_gets.lock().unwrap().len(),
            root_reads,
            "Q1 hash mismatch is checked before root I/O"
        );
        for mode in [
            SessionRunActivityAdmissionMode::RecoverOnly,
            SessionRunActivityAdmissionMode::RecoverOrAdmit,
        ] {
            assert_corruption(
                fixture
                    .app
                    .coordination
                    .admit_session_run_activity(&command.session_id, "agent", &command.run_id, mode)
                    .await
                    .unwrap_err(),
            );
        }
        assert_eq!(
            fixture.durable.get(&command.session_id).await.unwrap(),
            expected
        );
        assert_eq!(
            fixture
                .durable
                .idempotency_receipt(&command.session_id, &record.key)
                .await
                .unwrap(),
            Some(receipt)
        );
        assert!(expected.active_activity_epochs.is_empty());
        assert_eq!(activity_record_count(&expected), 0);
        assert_eq!(fixture.runtime.counts().activations, 0);
    }
}

#[tokio::test]
async fn refusal_cas_loses_to_a_real_concurrent_raise_with_or_without_exact_acceptance() {
    // C1 the latest reached-root refusal candidate has actually reached the
    // original repository CAS; C2 another writer raises the cap; C3 that writer
    // also accepts the exact Run, or leaves it undecided. E5/E6 the REAL SQLite
    // revision fence refuses the stale negative replacement, so the first
    // answer stays Pending, the competing full root is byte-equal and no false
    // negative receipt exists. Canonical recovery then yields the accepted
    // original epoch or None; no response-conflict injection substitutes for
    // the actual CAS-loss rows. The adjacent fault table covers lost responses.
    for accepted in [false, true] {
        let id = format!("budget-real-cas-loss-{accepted}");
        let fixture = race_fixture(&id).await;
        let command = bypass_run(&id);
        let capture = fixture
            .repository
            .pause_next_mutation("managed:session-run-rejection:");
        let mut pending = Box::pin(raced_activity(&fixture, &command, true));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                _ = capture.entered.notified() => {},
                _ = pending.as_mut() => panic!("latest refusal did not reach its actual CAS"),
            }
        })
        .await
        .unwrap();
        let negative = captured_record(&capture);
        fixture
            .app
            .update_commands
            .update_session(&command.session_id, raise(CONSUMED_MINOR + 1, "cas-raise"))
            .await
            .unwrap();
        let epoch = if accepted {
            Some(accepted_epoch(
                fixture
                    .app
                    .run_admissions
                    .admit_session_run(command.clone())
                    .await
                    .unwrap(),
            ))
        } else {
            None
        };
        let expected = fixture.durable.get(&command.session_id).await.unwrap();
        capture.release.notify_one();
        assert_eq!(
            pending.await.unwrap_err().code,
            "session_run_admission_pending",
            "E5/E6 genuine revision conflict remains undecided"
        );
        assert_eq!(
            fixture.durable.get(&command.session_id).await.unwrap(),
            expected
        );
        assert!(
            fixture
                .durable
                .idempotency_receipt(&command.session_id, &negative.key)
                .await
                .unwrap()
                .is_none(),
            "E5/E6 no false negative write"
        );
        let thread = ThreadId(id.clone());
        let operation = awaken_session_execution_contract::session_run_activity_operation_id(
            &command.session_id,
            &command.run_id,
        );
        let recovered = fixture
            .app
            .activities
            .recover_activity_for_operation(
                &command.session_id,
                &operation,
                Some((&thread, &command.run_id)),
            )
            .await
            .unwrap();
        assert_eq!(recovered.as_ref().map(|(_, observed)| *observed), epoch);
        assert_eq!(activity_record_count(&expected), usize::from(accepted));
        assert_eq!(fixture.runtime.counts().activations, 0);
        assert_eq!(fixture.runtime.counts().model_calls, 0);
    }
}
