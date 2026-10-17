use super::super::RequestJoin;
use super::*;
use crate::create_ledger::capture::{CreateReplyProfile, validate_fresh_result};

const NONCE: &str = "0123456789abcdef0123456789abcdef";
fn request(resource: &str, name: &str) -> CreateTicketRequest {
    CreateTicketRequest::new("worker-group", resource, "ns", name, "a".repeat(64)).unwrap()
}

#[test]
fn fresh_capture_result_range_preserves_the_intrinsic_uid_law() {
    // R1/R2/V6/V7: raw UID format is unchanged, only fresh original serde
    // bytes are bounded. ASCII4097 retains its old positive partition;
    // controls/quotes/backslashes/UTF8 are counted without normalization.
    // A <=R exact result captures; >R stays InFlight. Legacy completed data
    // >R still decodes/replays; missing policy is not inferred or upgraded.
    for uid in ["u".repeat(4097), "u\0\n\\\"😀".to_owned()] {
        let result = TicketResult::Created { uid: uid.clone() };
        assert!(result.validate().is_ok());
        assert!(validate_fresh_result(&result).is_ok());
        let mut state = WorkerCreateLedgerState::fresh();
        let req = request("pods", "pod");
        state.admit(&req, NONCE).unwrap();
        state.finish(&req, result.clone()).unwrap();
        assert_eq!(
            state.ticket(&req).unwrap().status,
            TicketStatus::Finished(result)
        );
    }
    let overhead = measure(&TicketResult::Created { uid: String::new() })
        .unwrap()
        .0;
    for delta in [0_usize, 1, 2] {
        let bytes = RESULT_BYTES - 1 + delta;
        let result = TicketResult::Created {
            uid: "u".repeat(bytes - overhead),
        };
        assert_eq!(measure(&result).unwrap().0, bytes);
        assert!(result.validate().is_ok());
        let mut state = WorkerCreateLedgerState::fresh();
        let req = request("pods", "pod");
        state.admit(&req, NONCE).unwrap();
        assert_eq!(
            state.finish(&req, result.clone()).is_ok(),
            bytes <= RESULT_BYTES
        );
        if bytes > RESULT_BYTES {
            assert_eq!(state.ticket(&req).unwrap().status, TicketStatus::InFlight);
        }
        let mut legacy = WorkerCreateLedgerState::default();
        legacy.admit(&req, NONCE).unwrap();
        legacy.finish(&req, result.clone()).unwrap();
        let decoded = WorkerCreateLedgerState::decode(&legacy.encode().unwrap()).unwrap();
        assert_eq!(
            decoded.reply_profile(),
            CreateReplyProfile::LegacyRetainedCreate
        );
        assert_eq!(
            decoded.ticket(&req).unwrap().status,
            TicketStatus::Finished(result)
        );
    }
    let mut unknown: serde_json::Value =
        serde_json::from_str(&WorkerCreateLedgerState::fresh().encode().unwrap()).unwrap();
    unknown["capture_policy"] = "future_policy".into();
    assert!(WorkerCreateLedgerState::decode(&unknown.to_string()).is_err());
}

#[test]
fn guard_capacity_counts_each_possible_claim_and_consumes_only_its_target() {
    // C7-C10/G7: two PVCs in one Worker group each reserve a possible missing
    // Pod. A covering original Pod consumes only that target's extra row;
    // Rejected->Isolated still reserves its result. Unknown names reserve
    // maximum escaped row bytes and never gain target/membership authority.
    let mut state = WorkerCreateLedgerState::fresh();
    let first = request("persistentvolumeclaims", "awc-first");
    let second = request("persistentvolumeclaims", "awc-second");
    state.admit(&first, NONCE).unwrap();
    let one = future_capacity(&state).unwrap();
    state.admit(&second, NONCE).unwrap();
    let two = future_capacity(&state).unwrap();
    assert!(two.data_growth > one.data_growth);
    for req in [&first, &second] {
        state
            .finish(
                req,
                TicketResult::Created {
                    uid: "claim-uid".into(),
                },
            )
            .unwrap();
    }
    state.seal_worker();
    state.seal_group("worker-group").unwrap();
    let before = future_capacity(&state).unwrap();
    let guard = request("pods", "awaken-first");
    state.admit_terminal_pod_guard(&guard, NONCE).unwrap();
    let pending = future_capacity(&state).unwrap();
    assert!(pending.data_growth < before.data_growth);
    state.start_isolation(&guard).unwrap();
    state
        .finish_isolation(
            &guard,
            TicketResult::Isolated {
                uid: "guard-uid".into(),
            },
        )
        .unwrap();
    let settled = future_capacity(&state).unwrap();
    assert!(
        settled.data_growth > 0,
        "second claim guard obligation retained"
    );
    assert!(settled.data_growth < pending.data_growth);
    let mut unknown = WorkerCreateLedgerState::fresh();
    unknown
        .admit(&request("persistentvolumeclaims", "not-canonical"), NONCE)
        .unwrap();
    assert!(future_capacity(&unknown).unwrap().data_growth > one.data_growth);
    let mut quantity = WorkerCreateLedgerState::fresh();
    for index in 0..MAX_TICKETS {
        quantity
            .admit(&request("pods", &format!("pod-{index}")), NONCE)
            .unwrap();
    }
    let claim = request("persistentvolumeclaims", "awc-last");
    quantity.tickets.remove(&request("pods", "pod-0").key());
    quantity.admit(&claim, NONCE).unwrap();
    assert!(
        future_capacity(&quantity).is_err(),
        "final ordinary row must not steal guard slot"
    );
}

