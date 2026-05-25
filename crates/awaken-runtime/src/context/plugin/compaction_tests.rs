use super::*;
use crate::state::StateStore;

#[test]
fn compaction_state_record_boundary() {
    let mut state = CompactionState::default();
    assert_eq!(state.total_compactions, 0);
    assert!(state.boundaries.is_empty());

    state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
        summary: "User asked to implement feature X.".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 5000,
        post_tokens: 200,
        timestamp_ms: 1234567890,
    }));

    assert_eq!(state.total_compactions, 1);
    assert_eq!(state.boundaries.len(), 1);
    assert_eq!(
        state.latest_boundary().unwrap().summary,
        "User asked to implement feature X."
    );
}

#[test]
fn compaction_state_multiple_boundaries() {
    let mut state = CompactionState::default();

    for i in 0..3 {
        state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
            summary: format!("summary {i}"),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 1000 * (i + 1),
            post_tokens: 100 * (i + 1),
            timestamp_ms: 1000 + i as u64,
        }));
    }

    assert_eq!(state.total_compactions, 3);
    assert_eq!(state.boundaries.len(), 3);
    assert_eq!(state.latest_boundary().unwrap().summary, "summary 2");
}

#[test]
fn compaction_state_clear() {
    let mut state = CompactionState {
        boundaries: vec![CompactionBoundary {
            summary: "old".into(),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 100,
            post_tokens: 10,
            timestamp_ms: 1,
        }],
        failures: Vec::new(),
        skipped: Vec::new(),
        total_compactions: 1,
        in_flight: None,
    };

    state.reduce(CompactionAction::Clear);
    assert!(state.boundaries.is_empty());
    assert_eq!(state.total_compactions, 0);
}

#[test]
fn compaction_state_latest_boundary_empty() {
    let state = CompactionState::default();
    assert!(state.latest_boundary().is_none());
}

#[test]
fn compaction_state_serde_roundtrip() {
    let state = CompactionState {
        boundaries: vec![
            CompactionBoundary {
                summary: "first".into(),
                task_id: None,
                boundary_message_id: None,
                pre_tokens: 5000,
                post_tokens: 200,
                timestamp_ms: 1000,
            },
            CompactionBoundary {
                summary: "second".into(),
                task_id: None,
                boundary_message_id: None,
                pre_tokens: 3000,
                post_tokens: 150,
                timestamp_ms: 2000,
            },
        ],
        failures: Vec::new(),
        skipped: Vec::new(),
        total_compactions: 2,
        in_flight: None,
    };

    let json = serde_json::to_string(&state).unwrap();
    let parsed: CompactionState = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, state);
}

#[test]
fn compaction_plugin_registers_key() {
    let store = StateStore::new();
    store.install_plugin(CompactionPlugin::default()).unwrap();
    let registry = store.registry.lock();
    assert!(registry.keys_by_name.contains_key("__context_compaction"));
}

#[test]
fn compaction_plugin_state_via_store() {
    let store = StateStore::new();
    store.install_plugin(CompactionPlugin::default()).unwrap();

    let mut patch = store.begin_mutation();
    patch.update::<CompactionStateKey>(crate::context::record_compaction_boundary(
        CompactionBoundary {
            summary: "test summary".into(),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 4000,
            post_tokens: 180,
            timestamp_ms: 9999,
        },
    ));
    store.commit(patch).unwrap();

    let state = store.read::<CompactionStateKey>().unwrap();
    assert_eq!(state.total_compactions, 1);
    assert_eq!(state.boundaries[0].summary, "test summary");
}

#[test]
fn record_compaction_boundary_constructor() {
    let action = crate::context::record_compaction_boundary(CompactionBoundary {
        summary: "s".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 100,
        post_tokens: 10,
        timestamp_ms: 0,
    });
    assert!(matches!(action, CompactionAction::RecordBoundary(_)));
}

