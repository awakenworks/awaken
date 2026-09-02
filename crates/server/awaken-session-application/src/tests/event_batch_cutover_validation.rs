use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_contract::{SessionEventInput, SessionInitialEventPlan};

use super::*;

#[tokio::test]
async fn final_authority_scan_is_the_only_event_batch_cutover_generation_trigger() {
    // Cause/effect graph: C1 no final authority scan has completed; C2 an empty
    // scan succeeds; C3 the next final repository scan is unavailable; C4 a
    // later successful scan contains 256 active prefix rows followed by one
    // terminal Session with an incomplete Event batch, while this cycle
    // observed two Event-batch failures. Effects:
    // E1 C1 has no snapshot; E2 C2 publishes generation one and zero counts;
    // E3 C3 returns the outage and preserves E2 byte-for-byte; E4 C4 publishes
    // generation two with exact nonzero counts. Quarantine counting is covered
    // by the reducer test; the admin response test owns HTTP fail-closed mapping.
    //
    // | Rule | Final scan | Terminal incomplete | Batch failures | Effect |
    // |---|---|---:|---:|---|
    // | V1 | none yet | n/a | n/a | no snapshot |
    // | V2 | success | 0 | 0 | generation 1, zero counts |
    // | V3 | unavailable | unknown | any | error, V2 unchanged |
    // | V4 | two-page success; terminal on page 2 | 1 | 2 | generation 2, exact counts |
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

    for index in 0..256 {
        create(
            inner.as_ref(),
            persisted(&format!("cutover-a-active-{index:03}"), true, "idle"),
        )
        .await;
    }
    let mut terminal = persisted("cutover-z-terminal-incomplete", false, "idle");
    terminal
        .install_initial_event_plan(
            SessionInitialEventPlan::compile(
                "cutover-z-terminal-incomplete",
                "initial:cutover-z-terminal-incomplete",
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

    let second = application
        .refresh_event_batch_cutover_validation(2)
        .await
        .expect("V4 complete scan");
    assert_eq!(
        second,
        SessionEventBatchCutoverValidationSnapshot {
            generation: 2,
            terminal_with_incomplete_event_batches: 1,
            event_batch_failures: 2,
            quarantined: 0,
        },
        "V4/E4"
    );
    assert_eq!(
        serde_json::to_value(second).expect("secret-free JSON"),
        serde_json::json!({
            "generation": 2,
            "terminal_with_incomplete_event_batches": 1,
            "event_batch_failures": 2,
            "quarantined": 0,
        }),
        "V4 exposes exactly the four frozen fields"
    );
}
