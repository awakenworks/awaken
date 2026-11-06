//! T07-C01 (激活前零执行): pause or fail after the Run reservation, at the
//! Session activity CAS, and stop before activation.
//!
//! Oracles (groups-2.json order):
//! - O0 an un-activated Run calls no model and no tool;
//! - O1 an un-admitted reservation has an explicit release responsibility.
//!
//! Negative control: a success/error return code is never accepted in place
//! of the model/tool counters (executed in
//! `return_code_alone_cannot_sign_off_zero_execution`).
//!
//! | Row | Cut | Caller result | Root activity | Repair RecoverOnly | Repair RecoverOrAdmit |
//! |---|---|---|---|---|---|
//! | Z1 | activity CAS paused | pending | none | n/a | n/a |
//! | Z2 | activity CAS fails | Err(session_run_admission_pending) | none | Rejected (delete) | Admitted once (pending settled) |
//! | Z3 | retry same operation | AlreadyReserved(e) | exactly one record e | Admitted(e) | Admitted(e) |
//! | Z4 | admitted, activate never called | Reserved(e) | one record e | Admitted(e) | Admitted(e) |
use super::*;
use crate::tests::effect_counting_runtime::{EffectCountingRuntime, EffectCounts};
use awaken_session_execution_contract::{
    SessionAgentCoordination as _, SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};

const ACTIVITY_CAS: &str = "managed:session-activity:";

fn admit_command(session: &str, operation: &str) -> AdmitSessionRun {
    let session_id = SessionId::from(session);
    AdmitSessionRun {
        session_id: session_id.clone(),
        agent_id: "agent".into(),
        operation_id: operation.into(),
        run_id: awaken_session_execution_contract::session_run_id(&session_id, operation),
        messages: vec![awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id(format!("{operation}-input")),
            awaken_agent_contract::agent::message::Role::User,
            "must not reach a model before activation",
        )],
        data_subject_id: None,
        traceparent: None,
        execution_requirements: Default::default(),
        replacement: Default::default(),
        private_task_input: None,
    }
}

