//! Replay of a failed Session's terminal history across later ordinary release.

use super::*;

#[tokio::test]
async fn failed_terminal_times_survive_archive_and_cold_reconstruction() {
    // Causes: C1 durable ActivationFailed; C2 failure time known/unknown;
    // C3 source Run error with commit time 200, or no source; C4 terminal
    // history read before ordinary release archives the root at a later time;
    // C5 a second replica reads before release or cold-rebuilds after it.
    // Effects: E1 both parent terminal events use only C2 (unknown stays null);
    // E2 release and replay succeed; E3 exact complete event DTOs remain equal
    // in warm, second-replica and cold reads. Run error keeps its commit time.
    // Rules: R1=C1+known+C3+C4+C5=>E1+E2+E3;
    // R2=C1+unknown+C3+C4+C5=>E1(null)+E2+E3. Cross each rule with source/no
    // source. Existing normal-archive and delivery tests own nonfailed archive
    // times and rejection of genuinely conflicting known observations.
    for failure_time in [Some(100), None] {
        for sourced in [true, false] {
            let id = format!("failed-time-{failure_time:?}-{sourced}");
            let repository = Arc::new(ephemeral_session_repo().await);
            let mut persisted = crate::state::tests::sample_persisted(&id);
            persisted.execution =
                awaken_session_state_contract::SessionExecutionState::ActivationFailed;
            persisted.realization_progress.failed_at_unix_ms = failure_time;
            persisted.realization_progress.last_error = Some("activation failed".into());
            let runtime = RehydrateFake::default();
            if sourced {
                let run = RunId("failed-time-run".into());
                persisted.realization_progress.failure_source_run_id = Some(Box::new(run.clone()));
                runtime.lifecycle.lock().unwrap().push(lifecycle(
                    1,
                    &id,
                    &run,
                    RunLifecycleEventKind::Failed,
                    RunState::Ended(EndCause::Error(
                        awaken_agent_contract::agent::run::Failure::Inference {
                            code: "dispatch_resolution_failed".into(),
                            message: "stream ended without exit status".into(),
                        },
                    )),
                ));
                runtime
                    .commit_times_by_thread
                    .lock()
                    .unwrap()
                    .insert(id.clone(), std::collections::BTreeMap::from([(1, 200)]));
            }
            crate::state::test_support::create_session_fixture(
                repository.as_ref(),
                DEFAULT_SCOPE,
                persisted,
            )
            .await;
            let warm = ManagedState::new(runtime.clone())
                .await
                .with_session_repo(repository.clone());
            let replica = ManagedState::new(runtime.clone())
                .await
                .with_session_repo(repository.clone());
            warm.refresh_committed_events(&id).await.unwrap();
            replica.refresh_committed_events(&id).await.unwrap();
            let events =
                |state: &ManagedState| state.list_events(&id, None, None, false).unwrap().data;
            let before = events(&warm);
            warm.release_profiled_session_without_publication(&id)
                .await
                .expect("E2 ordinary release must replay terminal history");
            warm.release_profiled_session_without_publication(&id)
                .await
                .expect("E2 exact replay");
            let terminals = before
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        OutboundKind::SessionThreadStatusTerminated { .. }
                            | OutboundKind::SessionStatusTerminated { .. }
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(terminals.len(), 2, "E1 both parent terminal events");
            for event in terminals {
                assert_eq!(
                    event.processed_at,
                    failure_time.map(awaken_session_application_contract::epoch_millis_to_rfc3339),
                    "E1 failure owns terminal time, sourced={sourced}"
                );
            }
            replica.refresh_committed_events(&id).await.unwrap();
            let cold = ManagedState::new(runtime)
                .await
                .with_session_repo(repository);
            cold.refresh_committed_events(&id).await.unwrap();
            for state in [&warm, &replica, &cold] {
                assert_eq!(events(state), before, "E3 complete DTOs survive release");
            }
        }
    }
}
