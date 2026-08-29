use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_contract::{SessionEventInput, SessionInitialEventPlan};

use super::*;

#[tokio::test]
async fn final_authority_scan_is_the_only_event_batch_cutover_generation_trigger() {
    // Cause/effect graph: C1 no final authority scan has completed; C2 an empty
    // scan and global Environment-phase count both succeed; C3 the next final
    // scan is unavailable; C4 the scan succeeds but the global count is
    // unavailable; C5 the global count encounters a corrupt root; C6 a later
    // successful pair sees one terminal incomplete Event batch, two
    // Event-batch failures, and one Restoring Environment. Effects: E1 C1 has
    // no snapshot; E2 C2 publishes generation one; E3 C3, C4, or C5 preserves
    // E2 byte-for-byte; E4 C6 publishes generation two with exact counts.
    // Quarantine counting is covered by the reducer test; the admin response
    // test owns HTTP fail-closed mapping.
    //
    // | Rule | Final scan | Global phase count | Terminal/batch/restore | Effect |
    // |---|---|---|---|---|
    // | V1 | none yet | none yet | n/a | no snapshot |
    // | V2 | success | success | 0/0/0 | generation 1 |
    // | V3 | unavailable | not called | unknown | error, V2 unchanged |
    // | V4 | success | unavailable | unknown | error, V2 unchanged |
    // | V5 | success | corrupt | unknown | error, V2 unchanged |
    // | V6 | success | success | 1/2/1 | generation 2, exact counts |
    let inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("validation repository"),
    );
    let repository = Arc::new(FaultingSessionRepository::new(inner.clone()));
    let application = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let validation = application.session_event_batch_cutover_validation_source();

    assert_eq!(validation.snapshot(), None, "V1/E1");
    let first = application
        .refresh_event_batch_cutover_validation(0)
        .await
        .expect("V2 complete scan");
    assert_eq!(
        first,
        SessionEventBatchCutoverValidationSnapshot {
            generation: 1,
            terminal_with_incomplete_event_batches: 0,
            event_batch_failures: 0,
            quarantined: 0,
            restoring_sessions: 0,
        },
        "V2/E2"
    );

    repository.fail_recovery_scan_once();
    assert!(
        application
            .refresh_event_batch_cutover_validation(7)
            .await
            .is_err(),
        "V3 repository outage"
    );
    assert_eq!(validation.snapshot(), Some(first), "V3/E3");

    repository.fail_environment_phase_count_once();
    assert!(
        application
            .refresh_event_batch_cutover_validation(8)
            .await
            .is_err(),
        "V4 global count outage"
    );
    assert_eq!(validation.snapshot(), Some(first), "V4/E3");

    repository.corrupt_environment_phase_count_once();
    assert!(
        application
            .refresh_event_batch_cutover_validation(9)
            .await
            .is_err(),
        "V5 corrupt canonical root"
    );
    assert_eq!(validation.snapshot(), Some(first), "V5/E3");

    let mut terminal = persisted("terminal-incomplete", false, "idle");
    terminal
        .install_initial_event_plan(
            SessionInitialEventPlan::compile(
                "terminal-incomplete",
                "initial:terminal-incomplete",
                vec![SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("accepted before cutover")],
                }],
            )
            .expect("valid retained Event batch"),
        )
        .expect("install retained Event batch");
    terminal.execution = awaken_session_contract::SessionExecutionState::Running;
    terminal
        .transition_execution(awaken_session_contract::SessionExecutionState::Terminated)
        .expect("terminal aggregate");
    create(inner.as_ref(), terminal).await;
    let mut restoring = super::continuation::hibernated_session("restore-in-progress", 100_000);
    restoring
        .environment
        .begin_restore("restore-in-progress", 0, None, 1)
        .expect("valid Restoring fixture");
    create(inner.as_ref(), restoring).await;

    let second = application
        .refresh_event_batch_cutover_validation(2)
        .await
        .expect("V6 complete scan and count");
    assert_eq!(
        second,
        SessionEventBatchCutoverValidationSnapshot {
            generation: 2,
            terminal_with_incomplete_event_batches: 1,
            event_batch_failures: 2,
            quarantined: 0,
            restoring_sessions: 1,
        },
        "V6/E4"
    );
    assert_eq!(
        serde_json::to_value(second).expect("secret-free JSON"),
        serde_json::json!({
            "generation": 2,
            "terminal_with_incomplete_event_batches": 1,
            "event_batch_failures": 2,
            "quarantined": 0,
            "restoring_sessions": 1,
        }),
        "V6 exposes exactly the five frozen fields"
    );
}