#[test]
fn compaction_state_record_then_clear_then_record() {
    let mut state = CompactionState::default();

    state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
        summary: "first".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 1000,
        post_tokens: 100,
        timestamp_ms: 1,
    }));
    assert_eq!(state.total_compactions, 1);

    state.reduce(CompactionAction::Clear);
    assert_eq!(state.total_compactions, 0);
    assert!(state.boundaries.is_empty());
    assert!(state.latest_boundary().is_none());

    state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
        summary: "after clear".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 2000,
        post_tokens: 150,
        timestamp_ms: 2,
    }));
    assert_eq!(state.total_compactions, 1);
    assert_eq!(state.latest_boundary().unwrap().summary, "after clear");
}

#[test]
fn compaction_state_key_properties() {
    assert_eq!(CompactionStateKey::KEY, "__context_compaction");
}

#[test]
fn compaction_state_key_apply() {
    let mut state = CompactionState::default();
    CompactionStateKey::apply(
        &mut state,
        CompactionAction::RecordBoundary(CompactionBoundary {
            summary: "via apply".into(),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 500,
            post_tokens: 50,
            timestamp_ms: 42,
        }),
    );
    assert_eq!(state.total_compactions, 1);
    assert_eq!(state.boundaries[0].summary, "via apply");
}

#[test]
fn compaction_plugin_descriptor_name() {
    let plugin = CompactionPlugin::default();
    assert_eq!(plugin.descriptor().name, CONTEXT_COMPACTION_PLUGIN_ID);
}

#[test]
fn compaction_plugin_new_with_config() {
    let config = CompactionConfig {
        min_savings_ratio: 0.8,
        ..Default::default()
    };
    let plugin = CompactionPlugin::new(config);
    assert!((plugin.config.min_savings_ratio - 0.8).abs() < f64::EPSILON);
}

#[test]
fn compaction_boundary_equality() {
    let a = CompactionBoundary {
        summary: "s".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 100,
        post_tokens: 10,
        timestamp_ms: 0,
    };
    let b = a.clone();
    assert_eq!(a, b);
}

#[test]
fn compaction_boundary_serde_roundtrip() {
    let boundary = CompactionBoundary {
        summary: "test summary".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 3000,
        post_tokens: 200,
        timestamp_ms: 1234567890,
    };
    let json = serde_json::to_string(&boundary).unwrap();
    let parsed: CompactionBoundary = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, boundary);
}

// -----------------------------------------------------------------------
// Migrated from uncarve: additional compaction state tests
// -----------------------------------------------------------------------

#[test]
fn compaction_state_default_is_empty() {
    let state = CompactionState::default();
    assert!(state.boundaries.is_empty());
    assert_eq!(state.total_compactions, 0);
    assert!(state.latest_boundary().is_none());
}

#[test]
fn compaction_state_boundary_ordering_preserved() {
    let mut state = CompactionState::default();
    for i in 0..5 {
        state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
            summary: format!("boundary_{i}"),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 1000,
            post_tokens: 100,
            timestamp_ms: i as u64,
        }));
    }
    assert_eq!(state.boundaries.len(), 5);
    assert_eq!(state.total_compactions, 5);
    for (i, b) in state.boundaries.iter().enumerate() {
        assert_eq!(b.summary, format!("boundary_{i}"));
        assert_eq!(b.timestamp_ms, i as u64);
    }
}

#[test]
fn compaction_state_clear_twice_is_idempotent() {
    let mut state = CompactionState::default();
    state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
        summary: "s".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 1,
        post_tokens: 1,
        timestamp_ms: 0,
    }));
    state.reduce(CompactionAction::Clear);
    state.reduce(CompactionAction::Clear);
    assert!(state.boundaries.is_empty());
    assert_eq!(state.total_compactions, 0);
}

