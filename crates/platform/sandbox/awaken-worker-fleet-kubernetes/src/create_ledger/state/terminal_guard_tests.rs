//! Decision table for retaining original creation during terminal reisolation.
use super::*;

#[test]
fn historical_created_pod_requires_exact_absence_and_retains_both_incarnations() {
    // C1 original Created ticket; C2 Worker/group closed; C3 trusted provider
    // observed its exact original UID absent; C4 isolation response retained or
    // lost; C5 a late Worker completion races the terminal owner.
    // | Rule | C2 | C3 | result |
    // | H0 | false | any | reject, unchanged |
    // | H1 | true | missing/foreign | no new effect |
    // | H2 | true | exact | durable pending marker, join Unknown |
    // | H3 | true | H2 + Isolated | retain original UID and new guard UID |
    // | H4 | true | H3 + replay | unchanged, original nonce retained |
    // | H5 | true | H3 + late Worker/forged result | reject, unchanged |
    // Provider 404/policy/UID races are tested through the shared API fixture;
    // this pure state table itself grants no physical absence or disposal.
    let ticket = crate::test_support::isolation_ticket();
    let request = ticket.request;
    let nonce = ticket.isolation_id;
    let mut state = WorkerCreateLedgerState::default();
    state.admit(&request, &nonce).unwrap();
    let original = TicketResult::Created {
        uid: "original-uid".into(),
    };
    state.finish(&request, original.clone()).unwrap();
    let before = state.clone();
    assert!(
        state
            .admit_terminal_pod_guard(&request, &nonce, Some("original-uid"))
            .is_err(),
        "H0"
    );
    assert_eq!(state, before);
    state.seal_worker();
    state.seal_group(&request.group).unwrap();
    for asserted in [None, Some("foreign-uid")] {
        let before = state.clone();
        assert!(
            !state
                .admit_terminal_pod_guard(&request, &nonce, asserted)
                .unwrap(),
            "H1"
        );
        assert_eq!(state, before);
    }
    assert!(
        state
            .admit_terminal_pod_guard(&request, &"b".repeat(32), Some("original-uid"))
            .unwrap(),
        "H2"
    );
    assert_eq!(state.ticket(&request).unwrap().isolation_id, nonce);
    assert_eq!(
        state.ticket(&request).unwrap().status,
        TicketStatus::Finished(original.clone())
    );
    assert_eq!(state.join_group(&request.group), RequestJoin::Unknown);
    state = WorkerCreateLedgerState::decode(&state.encode().unwrap()).unwrap();
    assert!(
        !state
            .admit_terminal_pod_guard(&request, &nonce, Some("original-uid"))
            .unwrap(),
        "H2 cold replay"
    );
    assert!(
        state
            .finish_isolation(
                &request,
                TicketResult::Isolated {
                    uid: "reservation-uid".into()
                }
            )
            .unwrap(),
        "H3"
    );
    let retained = TicketResult::CreatedThenIsolated {
        created_uid: "original-uid".into(),
        uid: "reservation-uid".into(),
    };
    assert_eq!(
        state.ticket(&request).unwrap().status,
        TicketStatus::Finished(retained.clone())
    );
    assert_eq!(retained.isolated_uid(), Some("reservation-uid"));
    let before = state.clone();
    assert!(
        !state
            .finish_isolation(
                &request,
                TicketResult::Isolated {
                    uid: "reservation-uid".into()
                }
            )
            .unwrap(),
        "H4"
    );
    assert!(state.finish(&request, original).is_err(), "H5 late Worker");
    assert!(
        state
            .finish_isolation(
                &request,
                TicketResult::CreatedThenIsolated {
                    created_uid: "forged".into(),
                    uid: "reservation-uid".into()
                }
            )
            .is_err(),
        "H5 raw forged history"
    );
    assert_eq!(state, before);
    assert_eq!(
        WorkerCreateLedgerState::decode(&state.encode().unwrap()).unwrap(),
        state
    );
}
