// @formal surface exclude=test_support_crate
// @formal owner crates/platform/committed-truth/awaken-store-conformance/src/session_store.rs
// @formal owner crates/platform/session/awaken-session-store/tests/session_repo_conformance.rs
// @formal reason: SessionStore root-CAS, tombstone, idempotency and create-replay rows split from the
// @formal reason: existing trait-generic conformance owner. It remains tooling-only and every durable
// @formal reason: backend invokes it through the same run_suite entrypoint.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

#[derive(Clone, Copy)]
pub enum CasRule {
    Create,
    CreateReplay,
    CreateIdempotencyMismatch,
    CreateReceiptAsMutation,
    Replace,
    ReplaceReplay,
    ReplaceReplayWrongExpected,
    ReplaceReplayWrongOwner,
    ReplaceIdempotencyMismatch,
    StaleRevision,
    WrongOwner,
    Delete,
    DeleteReplay,
}

pub fn record(key: &str, payload: &SessionMutationPayload) -> IdempotencyRecord {
    IdempotencyRecord {
        key: key.into(),
        payload_hash: payload.stable_hash(),
    }
}

pub fn delete_mutation(value: &PersistedSession, key: &str) -> SessionMutation {
    let payload = SessionMutationPayload::Delete(SessionTombstone {
        session_id: value.session_id.clone(),
        deleted_revision: SessionRevision(value.revision.0 + 1),
        deleted_at: at(1_786_752_000),
    });
    SessionMutation {
        expected_revision: value.revision,
        idempotency: record(key, &payload),
        payload,
        lifecycle_facts: Vec::new(),
    }
}

pub async fn tombstone_requires_hidden_completed_cleanup<
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox,
>(
    repo: &R,
) {
    // Every rejected delete must be an exact stutter: the aggregate, command
    // receipt and lifecycle outbox remain unchanged. The cases isolate the two
    // durable admission axes: hidden disposition and verified cleanup completion.
    let mut active = session("sesn_delete_active", "active");
    active.execution = SessionExecutionState::Terminated;
    complete_terminal_cleanup(&mut active);
    let active = create_session(repo, "ws_a", active, Vec::new()).await;

    let mut archived = session("sesn_delete_archived", "archived");
    archived
        .archive(at(1_786_752_000), 1_786_752_000_000)
        .unwrap();
    complete_terminal_cleanup(&mut archived);
    let archived = create_session(repo, "ws_a", archived, Vec::new()).await;

    let mut pending = session("sesn_delete_pending", "pending");
    assert!(pending.request_delete("ws_a", 0).unwrap().is_some());
    pending
        .terminal_cleanup
        .freeze_targets(&pending.session_id.clone(), [], 0, 0)
        .expect("pending delete freezes its root target");
    let pending = create_session(repo, "ws_a", pending, Vec::new()).await;

    for (rule, value) in [
        ("active", active),
        ("archived", archived),
        ("deleting-pending", pending),
    ] {
        let key = format!("test:tombstone:{rule}");
        let before = repo.get(&value.session_id).await.unwrap();
        let outbox_before = repo.pending_lifecycle().await.unwrap();
        assert!(
            matches!(
                repo.commit("ws_a", delete_mutation(&before, &key)).await,
                Err(SessionRepositoryError::InvalidMutation(_))
            ),
            "{rule}: tombstone admission fails closed"
        );
        assert_eq!(
            repo.get(&value.session_id).await,
            Ok(before.clone()),
            "{rule}: rejected tombstone preserves the complete aggregate"
        );
        assert_eq!(
            repo.idempotency_receipt(&value.session_id, &key).await,
            Ok(None),
            "{rule}: rejection writes no command receipt"
        );
        assert_eq!(
            repo.pending_lifecycle().await.unwrap(),
            outbox_before,
            "{rule}: rejection writes no lifecycle fact"
        );
    }

    let mut completed = session("sesn_delete_completed", "completed");
    assert!(completed.request_delete("ws_a", 0).unwrap().is_some());
    complete_terminal_cleanup(&mut completed);
    let completed = create_session(repo, "ws_a", completed, Vec::new()).await;
    let mutation = delete_mutation(&completed, "test:tombstone:completed");
    assert_eq!(
        repo.commit("ws_a", mutation.clone()).await.unwrap(),
        SessionMutationResult::Applied {
            new_revision: SessionRevision(completed.revision.0 + 1)
        },
        "deleting plus verified completed cleanup is tombstone-admissible"
    );
    assert_eq!(
        repo.get(&completed.session_id).await,
        Err(SessionRepositoryError::NotFound)
    );
    assert_eq!(
        repo.commit("ws_a", mutation).await.unwrap(),
        SessionMutationResult::Replayed {
            new_revision: SessionRevision(completed.revision.0 + 1)
        },
        "an admitted tombstone has one stable replay receipt"
    );
}

