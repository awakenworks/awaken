use super::*;

fn binding<'a>(
    ledger: &'a K8sWorkerCreateLedger,
    name: &'a str,
    uid: &'a str,
) -> K8sLedgerBinding<'a> {
    K8sLedgerBinding {
        worker_namespace: "awaken-system",
        worker_name: "worker",
        worker_uid: "pod-uid",
        ledger_name: name,
        ledger_uid: uid,
        configuration_fingerprint: ledger.configuration_fingerprint(),
    }
}
fn request(resource: &str, name: &str) -> CreateTicketRequest {
    CreateTicketRequest::new(
        "worker-group",
        resource,
        "awaken-system",
        name,
        "a".repeat(64),
    )
    .unwrap()
}

#[tokio::test]
async fn legacy_physical_ledger_replays_exact_history_without_new_reserve_admission() {
    // V5/V6/C6/G7: registration409+GET finds the exact old UID and never
    // upgrades policy. Old completed >R UID remains decode/replayable;
    // original InFlight stays unproved, new request cannot Forward. Missing
    // old nonce remains empty. No replacement ledger or new result proof.
    let Fixture {
        ledger,
        stored,
        calls,
        ..
    } = fixture(false);
    let (name, uid) = ledger
        .initialize("awaken-system", "worker", "pod-uid")
        .await
        .unwrap();
    let req = request("pods", "original");
    let pending = request("pods", "pending");
    let old_uid = "u".repeat(capture::RESULT_BYTES + 1);
    let mut old = WorkerCreateLedgerState::default();
    old.admit(&req, "a".repeat(32).as_str()).unwrap();
    old.finish(
        &req,
        TicketResult::Created {
            uid: old_uid.clone(),
        },
    )
    .unwrap();
    old.admit(&pending, "a".repeat(32).as_str()).unwrap();
    let mut historical: Value = serde_json::from_str(&old.encode().unwrap()).unwrap();
    for ticket in historical["tickets"].as_object_mut().unwrap().values_mut() {
        for field in ["origin", "isolation_id", "isolation_pending"] {
            ticket.as_object_mut().unwrap().remove(field);
        }
    }
    stored.lock().unwrap().as_mut().unwrap()["data"]["ledger"] = historical.to_string().into();
    assert_eq!(
        ledger
            .initialize("awaken-system", "worker", "pod-uid")
            .await
            .unwrap(),
        (name.clone(), uid.clone())
    );
    let writes = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|method| *method == "PUT")
        .count();
    let exact = binding(&ledger, &name, &uid);
    assert_eq!(
        ledger.admit_request(exact, &req).await.unwrap(),
        (
            TicketAdmission::Finished(TicketResult::Created { uid: old_uid }),
            CreateReplyProfile::LegacyRetainedCreate
        )
    );
    assert_eq!(
        ledger.admit_request(exact, &pending).await.unwrap(),
        (
            TicketAdmission::InFlight,
            CreateReplyProfile::LegacyRetainedCreate
        )
    );
    assert!(
        ledger
            .admit_request(exact, &request("pods", "new"))
            .await
            .is_err()
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "PUT")
            .count(),
        writes
    );
    let retained = ledger.ticket_for(exact, &pending).await.unwrap().unwrap();
    assert!(retained.isolation_id.is_empty());
}

#[tokio::test]
async fn fresh_cas_recomputes_conflicts_and_lost_finish_reply_from_original_state() {
    // C12/R6/R7: first CAS conflict grants no Forward; winner keeps one
    // nonce. Finish commits then loses response: caller receives uncertainty,
    // cold adapter reads original Finished exact full UID with zero extra
    // mutation or new admission. Same result replay does not rewrite history.
    let Fixture {
        ledger,
        stored,
        calls,
        conflict,
        lose_finish_reply,
        ..
    } = fixture(false);
    let (name, uid) = ledger
        .initialize("awaken-system", "worker", "pod-uid")
        .await
        .unwrap();
    let exact = binding(&ledger, &name, &uid);
    let req = request("pods", "original");
    conflict.store(true, Ordering::SeqCst);
    assert_eq!(
        ledger.admit_request(exact, &req).await.unwrap(),
        (TicketAdmission::Forward, CreateReplyProfile::FreshCreate)
    );
    let nonce = ledger
        .ticket_for(exact, &req)
        .await
        .unwrap()
        .unwrap()
        .isolation_id;
    let result = TicketResult::Created {
        uid: "u".repeat(4097),
    };
    lose_finish_reply.store(true, Ordering::SeqCst);
    assert!(ledger.finish_request(exact, &req, &result).await.is_err());
    let cold = K8sWorkerCreateLedger::new(
        ledger.client.clone(),
        "awaken-control",
        "awaken-system",
        "provider-a",
    )
    .unwrap();
    let before = stored.lock().unwrap().clone();
    let writes = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|method| *method == "PUT")
        .count();
    assert_eq!(
        cold.admit_request(binding(&cold, &name, &uid), &req)
            .await
            .unwrap(),
        (
            TicketAdmission::Finished(result.clone()),
            CreateReplyProfile::FreshCreate
        )
    );
    assert_eq!(
        cold.ticket_for(binding(&cold, &name, &uid), &req)
            .await
            .unwrap()
            .unwrap()
            .isolation_id,
        nonce
    );
    cold.finish_request(binding(&cold, &name, &uid), &req, &result)
        .await
        .unwrap();
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "PUT")
            .count(),
        writes
    );
    assert_eq!(stored.lock().unwrap().clone(), before);
}

#[tokio::test]
async fn metadata_growth_blocks_even_an_unchanged_pending_isolation_admission() {
    // C1/C2/C13/R2/G2: fresh exact request has an admitted nonce; foreign
    // preserved data later consumes the future reserve. Even changed=false
    // pending replay must fail before physical API. R oversize never retains
    // a partial UID. Original pending state survives all these failures.
    let Fixture {
        ledger,
        stored,
        calls,
        ..
    } = fixture(false);
    let (name, uid) = ledger
        .initialize("awaken-system", "worker", "pod-uid")
        .await
        .unwrap();
    let exact = binding(&ledger, &name, &uid);
    let req = request("pods", "original");
    ledger.admit_request(exact, &req).await.unwrap();
    ledger.seal_worker(exact).await.unwrap();
    ledger
        .seal_group_and_join(exact, "worker-group")
        .await
        .unwrap();
    ledger
        .mutate(exact, |state| state.start_isolation(&req))
        .await
        .unwrap();
    assert!(
        ledger
            .finish_request(
                exact,
                &req,
                &TicketResult::Created {
                    uid: "u".repeat(capture::RESULT_BYTES + 1)
                }
            )
            .await
            .is_err()
    );
    {
        let mut current = stored.lock().unwrap();
        let object = current.as_mut().unwrap();
        let ledger_bytes = object["data"]["ledger"].as_str().unwrap().len();
        object["data"]["unrelated"] = "x"
            .repeat(capacity::CONFIGMAP_DATA_BYTES - ledger_bytes)
            .into();
    }
    let writes = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|method| *method == "PUT")
        .count();
    assert!(
        ledger
            .mutate(exact, |state| state.start_isolation(&req))
            .await
            .is_err()
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "PUT")
            .count(),
        writes
    );
    let retained = ledger.ticket_for(exact, &req).await.unwrap().unwrap();
    assert_eq!(retained.status, TicketStatus::InFlight);
    assert!(retained.isolation_pending);
}