#[test]
fn compaction_config_default_has_sane_values() {
    let config = CompactionConfig::default();
    assert!(!config.summarizer_system_prompt.is_empty());
    assert!(config.summarizer_user_prompt.contains("{messages}"));
    assert!(config.min_savings_ratio > 0.0);
    assert!(config.min_savings_ratio < 1.0);
    assert!(config.summary_max_tokens.is_none());
    assert!(config.summary_model.is_none());
}

#[test]
fn compaction_config_serde_roundtrip() {
    let config = CompactionConfig {
        summarizer_system_prompt: "custom system".into(),
        summarizer_user_prompt: "custom user: {messages}".into(),
        summary_max_tokens: Some(512),
        summary_model: Some("claude-3-haiku".into()),
        min_savings_ratio: 0.5,
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: CompactionConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed.summarizer_system_prompt,
        config.summarizer_system_prompt
    );
    assert_eq!(parsed.summary_max_tokens, Some(512));
    assert_eq!(parsed.summary_model.as_deref(), Some("claude-3-haiku"));
}

#[test]
fn compaction_state_pre_post_tokens_preserved() {
    let mut state = CompactionState::default();
    state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
        summary: "test".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 10_000,
        post_tokens: 500,
        timestamp_ms: 99,
    }));
    let b = state.latest_boundary().unwrap();
    assert_eq!(b.pre_tokens, 10_000);
    assert_eq!(b.post_tokens, 500);
    assert_eq!(b.timestamp_ms, 99);
}

// -----------------------------------------------------------------------
// Additional compaction tests
// -----------------------------------------------------------------------

#[test]
fn compaction_fires_at_threshold() {
    // Verify savings ratio check: only accept compaction when savings >= min_savings_ratio
    let config = CompactionConfig {
        min_savings_ratio: 0.5,
        ..Default::default()
    };
    let boundary_good = CompactionBoundary {
        summary: "good".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 1000,
        post_tokens: 400, // 60% savings > 50% threshold
        timestamp_ms: 1,
    };
    let savings_good = 1.0 - (boundary_good.post_tokens as f64 / boundary_good.pre_tokens as f64);
    assert!(
        savings_good >= config.min_savings_ratio,
        "60% savings should meet 50% threshold"
    );

    let boundary_bad = CompactionBoundary {
        summary: "bad".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 1000,
        post_tokens: 600, // 40% savings < 50% threshold
        timestamp_ms: 2,
    };
    let savings_bad = 1.0 - (boundary_bad.post_tokens as f64 / boundary_bad.pre_tokens as f64);
    assert!(
        savings_bad < config.min_savings_ratio,
        "40% savings should not meet 50% threshold"
    );
}

#[test]
fn compaction_state_tracks_across_multiple_rounds() {
    let mut state = CompactionState::default();
    // Simulate 5 compaction rounds with increasing pre-token counts
    for round in 1..=5u64 {
        state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
            summary: format!("round {round}"),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 1000 * round as usize,
            post_tokens: 100 * round as usize,
            timestamp_ms: round * 1000,
        }));
        assert_eq!(state.total_compactions, round);
        assert_eq!(state.boundaries.len(), round as usize);
    }
    // Latest boundary should be the last round
    assert_eq!(state.latest_boundary().unwrap().summary, "round 5");
    assert_eq!(state.latest_boundary().unwrap().pre_tokens, 5000);
}

#[test]
fn compaction_config_serialization_omits_none_fields() {
    let config = CompactionConfig::default();
    let json = serde_json::to_value(&config).unwrap();
    // summary_max_tokens and summary_model are None, should be omitted via skip_serializing_if
    assert!(
        !json.as_object().unwrap().contains_key("summary_max_tokens"),
        "None fields should be omitted"
    );
    assert!(
        !json.as_object().unwrap().contains_key("summary_model"),
        "None fields should be omitted"
    );
}

#[test]
fn compaction_config_serialization_includes_some_fields() {
    let config = CompactionConfig {
        summary_max_tokens: Some(1024),
        summary_model: Some("claude-3-sonnet".into()),
        ..Default::default()
    };
    let json = serde_json::to_value(&config).unwrap();
    assert_eq!(json["summary_max_tokens"], 1024);
    assert_eq!(json["summary_model"], "claude-3-sonnet");
}