pub async fn deleted_event_admission_decision_table<
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox,
>(
    repo: &R,
) {
    use awaken_session_contract::SessionEventAdmission;
    use awaken_session_contract::{SessionEventBatch, SessionEventInput};
    // Delete CAS is the sole proof writer. D1 accepted key=>same Accepted;
    // D2 closed key=>same Closed; D3 absent key=>Deleted fence; D4 changed
    // fingerprint=>conflict; D5 foreign/missing Session=>no disclosure; D6
    // stale delete=>no tombstone. D7 mixed keyed/unkeyed batches=>retain only
    // keyed receipts in original order, including an accepted second key with
    // 12 operations (numeric order differs from lexical order at 10). D8 every
    // lookup/error/replay=>the complete stored proof remains unchanged. The
    // expected operation order is frozen from the original commands, without
    // calling the production operation_ids projection used by the encoder.
    // Payload is absent from retained projection.
    for kind in ["accepted", "closed", "empty", "mixed"] {
        let id = format!("sesn_retained_{kind}");
        let mut value = session(&id, "private title");
        let mut expected_receipts = vec![];
        let mut expected_operations = vec![];
        if kind != "empty" {
            let batch = if kind == "closed" {
                SessionEventBatch::closed_admission(
                    "batch".into(),
                    SessionRevision(1),
                    "key".into(),
                    "fp".into(),
                )
                .unwrap()
            } else {
                let input = SessionEventInput::UserMessage {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "private message",
                    )],
                };
                let inputs = vec![input; if kind == "mixed" { 12 } else { 1 }];
                let mut batch =
                    SessionEventBatch::compile(&SessionId::from(id.as_str()), "batch", inputs)
                        .unwrap();
                batch.admitted_revision = SessionRevision(1);
                batch
                    .anchor_admission_projection(SessionEventProjectionAnchor {
                        source_commit_cursor: 0,
                    })
                    .unwrap();
                batch.bind_idempotency("key", "fp").unwrap();
                batch.resolve_terminally(None, 1);
                batch
            };
            expected_operations = batch
                .events
                .iter()
                .map(|entry| entry.event.operation_id().to_owned())
                .collect();
            expected_receipts.push(
                json!({"key":"key","request_fingerprint":"fp","batch_id":"batch",
                "closed":kind == "closed", "operation_ids":expected_operations}),
            );
            value.event_batches.push(batch);
        }
        if kind == "mixed" {
            let closed = SessionEventBatch::closed_admission(
                "prior:batch".into(),
                SessionRevision(1),
                "prior-key".into(),
                "prior-fp".into(),
            )
            .unwrap();
            value.event_batches.insert(0, closed);
            expected_receipts.insert(
                0,
                json!({"key":"prior-key", "request_fingerprint":"prior-fp",
                "batch_id":"prior:batch", "closed":true, "operation_ids":[]}),
            );
            let mut unkeyed = SessionEventBatch::compile(
                &SessionId::from(id.as_str()),
                "unkeyed",
                vec![SessionEventInput::UserMessage {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "unretained private message",
                    )],
                }],
            )
            .unwrap();
            unkeyed.admitted_revision = SessionRevision(1);
            unkeyed
                .anchor_admission_projection(SessionEventProjectionAnchor {
                    source_commit_cursor: 0,
                })
                .unwrap();
            unkeyed.resolve_terminally(None, 1);
            value.event_batches.push(unkeyed);
        }
        assert!(value.request_delete("ws_a", 0).unwrap().is_some());
        complete_terminal_cleanup(&mut value);
        let value = create_session(repo, "ws_a", value, vec![]).await;
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &SessionId::from(&id))
                .await
                .unwrap(),
            None
        );
        let mut stale = delete_mutation(&value, &format!("delete-stale-{kind}"));
        stale.expected_revision = SessionRevision(0);
        assert!(repo.commit("ws_a", stale).await.is_err(), "D6");
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &SessionId::from(&id))
                .await
                .unwrap(),
            None,
            "D6"
        );
        repo.commit("ws_a", delete_mutation(&value, &format!("delete-{kind}")))
            .await
            .unwrap();
        let proof = repo
            .deleted_event_admissions("ws_a", &SessionId::from(&id))
            .await
            .unwrap()
            .unwrap();
        let deleted = SessionEventAdmission::Deleted {
            deleted_revision: SessionRevision(value.revision.0 + 1),
        };
        let resolve = |key, fingerprint| {
            awaken_session_contract::resolve_deleted_event_admission(&proof, key, fingerprint)
        };
        assert_eq!(resolve("unseen", "fp").unwrap(), deleted, "D3");
        if kind == "empty" {
            assert_eq!(proof.admissions, Some(vec![]));
            assert_eq!(resolve("key", "fp").unwrap(), deleted);
        } else {
            assert_eq!(
                resolve("key", "fp").unwrap(),
                if kind != "closed" {
                    SessionEventAdmission::Accepted {
                        batch_id: "batch".into(),
                        operation_ids: Some(expected_operations.clone()),
                    }
                } else {
                    SessionEventAdmission::Closed {
                        batch_id: "batch".into(),
                    }
                },
                "D1/D2"
            );
            assert_eq!(
                resolve("key", "changed").unwrap_err().code,
                "idempotency_conflict",
                "D4"
            );
            let retained = serde_json::to_value(proof.admissions.as_ref().unwrap()).unwrap();
            assert_eq!(retained, json!(expected_receipts));
        }
        if kind == "mixed" {
            assert_eq!(
                resolve("prior-key", "prior-fp").unwrap(),
                SessionEventAdmission::Closed {
                    batch_id: "prior:batch".into(),
                },
                "D7 first key remains distinct from accepted second key"
            );
            assert_eq!(
                resolve("prior-key", "fp").unwrap_err().code,
                "idempotency_conflict",
                "D7 fingerprint belongs to its exact key"
            );
        }
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &SessionId::from(&id))
                .await
                .unwrap(),
            Some(proof.clone()),
            "D8 reads preserve full proof"
        );
        assert!(
            matches!(repo.commit("ws_a", delete_mutation(&value, &format!("delete-{kind}"))).await.unwrap(),
            SessionMutationResult::Replayed { new_revision } if new_revision == proof.deleted_revision),
            "D8 stable delete receipt"
        );
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &SessionId::from(&id))
                .await
                .unwrap(),
            Some(proof),
            "D8 deletion replay preserves full proof"
        );
        assert_eq!(
            repo.deleted_event_admissions("foreign", &SessionId::from(&id))
                .await
                .unwrap(),
            None,
            "D5"
        );
        assert!(matches!(
            repo.get(&SessionId::from(id)).await,
            Err(SessionRepositoryError::NotFound)
        ));
        let mut recreated = value;
        recreated.revision = SessionRevision(0);
        let payload = SessionMutationPayload::Replace(recreated.clone());
        assert!(
            matches!(
                repo.create(
                    "ws_a",
                    recreated,
                    record(&format!("recreate-{kind}"), &payload),
                    vec![]
                )
                .await,
                Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::Tombstoned
                ))
            ),
            "late recreation is fenced"
        );
    }
    assert_eq!(
        repo.deleted_event_admissions("ws_a", &SessionId::from("missing"))
            .await
            .unwrap(),
        None,
        "D5"
    );
}

