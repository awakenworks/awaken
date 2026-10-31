//! T07-C02 (丢响应重放): a Run admission whose committed activity CAS lost
//! its response is retried AFTER a newer, independent activity epoch exists.
//! The retry must return the original reservation, Run and root receipt.
//!
//! Oracles (groups JSON order):
//! - O0 same reservation, Run and original receipt;
//! - O1 no second admission;
//! - O2 the root is the only epoch authority.
//!
//! Negative control: "a newer independent epoch overrides the original root
//! receipt" must fail. It is executed twice: (a) a real newer epoch (op-b)
//! is committed before the retry, so a "latest active epoch" implementation
//! would answer op-b's epoch; (b) the same `replays_root_receipt` checker is
//! run against a delivery carrying op-b's epoch and must reject it.
//!
//! | Row | Step | Expected |
//! |---|---|---|
//! | L1 | admit op-a, CAS commits, response lost | Err; root has one op-a record (epoch A) |
//! | L2 | open independent op-b | epoch B > A |
//! | L3 | retry op-a | delivery epoch A, same Run, reservations equal the original command |
//! | NC | delivery with epoch B | rejected by the L3 checker |
use super::*;

/// O0/O2 checker: the delivered epoch is exactly the root's run-activity
/// record for this Run; no other epoch source may answer.
fn replays_root_receipt(delivered: &SessionRunDelivery, root: &PersistedSession) -> bool {
    root.run_activities()
        .map(|(records, _)| {
            records.iter().any(|record| {
                record.run_id() == &delivered.run_id
                    && record.activity_epoch().get() == delivered.session_activity_epoch
            })
        })
        .unwrap_or(false)
}

fn delivery(admitted: &AdmittedSessionRun) -> SessionRunDelivery {
    admitted
        .delivery()
        .cloned()
        .unwrap_or_else(|| panic!("admission without a delivery: {admitted:?}"))
}

#[tokio::test]
async fn lost_response_retry_keeps_the_root_receipt_over_a_newer_independent_epoch() {
    let session = "t07-c02";
    let fixture = fixture(&[session]).await;
    let original = admit_command(session, "op-a");

    // L1: the committed admission's response is lost.
    fixture.loss.commit_then_unavailable_once(ACTIVITY_CAS);
    let lost = fixture.admit(session, "op-a").await;
    assert!(lost.is_err(), "L1 the caller sees an unknown outcome");
    let (run_a, epoch_a) =
        single_admission(&fixture.root(session).await).expect("L1 one durable admission");
    assert_eq!(run_a, original.run_id.0, "L1 original Run identity");

    // L2: a newer, independent activity epoch commits on the same root.
    let (_, epoch_b) = fixture
        .app
        .activities
        .begin_activity_for_operation(&SessionId::from(session), "op-b")
        .await
        .expect("L2 independent operation opens its own epoch");
    assert!(epoch_b > epoch_a, "L2 the independent epoch is newer");
    let before_retry = fixture.root(session).await;
    assert!(
        before_retry.active_activity_epochs.contains(&epoch_b),
        "L2 newer epoch is active"
    );

    // L3: the exact retry of op-a.
    let replay = fixture
        .retry(session, "op-a")
        .await
        .expect("L3 exact retry");
    let delivered = delivery(&replay);
    let root = fixture.root(session).await;
    assert_eq!(
        delivered.session_activity_epoch, epoch_a,
        "L3/O0 original epoch"
    );
    assert_ne!(
        delivered.session_activity_epoch, epoch_b,
        "L3/O2 not the newer epoch"
    );
    assert_eq!(delivered.run_id, original.run_id, "L3/O0 same Run");
    assert!(
        replays_root_receipt(&delivered, &root),
        "L3/O2 root receipt"
    );
    assert_eq!(
        single_admission(&root),
        Ok((original.run_id.0.clone(), epoch_a)),
        "L3/O1 no second admission"
    );
    let reservations = fixture.runtime.reservations.lock().unwrap().clone();
    assert!(
        !reservations.is_empty() && reservations.iter().all(|command| command == &original),
        "L3/O0 every reservation is the original command: {reservations:?}"
    );
    assert_eq!(
        root.active_activity_epochs, before_retry.active_activity_epochs,
        "L3/O1 the retry opened no activity"
    );
    assert_eq!(
        fixture.runtime.counts().activations,
        0,
        "admission activates nothing"
    );

    // NC: a delivery answered from the newer independent epoch.
    let mutant = SessionRunDelivery {
        session_activity_epoch: epoch_b,
        ..delivered.clone()
    };
    assert!(
        !replays_root_receipt(&mutant, &root),
        "NC a newer independent epoch cannot stand in for the root receipt"
    );
}
