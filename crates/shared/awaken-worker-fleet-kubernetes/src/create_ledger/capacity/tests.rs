use super::*;
use awaken_container_contract::{CreateTicketRequest, TicketAdmission, TicketResult};
use k8s_openapi::{ByteString, apimachinery::pkg::apis::meta::v1::ObjectMeta};
use std::collections::BTreeMap;

const NONCE: &str = "0123456789abcdef0123456789abcdef";
fn object(state: &WorkerCreateLedgerState) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some("ledger".into()),
            namespace: Some("control".into()),
            uid: Some("original".into()),
            resource_version: Some("1".into()),
            ..Default::default()
        },
        data: Some(BTreeMap::from([("ledger".into(), state.encode().unwrap())])),
        ..Default::default()
    }
}
fn request(index: usize) -> CreateTicketRequest {
    CreateTicketRequest::new(
        "worker",
        "pods",
        "ns",
        format!("pod-{index}"),
        "a".repeat(64),
    )
    .unwrap()
}

#[test]
fn complete_configmap_capacity_counts_unknown_content_and_outer_escaping() {
    // C1-C6/C14: independent data/binary value, annotation key+value and
    // complete PUT limits; exact equality fits, one more fails. Count every
    // unknown map key and actual binding. Outer nested JSON can overflow
    // while value sum still fits. Legacy canonical defaults are counted by
    // the proposed full object, never old-length+new-delta.
    let legacy = WorkerCreateLedgerState::default();
    let mut cm = object(&legacy);
    let ledger_len = cm.data.as_ref().unwrap()["ledger"].len();
    cm.binary_data = Some(BTreeMap::from([(
        "unknown".into(),
        ByteString(vec![0; CONFIGMAP_DATA_BYTES - ledger_len]),
    )]));
    validate_candidate(&cm, &legacy).unwrap();
    cm.binary_data
        .as_mut()
        .unwrap()
        .get_mut("unknown")
        .unwrap()
        .0
        .push(0);
    assert!(validate_candidate(&cm, &legacy).is_err());
    cm.binary_data = None;
    cm.metadata.annotations = Some(BTreeMap::from([(
        "x".into(),
        "a".repeat(ANNOTATION_BYTES - 1),
    )]));
    validate_candidate(&cm, &legacy).unwrap();
    cm.metadata
        .annotations
        .as_mut()
        .unwrap()
        .get_mut("x")
        .unwrap()
        .push('a');
    assert!(validate_candidate(&cm, &legacy).is_err());
    cm.metadata.annotations = None;
    cm.data.as_mut().unwrap().insert(
        "worker_process_binding".into(),
        "a".repeat(CONFIGMAP_DATA_BYTES - ledger_len),
    );
    validate_candidate(&cm, &legacy).unwrap();
    cm.data
        .as_mut()
        .unwrap()
        .get_mut("worker_process_binding")
        .unwrap()
        .push('a');
    assert!(validate_candidate(&cm, &legacy).is_err());
    let mut escaped = object(&legacy);
    escaped.data.as_mut().unwrap().insert(
        "other".into(),
        "\0".repeat(CONFIGMAP_DATA_BYTES - ledger_len),
    );
    assert!(validate_observed(&escaped).is_ok());
    assert!(
        validate_candidate(&escaped, &legacy).is_err(),
        "full wire cannot borrow data limit"
    );
    assert!(add(usize::MAX, 1).is_err());
}

#[test]
fn ordinary_fixture_admissions_stop_before_the_full_future_capture_boundary() {
    // C1/C7/C12/R2/R6: normal small Pod requests, fixed actual binding,
    // same group. Fill with original admissions until the first entire
    // next+future envelope is refused. Every accepted request can then
    // capture its maximum result and seal, not merely today's small UID.
    // This fixture exposes the <128 worst-case outstanding cost without
    // altering Worker concurrency/retry/CI resource budgets.
    let mut state = WorkerCreateLedgerState::fresh();
    let mut count = 0;
    loop {
        let mut candidate = state.clone();
        assert_eq!(
            candidate.admit(&request(count), NONCE).unwrap(),
            TicketAdmission::Forward
        );
        let mut cm = object(&candidate);
        cm.data.as_mut().unwrap().insert(
            "worker_process_binding".into(),
            "retained-binding".repeat(64),
        );
        if validate_candidate(&cm, &candidate).is_err() {
            break;
        }
        state = candidate;
        count += 1;
    }
    assert!(
        count > 100 && count < 128,
        "ordinary fixture retained {count} admissions"
    );
    let overhead = serde_json::to_vec(&TicketResult::Created { uid: String::new() })
        .unwrap()
        .len();
    let maximum = TicketResult::Created {
        uid: "\\".repeat((super::super::capture::RESULT_BYTES - overhead) / 2),
    };
    for index in 0..count {
        state.finish(&request(index), maximum.clone()).unwrap();
    }
    state.seal_group("worker").unwrap();
    state.seal_worker();
    let mut final_cm = object(&state);
    final_cm.data.as_mut().unwrap().insert(
        "worker_process_binding".into(),
        "retained-binding".repeat(64),
    );
    validate_candidate(&final_cm, &state).unwrap();
    println!("finite-policy ordinary fixture admissions={count}");
}