#[test]
fn future_projection_retains_seal_and_pending_clear_growth() {
    // C9/C11/C13/G1-G3: original rejection while an isolation POST remains
    // pending is not closure. pending true->false adds a byte, all Open
    // groups may seal, and unchanged pending replay still needs the same
    // remaining result envelope. Exact settled result reduces only its own
    // obligations; legacy profiles receive no manufactured reserve.
    let req = request("pods", "pod");
    let mut state = WorkerCreateLedgerState::fresh();
    state.admit(&req, NONCE).unwrap();
    let open = future_capacity(&state).unwrap();
    state.seal_worker();
    state.seal_group("worker-group").unwrap();
    let sealed = future_capacity(&state).unwrap();
    assert_eq!(open.data_growth, sealed.data_growth + 2);
    state.start_isolation(&req).unwrap();
    let pending = future_capacity(&state).unwrap();
    assert_eq!(state.start_isolation(&req).unwrap(), (true, false));
    assert_eq!(
        future_capacity(&state).unwrap().data_growth,
        pending.data_growth
    );
    state
        .finish(&req, TicketResult::RejectedNoEffect { code: 409 })
        .unwrap();
    assert_eq!(state.join_group("worker-group"), RequestJoin::Unknown);
    assert!(future_capacity(&state).unwrap().data_growth > 0);
    state
        .finish_isolation(
            &req,
            TicketResult::Isolated {
                uid: "reserved".into(),
            },
        )
        .unwrap();
    assert_eq!(future_capacity(&state).unwrap().data_growth, 0);
    let legacy = WorkerCreateLedgerState::default();
    assert_eq!(future_capacity(&legacy).unwrap().data_growth, 0);
}

#[test]
fn whole_historical_join_projection_fits_without_a_fresh_uid_cap() {
    // R10/C6/V6: observed original CM ledger data is <=1MiB. The only
    // historical implicit ticket fields are origin/isolation_id/pending.
    // Adding their exact serialized object content+one connecting comma
    // costs defaults.len()-1 per ticket. Complete Vec removes64hex map
    // keys/groups/root headers, so a conservative upper bound is input
    // ledger bytes +2048*(defaults.len()-1)+empty Complete wrapper <4MiB.
    // No R*2048 assumption. Check MAX_TICKETS, escaping, absent defaults,
    // and a >R original UID; oversized standalone pages reject explicitly.
    let mut legacy = WorkerCreateLedgerState::default();
    for index in 0..MAX_TICKETS {
        let req = request("pods", &format!("pod-{index}"));
        legacy.admit(&req, NONCE).unwrap();
        legacy
            .finish(
                &req,
                TicketResult::Created {
                    uid: if index == 0 {
                        "u".repeat(RESULT_BYTES + 1)
                    } else {
                        "u\0\\\"😀".into()
                    },
                },
            )
            .unwrap();
    }
    legacy.seal_worker();
    legacy.seal_group("worker-group").unwrap();
    let mut historical: serde_json::Value =
        serde_json::from_str(&legacy.encode().unwrap()).unwrap();
    for ticket in historical["tickets"].as_object_mut().unwrap().values_mut() {
        for field in ["origin", "isolation_id", "isolation_pending"] {
            ticket.as_object_mut().unwrap().remove(field);
        }
    }
    let input = historical.to_string();
    assert!(input.len() <= super::super::super::capacity::CONFIGMAP_DATA_BYTES);
    let decoded = WorkerCreateLedgerState::decode(&input).unwrap();
    let joined = decoded.join_sealed_worker_group("worker-group");
    let wire = CreateReplyProfile::WholeJoin.encode_body(&joined).unwrap();
    let defaults = serde_json::to_vec(
        &serde_json::json!({"origin":"worker","isolation_id":"","isolation_pending":false}),
    )
    .unwrap();
    let upper = input
        .len()
        .checked_add(MAX_TICKETS.checked_mul(defaults.len() - 1).unwrap())
        .unwrap()
        .checked_add(
            serde_json::to_vec(&RequestJoin::Complete(Vec::new()))
                .unwrap()
                .len(),
        )
        .unwrap();
    assert!(wire.len() <= upper);
    assert!(
        super::super::super::capacity::CONFIGMAP_DATA_BYTES
            + MAX_TICKETS * (defaults.len() - 1)
            + 15
            < CreateReplyProfile::WholeJoin.body_limit().unwrap()
    );
    let RequestJoin::Complete(tickets) = &joined else {
        panic!("complete original set");
    };
    assert_eq!(tickets.len(), MAX_TICKETS);
    assert!(tickets.iter().any(|ticket| matches!(&ticket.status, TicketStatus::Finished(TicketResult::Created { uid }) if uid.len() > RESULT_BYTES)));
    let mut oversized = tickets[0].clone();
    oversized.status = TicketStatus::Finished(TicketResult::Created {
        uid: "u".repeat(CreateReplyProfile::WholeJoin.body_limit().unwrap()),
    });
    assert!(
        CreateReplyProfile::WholeJoin
            .encode_body(&RequestJoin::Complete(vec![oversized]))
            .is_err()
    );
}
