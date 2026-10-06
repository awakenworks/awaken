use super::*;

#[tokio::test]
async fn resumed_user_run_dispatch_settles_each_activity_without_republishing_input() {
    // Cause/effect graph: first User+System batch -> Applied Dispatch at Awaiting
    // -> new approval activity on SAME Run -> Applied Dispatch at next Awaiting
    // -> another activity -> terminal Applied Dispatch. R1 every boundary must
    // remove its own activity and close Running; R2 exact/stale callback replay
    // cannot change the committed root; R3 first input timestamps and anchors
    // stay immutable; R4 cold Session application reads the same persisted fence.
    // This fixture supplies Dispatch-Applied at the existing application port;
    // real Worker/HTTP and Native/ACP approval journeys own the transport proof.
    // R5 format=current/legacy: the supported old wire omits dispatch_settlement
    // after processing. Persist that shape through the existing SQLite owner,
    // then require cold continuation to establish its first NEW Dispatch receipt
    // without changing published input. This is a stored-format regression;
    // mixed executable versions remain a separate real deployment acceptance.
    // R6 simulated approval commits have increasing backend-wide source cursors
    // strictly after each durable activity admission. A pre/equal User Event
    // boundary is a separate fail-closed rule in interval_admission.rs.
    for legacy in [false, true] {
        let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(EventBatchRuntime::default());
        let id = "resumed-user-dispatch";
        let session = planned_session(
            id,
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("work")],
                },
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("context")],
                },
            ],
        );
        let run = match &session.event_batches[0].events[0].event {
            SessionEventCommand::UserMessage { run_id, .. } => run_id.clone(),
            _ => unreachable!(),
        };
        create(repository.as_ref(), session).await;
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        assert!(app.reconcile_event_batches().await.failures.is_empty());
        runtime.await_run(&run);
        finalize_applied_user_dispatch(&app, &runtime, id, &run).await;
        let first = repository.get(&SessionId::from(id)).await.unwrap();
        assert_eq!(
            first.execution,
            awaken_session_state_contract::SessionExecutionState::Idle
        );
        let published = |root: &PersistedSession| {
            root.event_batches[0]
                .events
                .iter()
                .map(|entry| {
                    (
                        entry.processed,
                        entry.projection_anchor,
                        entry.processed_at_unix_ms,
                    )
                })
                .collect::<Vec<_>>()
        };
        let original = published(&first);
        if legacy {
            let mut wire = serde_json::to_value(&first).unwrap();
            for entry in wire["event_batches"][0]["events"].as_array_mut().unwrap() {
                entry.as_object_mut().unwrap().remove("dispatch_settlement");
            }
            let old: PersistedSession = serde_json::from_value(wire).unwrap();
            let expected_revision = old.revision;
            let payload = awaken_session_state_contract::SessionMutationPayload::Replace(old);
            let payload_hash = payload.stable_hash();
            assert!(matches!(
                repository
                    .commit(
                        "workspace",
                        awaken_session_state_contract::SessionMutation {
                            expected_revision,
                            idempotency: awaken_session_state_contract::IdempotencyRecord {
                                key: "legacy:processed-user-without-dispatch-receipt".into(),
                                payload_hash,
                            },
                            payload,
                            lifecycle_facts: Vec::new(),
                        }
                    )
                    .await
                    .unwrap(),
                awaken_session_state_contract::SessionMutationResult::Applied { .. }
            ));
            let decoded = repository.get(&SessionId::from(id)).await.unwrap();
            assert!(
                decoded.event_batches[0].events[0]
                    .dispatch_settlement
                    .is_none()
            );
            assert_eq!(
                published(&decoded),
                original,
                "R5 old publication is valid stored input"
            );
        }
        let mut previous = None;
        let mut prior_source = 0;
        for cursor in [2, 3] {
            let boundary = if cursor == 2 {
                awaken_session_execution_contract::SessionAgentRunBoundary::Awaiting
            } else {
                awaken_session_execution_contract::SessionAgentRunBoundary::Completed
            };
            // Recreate the stateless application to exercise persisted receipts.
            let cold = application_with_runtime(
                runtime.clone(),
                repository.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let (admitted, epoch) = cold
                .begin_activity_for_operation(&SessionId::from(id), &format!("approval-{cursor}"))
                .await
                .unwrap();
            let admitted_after = admitted
                .running_interval
                .as_ref()
                .and_then(|interval| interval.admitted_after_commit_cursor)
                .expect("R6 admitted activity has a durable Runtime source fence");
            let source = admitted_after.max(prior_source) + 1;
            prior_source = source;
            let observation = awaken_session_state_contract::SessionRuntimeIntervalObservation {
                activity_epoch: epoch,
                thread_id: ThreadId(id.into()),
                run_id: run.clone(),
                lifecycle_cursor: awaken_agent_contract::encode_run_lifecycle_cursor(source, 0)
                    .unwrap(),
                source_commit_cursor: source,
            };
            let root = cold
                .settle_user_event_dispatch_observed(
                    &SessionId::from(id),
                    epoch,
                    observation.clone(),
                    &run,
                    boundary,
                )
                .await
                .unwrap();
            assert_eq!(
                root.execution,
                awaken_session_state_contract::SessionExecutionState::Idle,
                "R1"
            );
            assert!(
                root.active_activity_epochs.is_empty(),
                "R1 no stranded approval activity"
            );
            assert_eq!(published(&root), original, "R3");
            let bytes = serde_json::to_value(&root).unwrap();
            cold.settle_user_event_dispatch_observed(
                &SessionId::from(id),
                epoch,
                observation.clone(),
                &run,
                boundary,
            )
            .await
            .unwrap();
            if let Some((old_epoch, old_observation)) = previous {
                cold.settle_user_event_dispatch_observed(
                    &SessionId::from(id),
                    old_epoch,
                    old_observation,
                    &run,
                    boundary,
                )
                .await
                .unwrap();
            }
            assert_eq!(
                serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap(),
                bytes,
                "R2/R4"
            );
            previous = Some((epoch, observation));
        }
        assert_eq!(
            runtime.reserved.lock().unwrap().len(),
            1,
            "no User input replay"
        );
    }
}