pub fn root_cas_decision_table<
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox,
>(
    repo: &R,
) -> impl std::future::Future<Output = ()> + '_ {
    // Keep this owned conformance phase off the caller's async stack.
    // Assertions and their order stay in the original suite.
    Box::pin(async move {
        // Cause-effect graph:
        // C1 valid command -> C2 idempotency absent-or-equal -> C3 live row exists
        // -> C4 owner exact -> C5 revision exact -> E1 atomic applied/replayed.
        // Delete additionally yields E2 tombstone read-as-not-found. Any failed
        // cause yields its typed result and changes neither aggregate nor outbox.
        //
        // | Rule | Operation | C2 | C3 | C4 | C5 | Result |
        // |---|---|---|---|---|---|---|
        // | R1 | create | absent | - | - | new | revision 1 |
        // | R2 | create retry | equal | - | - | - | revision 1 |
        // | R3 | create retry | mismatch | - | - | - | idempotency error |
        // | R4 | replace | absent | T | T | T | applied revision 2 |
        // | R5 | replace retry | equal | T | T | T | replayed revision 2 |
        // | R6 | replace retry | equal | T | F | T | conflict revision 2 |
        // | R7 | replace retry | mismatch | T | T | T | idempotency mismatch |
        // | R8 | replace | absent | T | T | F | conflict revision 1 |
        // | R9 | replace | absent | T | F | T | conflict revision 1 |
        // | R10 | delete | absent | T | T | T | applied revision 2 + hidden |
        // | R11 | delete retry | equal | tombstone | T | old | replayed revision 2 |
        // | R12 | mutation with create receipt | equal | T | T | next differs | mismatch |
        // | R13 | replace retry | equal | T | T | wrong expected | mismatch |
        let rules = [
            CasRule::Create,
            CasRule::CreateReplay,
            CasRule::CreateIdempotencyMismatch,
            CasRule::CreateReceiptAsMutation,
            CasRule::Replace,
            CasRule::ReplaceReplay,
            CasRule::ReplaceReplayWrongExpected,
            CasRule::ReplaceReplayWrongOwner,
            CasRule::ReplaceIdempotencyMismatch,
            CasRule::StaleRevision,
            CasRule::WrongOwner,
            CasRule::Delete,
            CasRule::DeleteReplay,
        ];
        for (index, rule) in rules.into_iter().enumerate() {
            let id = format!("sesn_cas_{index}");
            let mut initial = session(&id, "initial");
            if matches!(rule, CasRule::Delete | CasRule::DeleteReplay) {
                assert!(initial.request_delete("ws_a", 0).unwrap().is_some());
                complete_terminal_cleanup(&mut initial);
            }
            let create_payload = SessionMutationPayload::Replace(initial.clone());
            let create_record = record("create", &create_payload);
            let created = repo
                .create("ws_a", initial, create_record.clone(), Vec::new())
                .await
                .expect("initial CAS create");
            assert_eq!(
                created,
                awaken_session_contract::SessionCreateResult::Applied({
                    let mut expected = session(&id, "initial");
                    if matches!(rule, CasRule::Delete | CasRule::DeleteReplay) {
                        assert!(expected.request_delete("ws_a", 0).unwrap().is_some());
                        complete_terminal_cleanup(&mut expected);
                    }
                    expected.revision = SessionRevision(1);
                    expected
                })
            );
            assert_eq!(
                repo.idempotency_receipt(&SessionId::from(&id), "create")
                    .await,
                Ok(Some(
                    awaken_session_contract::SessionIdempotencyReceipt::new(
                        create_record.payload_hash.clone(),
                        SessionRevision(1),
                        Default::default()
                    )
                )),
                "the decision table reads the same atomic receipt it writes"
            );

            match rule {
                CasRule::Create => {}
                CasRule::CreateReplay => {
                    let mut replay = session(&id, "initial");
                    replay.revision = SessionRevision(0);
                    let mut expected = replay.clone();
                    expected.revision = SessionRevision(1);
                    assert_eq!(
                        repo.create("ws_a", replay, create_record, Vec::new())
                            .await
                            .unwrap(),
                        awaken_session_contract::SessionCreateResult::Replayed(expected)
                    );
                }
                CasRule::CreateIdempotencyMismatch => {
                    let error = repo
                        .create(
                            "ws_a",
                            session(&id, "different"),
                            IdempotencyRecord {
                                key: "create".into(),
                                payload_hash: "different".into(),
                            },
                            Vec::new(),
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(
                        error,
                        SessionRepositoryError::Conflict(
                            awaken_session_contract::SessionRepositoryConflict::IdempotencyMismatch
                        )
                    );
                }
                CasRule::CreateReceiptAsMutation => {
                    let mut replacement = repo.get(&SessionId::from(id)).await.unwrap();
                    replacement.revision = SessionRevision(0);
                    assert_eq!(
                        repo.commit(
                            "ws_a",
                            SessionMutation {
                                expected_revision: SessionRevision(0),
                                idempotency: create_record,
                                payload: SessionMutationPayload::Replace(replacement),
                                lifecycle_facts: Vec::new(),
                            },
                        )
                        .await
                        .unwrap(),
                        SessionMutationResult::IdempotencyMismatch,
                        "R12 a revision-one create receipt cannot replay even a forged revision-zero mutation"
                    );
                }
                CasRule::Replace
                | CasRule::ReplaceReplay
                | CasRule::ReplaceReplayWrongExpected
                | CasRule::ReplaceReplayWrongOwner
                | CasRule::ReplaceIdempotencyMismatch
                | CasRule::StaleRevision
                | CasRule::WrongOwner => {
                    let before = repo.get(&SessionId::from(id)).await.unwrap();
                    let mut replacement = before.clone();
                    replacement.title = Some("replacement".into());
                    // Legal replace/replay rows carry the original State fact.
                    // Stale/foreign rows keep their invalid candidate untouched so
                    // the existing owner/revision classifier still wins first.
                    if !matches!(rule, CasRule::StaleRevision | CasRule::WrongOwner) {
                        replacement = record_update(&before, replacement, 0).into_snapshot();
                    }
                    if matches!(rule, CasRule::StaleRevision) {
                        replacement.revision = SessionRevision(0);
                    }
                    let payload = SessionMutationPayload::Replace(replacement);
                    let key = if matches!(rule, CasRule::ReplaceIdempotencyMismatch) {
                        "replace-mismatch"
                    } else {
                        "replace"
                    };
                    let mutation = SessionMutation {
                        expected_revision: if matches!(rule, CasRule::StaleRevision) {
                            SessionRevision(0)
                        } else {
                            SessionRevision(1)
                        },
                        idempotency: record(key, &payload),
                        payload: payload.clone(),
                        lifecycle_facts: Vec::new(),
                    };
                    let owner = if matches!(rule, CasRule::WrongOwner) {
                        "ws_b"
                    } else {
                        "ws_a"
                    };
                    let first = repo.commit(owner, mutation.clone()).await.unwrap();
                    let expected = if matches!(rule, CasRule::StaleRevision | CasRule::WrongOwner) {
                        SessionMutationResult::Conflict {
                            current_revision: SessionRevision(1),
                        }
                    } else {
                        SessionMutationResult::Applied {
                            new_revision: SessionRevision(2),
                        }
                    };
                    assert_eq!(first, expected);
                    if matches!(rule, CasRule::ReplaceReplay) {
                        assert_eq!(
                            repo.commit(owner, mutation.clone()).await.unwrap(),
                            SessionMutationResult::Replayed {
                                new_revision: SessionRevision(2)
                            }
                        );
                    }
                    if matches!(rule, CasRule::ReplaceReplayWrongExpected) {
                        let mut wrong_expected = mutation.clone();
                        wrong_expected.expected_revision = SessionRevision(0);
                        if let SessionMutationPayload::Replace(session) =
                            &mut wrong_expected.payload
                        {
                            session.revision = SessionRevision(0);
                        }
                        assert_eq!(
                            repo.commit(owner, wrong_expected).await.unwrap(),
                            SessionMutationResult::IdempotencyMismatch,
                            "R13 a receipt cannot replay under another expected revision"
                        );
                    }
                    if matches!(rule, CasRule::ReplaceReplayWrongOwner) {
                        assert_eq!(
                            repo.commit("ws_b", mutation.clone()).await.unwrap(),
                            SessionMutationResult::Conflict {
                                current_revision: SessionRevision(2)
                            },
                            "a foreign owner cannot adopt an exact mutation receipt"
                        );
                    }
                    if matches!(rule, CasRule::ReplaceIdempotencyMismatch) {
                        let mut mismatched = mutation;
                        mismatched.idempotency.payload_hash = "different".into();
                        assert_eq!(
                            repo.commit(owner, mismatched).await.unwrap(),
                            SessionMutationResult::IdempotencyMismatch
                        );
                    }
                }
                CasRule::Delete | CasRule::DeleteReplay => {
                    let payload = SessionMutationPayload::Delete(SessionTombstone {
                        session_id: SessionId::from(&id),
                        deleted_revision: SessionRevision(2),
                        deleted_at: at(1_784_937_600),
                    });
                    let mutation = SessionMutation {
                        expected_revision: SessionRevision(1),
                        idempotency: record("delete", &payload),
                        payload,
                        lifecycle_facts: Vec::new(),
                    };
                    assert_eq!(
                        repo.commit("ws_a", mutation.clone()).await.unwrap(),
                        SessionMutationResult::Applied {
                            new_revision: SessionRevision(2)
                        }
                    );
                    assert_eq!(
                        repo.get(&SessionId::from(id)).await,
                        Err(SessionRepositoryError::NotFound)
                    );
                    if matches!(rule, CasRule::DeleteReplay) {
                        assert_eq!(
                            repo.commit("ws_a", mutation).await.unwrap(),
                            SessionMutationResult::Replayed {
                                new_revision: SessionRevision(2)
                            }
                        );
                    }
                }
            }
        }
        materialization_retention_rows(repo).await;
    })
}

