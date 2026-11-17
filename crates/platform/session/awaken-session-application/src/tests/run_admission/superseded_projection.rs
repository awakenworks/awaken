//! Session Run admission decides a superseded projection again.
//!
//! A registered Worker settles a finished Run (activity close, usage, idle)
//! after the Thread's terminal events, so the next send can recover its
//! Runtime projection at root R while that settlement moves the root to R+1
//! before the next Run's activity CAS. The CAS loses before any effect ran;
//! the admission that recovered the projection recovers it again and retries
//! the same operation, within a bound.
//!
//! The cut is the real activity CAS of the admitted operation (exact receipt
//! key), held by the existing mutation pause; the concurrent change is a real
//! activity settlement through the production lifecycle, and the CAS loses
//! against the real SQLite root revision.
//!
//! | Row | Concurrent root change between projection read and activity CAS | Caller sees | Root |
//! |---|---|---|---|
//! | S1 | once: the prior Run's activity settles | `Reserved(e)`, e follows the settled root | one record e for the Run |
//! | S2 | before every bounded attempt | `Err` `session_run_admission_pending` | no record or epoch for the Run |
//!
//! Both rows reserve once, execute nothing, and S2 retries exactly
//! `SUPERSEDED_PROJECTION_ATTEMPTS` times.
use super::pre_activation::{
    Fixture, activity_records, admit_command, check_zero_execution, fixture,
};
use super::*;
use crate::run_admission::SUPERSEDED_PROJECTION_ATTEMPTS;

const CUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The exact activity receipt key of `operation`'s Run on `session`.
fn activity_cas_key(session: &str, command: &AdmitSessionRun) -> (String, String) {
    let operation = awaken_session_execution_contract::session_run_activity_operation_id(
        &SessionId::from(session),
        &command.run_id,
    );
    let key = crate::SessionActivities::activity_operation_record(session, &operation).key;
    (operation, key)
}

#[tokio::test]
async fn a_root_settled_between_projection_and_activity_cas_is_decided_again() {
    let session = "superseded-projection-once";
    let id = SessionId::from(session);
    let Fixture {
        durable,
        faulting,
        runtime,
        app,
    } = fixture(session).await;
    // The prior Run's activity is still open when the next send arrives.
    let (_, prior) = app
        .activities
        .begin_activity_for_operation(&id, "prior-run-activity")
        .await
        .expect("S1 prior Run activity");
    let command = admit_command(session, "next-send");
    let (operation, key) = activity_cas_key(session, &command);

    let pause = faulting.pause_next_mutation(&key);
    let admission = {
        let app = app.clone();
        let command = command.clone();
        tokio::spawn(async move { app.run_admissions.admit_session_run(command).await })
    };
    tokio::time::timeout(CUT, pause.entered.notified())
        .await
        .expect("S1 the admission reaches its activity CAS");
    let projected = durable.get(&id).await.unwrap().revision;
    assert_eq!(
        pause
            .candidate
            .lock()
            .unwrap()
            .as_ref()
            .expect("S1 held CAS")
            .expected_revision,
        projected,
        "S1 the held CAS expects the projected root"
    );

    // The prior Run settles concurrently: the root advances before the CAS.
    let settled = app
        .lifecycle()
        .settle_activity_observed(&id, prior, None)
        .await
        .expect("S1 the prior Run settles");
    assert!(settled.revision > projected, "S1 the root advanced");
    assert!(settled.active_activity_epochs.is_empty(), "S1 prior closed");
    pause.release.notify_one();

    let admitted = tokio::time::timeout(CUT, admission)
        .await
        .expect("S1 admission completes")
        .expect("S1 admission task")
        .expect("S1 a superseded projection is decided again, not reported pending");
    let AdmittedSessionRun::Reserved(delivery) = &admitted else {
        panic!("S1 the original reservation is admitted: {admitted:?}");
    };
    let epoch = delivery.session_activity_epoch;
    assert_eq!(
        epoch,
        settled.revision.0 + 1,
        "S1 the epoch is committed on the settled root, never the projected one"
    );
    let root = durable.get(&id).await.unwrap();
    assert_eq!(root.revision.0, epoch, "S1 one activity CAS");
    assert_eq!(
        root.active_activity_epochs,
        std::collections::BTreeSet::from([epoch]),
        "S1 only this Run is active"
    );
    assert_eq!(
        activity_records(&root)
            .into_iter()
            .filter(|(_, _, run)| run == &command.run_id.0)
            .collect::<Vec<_>>(),
        vec![(operation, epoch, command.run_id.0.clone())],
        "S1 exactly one record for the Run"
    );
    let counts = runtime.counts();
    assert_eq!(counts.reservations, 1, "S1 the reservation is not repeated");
    check_zero_execution(&counts).expect("S1 admission executes nothing");
}

#[tokio::test]
async fn a_root_that_keeps_moving_stays_pending_after_bounded_attempts() {
    let session = "superseded-projection-always";
    let id = SessionId::from(session);
    let Fixture {
        durable,
        faulting,
        runtime,
        app,
    } = fixture(session).await;
    let command = admit_command(session, "next-send");
    let (operation, key) = activity_cas_key(session, &command);

    let mut pause = faulting.pause_next_mutation(&key);
    let mut admission = {
        let app = app.clone();
        let command = command.clone();
        tokio::spawn(async move { app.run_admissions.admit_session_run(command).await })
    };
    let mut supersessions = 0;
    let result = loop {
        tokio::select! {
            () = pause.entered.notified() => {}
            result = &mut admission => break result.expect("S2 admission task"),
            () = tokio::time::sleep(CUT) => panic!("S2 neither a CAS nor a result"),
        }
        // Another activity opens and settles while this CAS is held.
        let (_, concurrent) = app
            .activities
            .begin_activity_for_operation(&id, &format!("concurrent-{supersessions}"))
            .await
            .expect("S2 concurrent activity");
        app.lifecycle()
            .settle_activity_observed(&id, concurrent, None)
            .await
            .expect("S2 concurrent settlement");
        supersessions += 1;
        let held = std::mem::replace(&mut pause, faulting.pause_next_mutation(&key));
        held.release.notify_one();
    };

    assert_eq!(
        supersessions, SUPERSEDED_PROJECTION_ATTEMPTS,
        "S2 every bounded attempt reached its own CAS"
    );
    let error = result.expect_err("S2 a root that never settles stays undecided");
    assert_eq!(
        (error.kind, error.code.as_str()),
        (
            awaken_runtime_contract::runs::RunErrorKind::Unavailable,
            "session_run_admission_pending"
        ),
        "S2 ADR-0094 D3 pending: {error:?}"
    );
    let root = durable.get(&id).await.unwrap();
    assert!(
        activity_records(&root)
            .iter()
            .all(|(recorded, _, _)| recorded != &operation),
        "S2 no record for the Run"
    );
    assert!(root.active_activity_epochs.is_empty(), "S2 no open epoch");
    let counts = runtime.counts();
    assert_eq!(counts.reservations, 1, "S2 the reservation is not repeated");
    check_zero_execution(&counts).expect("S2 admission executes nothing");
}