/// (operation id, epoch, run id) of every durable activity admission record.
fn activity_records(session: &PersistedSession) -> Vec<(String, u64, String)> {
    session
        .run_activities()
        .map(|(records, _)| {
            records
                .iter()
                .map(|record| {
                    (
                        record.operation_id().to_string(),
                        record.activity_epoch().get(),
                        record.run_id().0.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The O0 checker. It deliberately ignores the admission return value and
/// inspects only the counted Runtime effects.
fn check_zero_execution(counts: &EffectCounts) -> Result<(), String> {
    if counts.model_calls != 0 || counts.tool_calls != 0 || counts.activations != 0 {
        return Err(format!("pre-activation execution leaked: {counts:?}"));
    }
    Ok(())
}

/// A freshly created Session with the explicit empty activity history.
/// `persisted()` models a legacy root without `run_activities`, on which the
/// reducer retains no admission record at all.
fn fresh_session(id: &str) -> PersistedSession {
    let legacy = persisted(id, false, "idle");
    let mut session = PersistedSession::frozen_with_budget(
        id,
        legacy.frozen_baseline().unwrap().clone(),
        legacy.resources,
        legacy.mcp,
        legacy.title,
        legacy.metadata,
        legacy.tools,
        legacy.budget,
    );
    session.execution = legacy.execution;
    assert_eq!(
        session.run_activities(),
        Some((&[][..], false)),
        "fixture retains complete activity history"
    );
    session
}

struct Fixture {
    durable: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    faulting: Arc<FaultingSessionRepository>,
    runtime: Arc<EffectCountingRuntime>,
    app: Arc<SessionApplication>,
}

async fn fixture(session: &str) -> Fixture {
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("T07 Session store"),
    );
    create(durable.as_ref(), fresh_session(session)).await;
    let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let runtime = Arc::new(EffectCountingRuntime::default());
    let app = Arc::new(application_with_runtime(
        runtime.clone(),
        faulting.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    Fixture {
        durable,
        faulting,
        runtime,
        app,
    }
}

#[tokio::test]
async fn paused_or_failed_activity_cas_executes_nothing_and_leaves_a_repairable_reservation() {
    let session = "t07-pre-activation";
    let id = SessionId::from(session);
    let Fixture {
        durable,
        faulting,
        runtime,
        app,
    } = fixture(session).await;
    let command = admit_command(session, "t07-operation");
    let run_id = command.run_id.clone();
    let operation =
        awaken_session_execution_contract::session_run_activity_operation_id(&id, &run_id);

    // Z1: hold the real activity CAS after the reservation is durable.
    let pause = faulting.pause_next_mutation(ACTIVITY_CAS);
    let delayed = {
        let app = app.clone();
        let command = command.clone();
        tokio::spawn(async move { app.run_admissions.admit_session_run(command).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.entered.notified())
        .await
        .expect("Z1 the admission reaches the activity CAS");
    let paused = runtime.counts();
    assert_eq!(paused.reservations, 1, "Z1 reservation precedes the CAS");
    check_zero_execution(&paused).expect("Z1/O0 nothing executes while the CAS is held");
    assert!(
        activity_records(&durable.get(&id).await.unwrap()).is_empty(),
        "Z1 no activity record before the CAS"
    );
    let candidate = pause
        .candidate
        .lock()
        .unwrap()
        .clone()
        .expect("Z1 CAS candidate");
    assert!(
        candidate.idempotency.key.starts_with(ACTIVITY_CAS),
        "Z1 the held mutation is the activity CAS: {}",
        candidate.idempotency.key
    );

    // Z2: the held CAS now fails definitively (authority outage).
    faulting.fail_once(ACTIVITY_CAS);
    pause.release.notify_one();
    let error = tokio::time::timeout(std::time::Duration::from_secs(10), delayed)
        .await
        .expect("Z2 admission completes")
        .expect("Z2 admission task")
        .expect_err("Z2 failed activity CAS refuses admission");
    assert_eq!(
        error.kind,
        awaken_runtime_contract::runs::RunErrorKind::Unavailable,
        "Z2 {error:?}"
    );
    let failed = runtime.counts();
    assert_eq!(failed.reservations, 1, "Z2 one reservation, never repeated");
    check_zero_execution(&failed).expect("Z2/O0 a failed CAS executes nothing");
    let after_failure = durable.get(&id).await.unwrap();
    assert!(
        activity_records(&after_failure).is_empty(),
        "Z2 no activity record"
    );
    assert!(after_failure.active_activity_epochs.is_empty(), "Z2");
    assert_eq!(
        after_failure.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "Z2"
    );

    // Z2/O1: the release owner is the reservation repair. Cancellation
    // (RecoverOnly) may delete the never-admitted reservation: the Session
    // root answers Rejected and records nothing.
    assert_eq!(
        app.coordination
            .admit_session_run_activity(
                &id,
                "agent",
                &run_id,
                SessionRunActivityAdmissionMode::RecoverOnly
            )
            .await
            .expect("Z2/O1 cancellation repair decision"),
        SessionRunActivityAdmission::Rejected,
        "Z2/O1 a never-admitted reservation is releasable"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        after_failure,
        "Z2/O1 the release decision mutates no root"
    );
    check_zero_execution(&runtime.counts()).expect("Z2/O1 repair decision executes nothing");

    // Z3: the original caller retries the SAME operation; the durable
    // reservation is replayed (not a second Run) and gets exactly one record.
    runtime.already_reserved.store(true, Ordering::SeqCst);
    let admitted = app
        .run_admissions
        .admit_session_run(command.clone())
        .await
        .expect("Z3 retry admits the original reservation");
    let AdmittedSessionRun::AlreadyReserved(delivery) = &admitted else {
        panic!("Z3 the retry adopts the existing reservation: {admitted:?}")
    };
    assert_eq!(delivery.run_id, run_id, "Z3 same Run identity");
    let epoch = delivery.session_activity_epoch;
    assert!(epoch > 0, "Z3 non-zero activity epoch");
    {
        let reservations = runtime.reservations.lock().unwrap();
        assert_eq!(reservations.len(), 2, "Z3 replayed reservation call");
        assert!(
            reservations
                .iter()
                .all(|reserved| reserved.run_id == run_id),
            "Z3 no second Run identity"
        );
    }
    let admitted_root = durable.get(&id).await.unwrap();
    assert_eq!(
        activity_records(&admitted_root),
        vec![(operation.clone(), epoch, run_id.0.clone())],
        "Z3 exactly one activity admission record"
    );
    // Z4: admitted but activation was never called.
    check_zero_execution(&runtime.counts()).expect("Z4/O0 admitted-but-inactive executes nothing");
    for mode in [
        SessionRunActivityAdmissionMode::RecoverOnly,
        SessionRunActivityAdmissionMode::RecoverOrAdmit,
    ] {
        assert_eq!(
            app.coordination
                .admit_session_run_activity(&id, "agent", &run_id, mode)
                .await
                .expect("Z4/O1 repair decision"),
            SessionRunActivityAdmission::Admitted {
                session_activity_epoch: epoch
            },
            "Z4/O1 {mode:?} must settle the committed epoch, never delete or re-admit it"
        );
    }
    assert_eq!(
        activity_records(&durable.get(&id).await.unwrap()),
        vec![(operation, epoch, run_id.0.clone())],
        "Z4 repair opens no second record"
    );
    check_zero_execution(&runtime.counts()).expect("Z4/O0 still nothing executed");

    // Only activation publishes the delivery, with the original epoch.
    app.run_admissions
        .activate_admitted_session_run(admitted)
        .await
        .expect("activation after admission");
    let activated = runtime.counts();
    assert_eq!(activated.activations, 1, "activation is the first effect");
    assert_eq!(
        runtime.deliveries.lock().unwrap()[0].session_activity_epoch,
        epoch,
        "activation carries the admitted epoch"
    );
    assert_eq!(
        activated.model_calls, 0,
        "the application never runs inline"
    );
    assert_eq!(
        activated.tool_calls, 0,
        "the application never runs tools inline"
    );
}

#[tokio::test]
async fn return_code_alone_cannot_sign_off_zero_execution() {
    // Negative control: an executor that runs the model although activation
    // never happened. The admission still returns the same error, so a
    // return-code oracle would sign off; the counter oracle must reject it.
    let session = "t07-negative-control";
    let Fixture {
        faulting,
        runtime,
        app,
        ..
    } = fixture(session).await;
    faulting.fail_once(ACTIVITY_CAS);
    let result = app
        .run_admissions
        .admit_session_run(admit_command(session, "t07-nc-operation"))
        .await;
    let return_code_oracle = result.is_err();
    assert!(return_code_oracle, "NC the admission is refused");
    check_zero_execution(&runtime.counts()).expect("NC baseline: no leak yet");

    // Mutant: a leaked inline model execution behind the failed admission.
    awaken_session_execution_contract::SessionRuns::run(
        runtime.as_ref(),
        "agent",
        session,
        vec![awaken_agent_contract::agent::content::ContentBlock::text(
            "leaked",
        )],
    )
    .await
    .expect("mutant executes");
    assert!(
        return_code_oracle,
        "NC the return code is unchanged by the leak"
    );
    let detected = check_zero_execution(&runtime.counts())
        .expect_err("NC the counter oracle detects the leaked model call");
    assert!(detected.contains("model_calls: 1"), "NC {detected}");
}

/// The activity CAS failed after the reservation became durable: the outcome
/// is undecided, not refused. The caller gets the stable pending class, and
/// the reservation repair owns settlement: it admits the exact Run once and
/// never a second Run or epoch.
#[tokio::test]
async fn failed_activity_cas_reports_a_pending_admission_that_repair_settles_once() {
    let session = "t07-pending-reservation";
    let id = SessionId::from(session);
    let Fixture {
        durable,
        faulting,
        runtime,
        app,
    } = fixture(session).await;
    let command = admit_command(session, "t07-pending-operation");
    let run_id = command.run_id.clone();
    let operation =
        awaken_session_execution_contract::session_run_activity_operation_id(&id, &run_id);
    faulting.fail_once(ACTIVITY_CAS);
    let error = app
        .run_admissions
        .admit_session_run(command)
        .await
        .expect_err("activity CAS failure is reported to the caller");
    assert_eq!(
        (error.kind, error.code.as_str()),
        (
            awaken_runtime_contract::runs::RunErrorKind::Unavailable,
            "session_run_admission_pending"
        ),
        "the caller learns the admission is pending, never that it failed: {error:?}"
    );
    check_zero_execution(&runtime.counts()).expect("a pending admission executes nothing");
    let mut epochs = Vec::new();
    for _ in 0..2 {
        let repair = app
            .coordination
            .admit_session_run_activity(
                &id,
                "agent",
                &run_id,
                SessionRunActivityAdmissionMode::RecoverOrAdmit,
            )
            .await
            .expect("repair decision");
        let SessionRunActivityAdmission::Admitted {
            session_activity_epoch,
        } = repair
        else {
            panic!("repair settles the pending reservation: {repair:?}")
        };
        epochs.push(session_activity_epoch);
    }
    assert_eq!(epochs[0], epochs[1], "repair admits exactly once");
    assert_eq!(
        activity_records(&durable.get(&id).await.unwrap()),
        vec![(operation, epochs[0], run_id.0.clone())],
        "one activity record for the original Run"
    );
    check_zero_execution(&runtime.counts()).expect("repair activates nothing");
}
