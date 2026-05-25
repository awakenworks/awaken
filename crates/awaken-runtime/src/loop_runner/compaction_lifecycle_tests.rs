use super::*;

/// End-to-end success: spawn → release → drain inbox → consume event →
/// messages compacted in place AND in-flight cleared AND a boundary
/// recorded. This is the full happy-path closing the loop the
/// orchestrator runs in production.
#[tokio::test]
async fn round_trip_swap_completes_after_event_drained() {
    use crate::context::try_consume_compaction_event;

    let manager = Arc::new(BackgroundTaskManager::new());
    let (runtime, store, env) = make_phase_runtime(&manager);
    let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
    manager.set_owner_inbox(inbox_tx);

    let gate = Arc::new(Notify::new());
    let summarizer: Arc<dyn ContextSummarizer> = Arc::new(GatedSummarizer {
        gate: gate.clone(),
        summary: "round trip summary".into(),
        observed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let mut agent = make_resolved_agent(manager.clone(), summarizer);
    agent.env = env;
    let mut messages = make_long_messages();
    let original_len = messages.len();
    let identity = run_identity("thread-round-trip");
    let cancel = CancellationToken::new();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut truncation = TruncationState::default();
    let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

    {
        let mut ctx = StepContext {
            agent: &mut agent,
            messages: &mut messages,
            runtime: &runtime,
            sink: sink.clone(),
            checkpoint_store: None,
            commit: crate::loop_runner::CommitWiring::default(),
            run_identity: &identity,
            input_message_count: 0,
            cancellation_token: Some(&cancel),
            run_overrides: &None,
            total_input_tokens: &mut total_in,
            total_output_tokens: &mut total_out,
            truncation_state: &mut truncation,
            run_created_at: 0,
            thread_ctx: None,
        };
        assert!(maybe_spawn_compaction(&mut ctx, &default_policy()).await);
    }

    let started = recv_inbox_event(&mut inbox_rx).await;
    assert_eq!(started["event_type"], COMPACTION_STARTED_EVENT);
    assert!(try_consume_compaction_event(
        &mut messages,
        &started,
        runtime.store()
    ));

    gate.notify_one();
    let payload = recv_inbox_event(&mut inbox_rx).await;

    let consumed = try_consume_compaction_event(&mut messages, &payload, runtime.store());
    assert!(consumed, "router must claim compaction event");
    assert!(
        messages[0]
            .text()
            .contains("<conversation-summary>\nround trip summary"),
        "summary not at front: {}",
        messages[0].text()
    );
    assert!(
        messages.len() < original_len,
        "compaction must shrink the message list (was {original_len}, now {})",
        messages.len()
    );

    let final_state = store.read::<CompactionStateKey>().unwrap();
    assert!(!final_state.is_compacting(), "in-flight must be cleared");
    assert_eq!(
        final_state.boundaries.len(),
        1,
        "one boundary must be recorded"
    );
    assert_eq!(final_state.boundaries[0].summary, "round trip summary");
}

#[tokio::test]
async fn round_trip_skip_records_min_savings_rejection() {
    use crate::context::try_consume_compaction_event;

    let manager = Arc::new(BackgroundTaskManager::new());
    let (runtime, store, env) = make_phase_runtime(&manager);
    let (inbox_tx, mut inbox_rx) = crate::inbox::inbox_channel();
    manager.set_owner_inbox(inbox_tx);

    let gate = Arc::new(Notify::new());
    let summarizer: Arc<dyn ContextSummarizer> = Arc::new(GatedSummarizer {
        gate: gate.clone(),
        summary: "oversized summary ".repeat(5000),
        observed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let mut agent = make_resolved_agent(manager.clone(), summarizer);
    agent.env = env;
    set_compaction_config(
        &mut agent,
        CompactionConfig {
            min_savings_ratio: 0.95,
            ..Default::default()
        },
    );
    let mut messages = make_long_messages();
    let original = messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    let identity = run_identity("thread-skip");
    let cancel = CancellationToken::new();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut truncation = TruncationState::default();
    let sink: Arc<dyn EventSink> = Arc::new(NullEventSink);

    {
        let mut ctx = StepContext {
            agent: &mut agent,
            messages: &mut messages,
            runtime: &runtime,
            sink: sink.clone(),
            checkpoint_store: None,
            commit: crate::loop_runner::CommitWiring::default(),
            run_identity: &identity,
            input_message_count: 0,
            cancellation_token: Some(&cancel),
            run_overrides: &None,
            total_input_tokens: &mut total_in,
            total_output_tokens: &mut total_out,
            truncation_state: &mut truncation,
            run_created_at: 0,
            thread_ctx: None,
        };
        assert!(maybe_spawn_compaction(&mut ctx, &default_policy()).await);
    }

    let started = recv_inbox_event(&mut inbox_rx).await;
    assert_eq!(started["event_type"], COMPACTION_STARTED_EVENT);
    gate.notify_one();
    let skipped = recv_inbox_event(&mut inbox_rx).await;
    assert_eq!(skipped["event_type"], COMPACTION_SKIPPED_EVENT);
    assert_eq!(
        skipped["payload"]["reason"],
        COMPACTION_SKIP_REASON_MIN_SAVINGS_RATIO
    );
    assert!(
        skipped["payload"]["post_tokens"].as_u64().unwrap()
            >= skipped["payload"]["pre_tokens"].as_u64().unwrap()
    );

    let consumed = try_consume_compaction_event(&mut messages, &skipped, runtime.store());
    assert!(consumed);
    assert_eq!(
        messages
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>(),
        original,
        "skip must not mutate the live message list"
    );
    let final_state = store.read::<CompactionStateKey>().unwrap();
    assert!(!final_state.is_compacting(), "in-flight must be cleared");
    assert!(final_state.boundaries.is_empty());
    assert_eq!(final_state.skipped.len(), 1);
    assert_eq!(
        final_state.skipped[0].reason,
        COMPACTION_SKIP_REASON_MIN_SAVINGS_RATIO
    );
}