fn materialization_retention_rows<
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox,
>(
    repo: &R,
) -> impl std::future::Future<Output = ()> + '_ {
    // Box this phase at its constructor boundary. Otherwise its inline
    // state inflates the parent CAS poll frame even when that parent is
    // already pinned, overflowing the same 1 MiB MSVC regression thread.
    Box::pin(async move {
        // M1 actual first CAS -> immutable pending. M2 fresh mutation trying
        // known->None/empty/another first -> full root/outbox stutter. M3 matched
        // receipt -> origin; replay of the older first mutation must classify its
        // exact receipt before the replacement law and preserve the later origin.
        // M4 an old Unknown root cannot be upgraded by a generic replacement.
        use awaken_session_contract::{
            SessionAggregate, SessionEnvironmentEffectIntent, SessionEnvironmentEffectKind,
        };
        let old = session("sesn_materialization_cas", "fresh");
        let fresh = PersistedSession::frozen_with_budget(
            old.session_id.clone(),
            old.frozen_baseline().unwrap().clone(),
            old.resources,
            old.mcp,
            old.title,
            old.metadata,
            old.tools,
            old.budget,
        );
        let fresh = create_session(repo, "ws_a", fresh, Vec::new()).await;
        let mut claimed = SessionAggregate::try_from(fresh).unwrap();
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker:1:boot".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        claimed.assignment().claim(lease.clone(), false).unwrap();
        let empty = replace_session(
            repo,
            "ws_a",
            claimed.into_snapshot(),
            "materialization-claim",
            Vec::new(),
        )
        .await;
        let intent = SessionEnvironmentEffectIntent::new(
            empty.session_id.clone(),
            SessionEnvironmentEffectKind::Create,
            Some(lease.clone()),
        )
        .for_environment(
            empty
                .frozen_baseline()
                .unwrap()
                .environment
                .config_fingerprint
                .0
                .clone(),
        );
        let mut admitted = SessionAggregate::try_from(empty.clone()).unwrap();
        admitted
            .environment()
            .admit_environment_effect("ws_a", &intent, 1)
            .unwrap();
        let payload = SessionMutationPayload::Replace(admitted.into_snapshot());
        let first = SessionMutation {
            expected_revision: empty.revision,
            idempotency: record("materialization-first", &payload),
            payload,
            lifecycle_facts: Vec::new(),
        };
        assert!(matches!(
            repo.commit("ws_a", first.clone()).await.unwrap(),
            SessionMutationResult::Applied { .. }
        ));
        let pending = repo.get(&empty.session_id).await.unwrap();
        assert_eq!(
            pending.environment_materialization().unwrap().pending(),
            Some(&intent)
        );
        let outbox = repo.pending_lifecycle().await.unwrap();
        let mut erased_wire = serde_json::to_value(&pending).unwrap();
        erased_wire
            .as_object_mut()
            .unwrap()
            .remove("environment_materialization");
        let erased: PersistedSession = serde_json::from_value(erased_wire).unwrap();
        let mut reset = empty.clone();
        reset.revision = pending.revision;
        let mut wire = serde_json::to_value(&pending).unwrap();
        let mut shortened = lease;
        shortened.expires_at_unix_ms -= 1;
        wire["environment_materialization"]["pending"] = serde_json::to_value(
            SessionEnvironmentEffectIntent::new(
                pending.session_id.clone(),
                SessionEnvironmentEffectKind::Create,
                Some(shortened),
            )
            .for_environment(intent.environment_fingerprint().unwrap()),
        )
        .unwrap();
        let overwritten: PersistedSession = serde_json::from_value(wire).unwrap();
        for (index, bad) in [erased, reset, overwritten].into_iter().enumerate() {
            let payload = SessionMutationPayload::Replace(bad);
            let mutation = SessionMutation {
                expected_revision: pending.revision,
                idempotency: record(&format!("materialization-reset-{index}"), &payload),
                payload,
                lifecycle_facts: Vec::new(),
            };
            assert!(
                matches!(
                    repo.commit("ws_a", mutation).await,
                    Err(SessionRepositoryError::InvalidMutation(_))
                ),
                "M2 {index}"
            );
            assert_eq!(repo.get(&pending.session_id).await.unwrap(), pending);
            assert_eq!(repo.pending_lifecycle().await.unwrap(), outbox);
        }
        let mut completed = SessionAggregate::try_from(pending.clone()).unwrap();
        completed
            .environment()
            .try_apply_environment_receipt(
                "ws_a",
                &awaken_session_contract::SessionEnvironmentReceipt::from_intent(
                    &intent, "binding",
                )
                .unwrap(),
                1,
            )
            .unwrap();
        let origin = replace_session(
            repo,
            "ws_a",
            completed.into_snapshot(),
            "materialization-completed",
            Vec::new(),
        )
        .await;
        assert!(
            matches!(
                repo.commit("ws_a", first).await.unwrap(),
                SessionMutationResult::Replayed { .. }
            ),
            "M3 replay before reset gate"
        );
        assert_eq!(repo.get(&origin.session_id).await.unwrap(), origin);
        let legacy = create_session(
            repo,
            "ws_a",
            session("sesn_materialization_legacy", "old"),
            Vec::new(),
        )
        .await;
        let mut backfilled_wire = serde_json::to_value(&legacy).unwrap();
        backfilled_wire["environment_materialization"] =
            serde_json::to_value(empty.environment_materialization()).unwrap();
        let backfilled: PersistedSession = serde_json::from_value(backfilled_wire).unwrap();
        let payload = SessionMutationPayload::Replace(backfilled);
        assert!(
            matches!(
                repo.commit(
                    "ws_a",
                    SessionMutation {
                        expected_revision: legacy.revision,
                        idempotency: record("materialization-backfill", &payload),
                        payload,
                        lifecycle_facts: Vec::new()
                    }
                )
                .await,
                Err(SessionRepositoryError::InvalidMutation(_))
            ),
            "M4"
        );
        assert_eq!(repo.get(&legacy.session_id).await.unwrap(), legacy);
    })
}

