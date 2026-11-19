//! Original-source terminal isolation through the shared ledger/API fixtures.
use super::*;
use awaken_container_contract::{TicketOrigin, TicketStatus};

#[tokio::test]
async fn terminal_claim_isolation_is_scoped_and_replays_original_history() {
    // Cause graph: C1 sealed exact Worker/group + finished own PVC; C2 own Pod
    // never admitted / historical Created and now absent / live / foreign;
    // C3 other Session's Pod/PVC tickets; C4 unknown projected participant;
    // C5 current isolation policy; C6 response loss after marker/Pod creation.
    // R1 C1+absent(C2)+C5 -> one exact protected Pod name, no claim deletion.
    // R2 R1+C3 -> same result, whole original ledger retains C3 byte-for-byte.
    // R3 live/foreign C2 or C4 -> no new Pod reservation/disposal.
    // R4 !C1/!C5 -> Unknown; restoring C5 resumes the SAME pending operation.
    // R5 C6 -> no completion on ambiguous response; a cold client joins the
    // original nonce/UID and retains Created history. Repeat returns same UID.
    // Constraints: no Runtime/Session completion or actual DELETE is fabricated.
    for row in [
        "claim-only",
        "other-session",
        "created-gone",
        "created-gone-other",
        "live",
        "foreign",
        "unclassified",
        "worker-open",
        "policy-missing",
        "marker-response-loss",
        "pod-response-loss",
    ] {
        let f = fixture(false);
        let fingerprint = f.ledger.configuration_fingerprint().to_owned();
        let (name, uid) = f
            .ledger
            .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
            .await
            .unwrap();
        let binding = K8sLedgerBinding {
            worker_namespace: "awaken-system",
            worker_name: "worker",
            worker_uid: "pod-uid",
            worker_incarnation: "pod-uid",
            ledger_name: &name,
            ledger_uid: &uid,
            configuration_fingerprint: &fingerprint,
        };
        let group = "closed-original-group";
        let claim = CreateTicketRequest::new(
            group,
            "persistentvolumeclaims",
            "awaken-system",
            "awc-target",
            "a".repeat(64),
        )
        .unwrap();
        f.ledger.admit_request(binding, &claim).await.unwrap();
        f.ledger
            .finish_request(
                binding,
                &claim,
                &TicketResult::Created {
                    uid: "original-claim".into(),
                },
            )
            .await
            .unwrap();
        let has_original = matches!(
            row,
            "created-gone"
                | "created-gone-other"
                | "live"
                | "foreign"
                | "marker-response-loss"
                | "pod-response-loss"
        );
        let mut pod_request = None;
        if has_original {
            let request = CreateTicketRequest::new(
                group,
                "pods",
                "awaken-system",
                "awaken-target",
                "b".repeat(64),
            )
            .unwrap();
            f.ledger.admit_request(binding, &request).await.unwrap();
            f.ledger
                .finish_request(
                    binding,
                    &request,
                    &TicketResult::Created {
                        uid: "original-pod".into(),
                    },
                )
                .await
                .unwrap();
            pod_request = Some(request);
        }
        let has_other = matches!(row, "other-session" | "created-gone-other");
        let mut other_request = None;
        if has_other {
            for (resource, name) in [
                ("persistentvolumeclaims", "awc-other"),
                ("pods", "awaken-other"),
            ] {
                let request = CreateTicketRequest::new(
                    group,
                    resource,
                    "awaken-system",
                    name,
                    "c".repeat(64),
                )
                .unwrap();
                f.ledger.admit_request(binding, &request).await.unwrap();
                f.ledger
                    .finish_request(
                        binding,
                        &request,
                        &TicketResult::Created {
                            uid: format!("other-{resource}"),
                        },
                    )
                    .await
                    .unwrap();
                other_request = Some(request);
            }
        }
        if row == "unclassified" {
            let request = CreateTicketRequest::new(
                group,
                "secrets",
                "awaken-system",
                "unclassified-source",
                "d".repeat(64),
            )
            .unwrap();
            f.ledger.admit_request(binding, &request).await.unwrap();
            f.ledger
                .finish_request(
                    binding,
                    &request,
                    &TicketResult::Created {
                        uid: "unclassified-uid".into(),
                    },
                )
                .await
                .unwrap();
        }
        if matches!(row, "live" | "foreign") {
            f.isolation.lock().pod=Some(serde_json::from_value(json!({"metadata":{"name":"awaken-target","namespace":"awaken-system","uid":if row=="live" {"original-pod"} else {"foreign-pod"}}})).unwrap());
        }
        if row != "worker-open" {
            f.ledger.seal_worker(binding).await.unwrap();
        }
        f.ledger.seal_group_and_join(binding, group).await.unwrap();
        let original_other = if let Some(r) = &other_request {
            f.ledger.ticket_for(binding, r).await.unwrap()
        } else {
            None
        };
        if row == "policy-missing" {
            f.isolation.lock().policy = false;
        }
        if row == "marker-response-loss" {
            f.lose_finish_reply.store(true, Ordering::SeqCst);
        }
        if row == "pod-response-loss" {
            f.isolation.lock().pod_create_response_lost = true;
        }
        let result = f
            .ledger
            .reserve_terminal_claim_pod_and_join(binding, group, "target", "awaken-target")
            .await;
        if row == "worker-open" {
            assert_eq!(result.unwrap(), RequestJoin::Unknown, "R4 {row}");
            assert_eq!(f.isolation.lock().creates, 0);
            continue;
        }
        if matches!(row, "live" | "foreign" | "unclassified") {
            assert!(matches!(result.unwrap(), RequestJoin::Complete(_)), "R3");
            assert_eq!(
                f.isolation.lock().creates,
                0,
                "R3 no guard for a live/unknown source"
            );
            assert_eq!(f.isolation.lock().claim_deletes, 0);
            continue;
        }
        if row == "policy-missing" {
            assert_eq!(result.unwrap(), RequestJoin::Unknown, "R4");
            f.isolation.lock().policy = true;
        } else if matches!(row, "marker-response-loss" | "pod-response-loss") {
            assert!(result.is_err(), "R5 ambiguous reply cannot complete");
        } else {
            assert!(matches!(result.unwrap(), RequestJoin::Complete(_)), "R1/R2");
        }
        let cold = K8sWorkerCreateLedger::new(
            f.ledger.client.clone(),
            "awaken-control",
            "awaken-system",
            "awaken-system",
            "provider-a",
        )
        .unwrap();
        let RequestJoin::Complete(tickets) = cold
            .reserve_terminal_claim_pod_and_join(binding, group, "target", "awaken-target")
            .await
            .unwrap()
        else {
            panic!("R1/R2/R4/R5 resumes same source: {row}")
        };
        assert_eq!(
            tickets.len(),
            2,
            "R2 scoped view excludes other Session: {row}"
        );
        let guard = tickets
            .iter()
            .find(|t| t.request.resource == "pods")
            .unwrap();
        let TicketStatus::Finished(result) = &guard.status else {
            panic!("R1 finished guard")
        };
        assert_eq!(result.isolated_uid(), Some("sentinel-uid"));
        if let Some(original) = &pod_request {
            assert_eq!(guard.request, *original, "R5 original request retained");
            assert_eq!(
                *result,
                TicketResult::CreatedThenIsolated {
                    created_uid: "original-pod".into(),
                    uid: "sentinel-uid".into()
                }
            );
            assert_eq!(guard.origin, TicketOrigin::Worker);
        } else {
            assert_eq!(guard.origin, TicketOrigin::TerminalGuard);
        }
        assert_eq!(
            cold.reserve_terminal_claim_pod_and_join(binding, group, "target", "awaken-target")
                .await
                .unwrap(),
            RequestJoin::Complete(tickets),
            "R5 idempotent replay"
        );
        if let Some(request) = &other_request {
            assert_eq!(
                cold.ticket_for(binding, request).await.unwrap(),
                original_other,
                "R2 original inventory unchanged"
            );
        }
        assert_eq!(
            f.isolation.lock().claim_deletes,
            0,
            "isolation is not disposal"
        );
    }
}