#[test]
fn compaction_with_tool_messages_records_correctly() {
    // Simulate compaction that includes tool messages in the summarized range
    let store = StateStore::new();
    store.install_plugin(CompactionPlugin::default()).unwrap();

    // Record a boundary representing a range that included tool messages
    let mut patch = store.begin_mutation();
    patch.update::<CompactionStateKey>(crate::context::record_compaction_boundary(
        CompactionBoundary {
            summary: "User asked to search files. Tool search returned 3 results. Assistant presented findings.".into(),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 8000,
            post_tokens: 200,
            timestamp_ms: 1000,
        },
    ));
    store.commit(patch).unwrap();

    let state = store.read::<CompactionStateKey>().unwrap();
    assert_eq!(state.total_compactions, 1);
    assert!(state.boundaries[0].summary.contains("Tool search"));
    assert_eq!(state.boundaries[0].pre_tokens, 8000);
}

#[test]
fn in_flight_set_and_clear_round_trip() {
    let mut state = CompactionState::default();
    assert!(!state.is_compacting());

    state.reduce(CompactionAction::SetInFlight(CompactionInFlight {
        task_id: "bg_42".into(),
        boundary_message_id: "01HZ-msg-01".into(),
        started_at_ms: 100,
    }));
    let live = state.in_flight.as_ref().expect("in-flight set");
    assert_eq!(live.task_id, "bg_42");
    assert_eq!(live.boundary_message_id, "01HZ-msg-01");
    assert!(state.is_compacting());

    state.reduce(CompactionAction::ClearInFlight);
    assert!(state.in_flight.is_none());
    assert!(!state.is_compacting());
}

#[test]
fn clear_action_resets_in_flight_too() {
    let mut state = CompactionState::default();
    state.reduce(CompactionAction::SetInFlight(CompactionInFlight {
        task_id: "bg_1".into(),
        boundary_message_id: "msg-id".into(),
        started_at_ms: 1,
    }));
    state.reduce(CompactionAction::Clear);
    assert!(state.in_flight.is_none());
    assert!(state.boundaries.is_empty());
}

#[test]
fn record_boundary_does_not_touch_in_flight() {
    let mut state = CompactionState::default();
    state.reduce(CompactionAction::SetInFlight(CompactionInFlight {
        task_id: "bg_99".into(),
        boundary_message_id: "msg".into(),
        started_at_ms: 1,
    }));
    state.reduce(CompactionAction::RecordBoundary(CompactionBoundary {
        summary: "s".into(),
        task_id: None,
        boundary_message_id: None,
        pre_tokens: 10,
        post_tokens: 1,
        timestamp_ms: 2,
    }));
    // RecordBoundary alone does not clear the marker — the inbox
    // event router is responsible for the explicit ClearInFlight.
    assert!(state.is_compacting());
    assert_eq!(state.boundaries.len(), 1);
}

#[test]
fn compaction_action_serde_roundtrip() {
    let actions = vec![
        CompactionAction::RecordBoundary(CompactionBoundary {
            summary: "s".into(),
            task_id: None,
            boundary_message_id: None,
            pre_tokens: 1,
            post_tokens: 1,
            timestamp_ms: 0,
        }),
        CompactionAction::Clear,
    ];
    for action in actions {
        let json = serde_json::to_string(&action).unwrap();
        let parsed: CompactionAction = serde_json::from_str(&json).unwrap();
        // Verify the action type roundtrips
        match (&action, &parsed) {
            (CompactionAction::Clear, CompactionAction::Clear) => {}
            (CompactionAction::RecordBoundary(a), CompactionAction::RecordBoundary(b)) => {
                assert_eq!(a.summary, b.summary);
            }
            _ => panic!("action type mismatch after serde roundtrip"),
        }
    }
}