pub async fn create_receipt_decision_table<
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox,
>(
    repo: &R,
) {
    // Create-receipt cause/effect table. C1 receipt absent/present; C2 durable
    // identity absent/live/tombstoned; C3 owner exact/foreign; C4 hash exact/
    // mismatched; C5 the live aggregate advanced after create. Effects: E1 a
    // new root is Applied once; E2 exact replay returns the current durable
    // aggregate, never the caller's newly lowered candidate; E3 wrong owner or
    // another key conflicts; E4 hash reuse mismatches; E5 tombstone is terminal;
    // E6 absent/whitespace owner is rejected before writes.
    //
    // | Rule | Receipt | Identity | Owner | Hash | Effect |
    // |---|---|---|---|---|---|
    // | C1 | absent | absent | exact | valid | E1 |
    // | C2 | exact | live+advanced | exact | exact | E2 |
    // | C3 | exact/absent | live | foreign/exact | exact/other key | E3 |
    // | C4 | present | live | exact | mismatch | E4 |
    // | C5 | exact | tombstone | exact | exact | E5 |
    // | C6 | any | any | empty/space | any | E6 |
    let id = "sesn_create_receipt_table";
    let candidate = session(id, "original lowering");
    let create_record = IdempotencyRecord {
        key: "create-receipt-table".into(),
        payload_hash: "stable-request-fingerprint".into(),
    };
    let applied = repo
        .create("ws_a", candidate.clone(), create_record.clone(), Vec::new())
        .await
        .expect("C1/E1");
    let SessionCreateResult::Applied(mut durable) = applied else {
        panic!("C1/E1 must apply")
    };
    durable.title = Some("durable revision two".into());
    durable = replace_session(repo, "ws_a", durable, "advance-after-create", Vec::new()).await;

    assert_eq!(
        repo.replay_create("ws_a", &SessionId::from(id), &create_record)
            .await,
        Ok(Some(durable.clone())),
        "C2/E2 atomic preflight returns current durable aggregate"
    );
    let mut changed_lowering = candidate;
    changed_lowering.title = Some("new lowering must not escape".into());
    assert_eq!(
        repo.create("ws_a", changed_lowering, create_record.clone(), Vec::new(),)
            .await,
        Ok(SessionCreateResult::Replayed(durable.clone())),
        "C2/E2 create replay returns repository truth"
    );
    assert_eq!(
        repo.replay_create("ws_b", &SessionId::from(id), &create_record)
            .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::AlreadyExists
        )),
        "C3/E3 owner-bound receipt"
    );
    assert_eq!(
        repo.replay_create(
            "ws_a",
            &SessionId::from(id),
            &IdempotencyRecord {
                payload_hash: "another-request".into(),
                ..create_record.clone()
            },
        )
        .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::IdempotencyMismatch
        )),
        "C4/E4"
    );
    assert_eq!(
        repo.replay_create(
            "ws_a",
            &SessionId::from(id),
            &IdempotencyRecord {
                key: "another-key".into(),
                payload_hash: create_record.payload_hash.clone(),
            },
        )
        .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::AlreadyExists
        )),
        "C3/E3 another key cannot adopt an occupied identity"
    );

    let tombstoned_id = "sesn_create_receipt_tombstone";
    let mut tombstoned = session(tombstoned_id, "terminal");
    assert!(tombstoned.request_delete("ws_a", 0).unwrap().is_some());
    complete_terminal_cleanup(&mut tombstoned);
    let tombstone_record = IdempotencyRecord {
        key: "create-receipt-tombstone".into(),
        payload_hash: "terminal-request".into(),
    };
    let SessionCreateResult::Applied(tombstoned) = repo
        .create("ws_a", tombstoned, tombstone_record.clone(), Vec::new())
        .await
        .expect("C5 setup")
    else {
        panic!("C5 setup applies")
    };
    repo.commit(
        "ws_a",
        delete_mutation(&tombstoned, "delete-create-receipt-tombstone"),
    )
    .await
    .expect("C5 tombstone");
    assert_eq!(
        repo.replay_create("ws_a", &SessionId::from(tombstoned_id), &tombstone_record)
            .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::Tombstoned
        )),
        "C5/E5"
    );
    for (owner, invalid_id) in [("", "sesn_empty_owner"), (" \t", "sesn_whitespace_owner")] {
        assert!(matches!(
            repo.create(
                owner,
                session(invalid_id, "invalid"),
                IdempotencyRecord {
                    key: "invalid-owner".into(),
                    payload_hash: "invalid-owner".into(),
                },
                Vec::new(),
            )
            .await,
            Err(SessionRepositoryError::InvalidMutation(_))
        ));
        assert_eq!(
            repo.get(&SessionId::from(invalid_id)).await,
            Err(SessionRepositoryError::NotFound),
            "C6/E6"
        );
    }
}

pub async fn create_receipt_race_decision_table<
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox,
>(
    repo: std::sync::Arc<R>,
) {
    // Concurrent create cause/effect table. C1 same key+hash with different
    // lowerings; C2 same key with different hashes; C3 delete races a create on
    // the same durable identity. Effects: E1 exactly one Applied plus one
    // Replayed carrying the winner; E2 exactly one Applied plus one mismatch;
    // E3 delete never leaves both a live aggregate and tombstone and the create
    // cannot resurrect the identity. A two-party barrier makes every row enter
    // the repository concurrently; the repository transaction is the sole arbiter.
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let same_id = "sesn_create_race_same";
    let same_record = IdempotencyRecord {
        key: "race-same".into(),
        payload_hash: "same-request".into(),
    };
    let left = {
        let repo = repo.clone();
        let barrier = barrier.clone();
        let record = same_record.clone();
        async move {
            barrier.wait().await;
            repo.create("ws_a", session(same_id, "left"), record, Vec::new())
                .await
        }
    };
    let right = {
        let repo = repo.clone();
        let barrier = barrier.clone();
        let record = same_record;
        async move {
            barrier.wait().await;
            repo.create("ws_a", session(same_id, "right"), record, Vec::new())
                .await
        }
    };
    let (left, right) = tokio::join!(left, right);
    let outcomes = [left.expect("C1 left"), right.expect("C1 right")];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SessionCreateResult::Applied(_)))
            .count(),
        1,
        "C1/E1 one Applied"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SessionCreateResult::Replayed(_)))
            .count(),
        1,
        "C1/E1 one Replayed"
    );
    let durable = repo
        .get(&SessionId::from(same_id))
        .await
        .expect("C1 durable winner");
    assert!(outcomes.iter().all(|outcome| match outcome {
        SessionCreateResult::Applied(session) | SessionCreateResult::Replayed(session) =>
            session == &durable,
    }));

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let different_id = "sesn_create_race_different";
    let create = |title: &'static str, hash: &'static str| {
        let repo = repo.clone();
        let barrier = barrier.clone();
        async move {
            barrier.wait().await;
            repo.create(
                "ws_a",
                session(different_id, title),
                IdempotencyRecord {
                    key: "race-different".into(),
                    payload_hash: hash.into(),
                },
                Vec::new(),
            )
            .await
        }
    };
    let (left, right) = tokio::join!(create("left", "left-hash"), create("right", "right-hash"));
    let outcomes = [left, right];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(SessionCreateResult::Applied(_))))
            .count(),
        1,
        "C2/E2 one Applied"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    Err(SessionRepositoryError::Conflict(
                        SessionRepositoryConflict::IdempotencyMismatch
                    ))
                )
            })
            .count(),
        1,
        "C2/E2 one mismatch"
    );

    let delete_id = "sesn_delete_create_race";
    let mut deletable = session(delete_id, "delete winner");
    assert!(deletable.request_delete("ws_a", 0).unwrap().is_some());
    complete_terminal_cleanup(&mut deletable);
    let deletable = create_session(repo.as_ref(), "ws_a", deletable, Vec::new()).await;
    let delete = delete_mutation(&deletable, "delete-create-race");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let delete_future = {
        let repo = repo.clone();
        let barrier = barrier.clone();
        async move {
            barrier.wait().await;
            repo.commit("ws_a", delete).await
        }
    };
    let create_future = {
        let repo = repo.clone();
        let barrier = barrier;
        async move {
            barrier.wait().await;
            repo.create(
                "ws_a",
                session(delete_id, "must not resurrect"),
                IdempotencyRecord {
                    key: "create-after-delete-race".into(),
                    payload_hash: "create-after-delete-race".into(),
                },
                Vec::new(),
            )
            .await
        }
    };
    let (deleted, created) = tokio::join!(delete_future, create_future);
    assert!(
        matches!(deleted, Ok(SessionMutationResult::Applied { .. })),
        "C3/E3"
    );
    assert!(
        matches!(
            created,
            Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::AlreadyExists | SessionRepositoryConflict::Tombstoned
            ))
        ),
        "C3/E3"
    );
    assert_eq!(
        repo.get(&SessionId::from(delete_id)).await,
        Err(SessionRepositoryError::NotFound),
        "C3/E3"
    );
}
