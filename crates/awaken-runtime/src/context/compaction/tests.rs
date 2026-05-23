use super::*;
use awaken_contract::contract::message::ToolCall;
use serde_json::json;

fn long_user(text: &str, copies: usize) -> Arc<Message> {
    Arc::new(Message::user(text.repeat(copies)))
}

fn store_with_compaction_plugin() -> StateStore {
    let store = StateStore::new();
    store
        .install_plugin(super::super::plugin::CompactionPlugin::default())
        .unwrap();
    store
}

fn completed_event(boundary_id: &str, summary: &str, pre_tokens: u64) -> serde_json::Value {
    json!({
        "kind": "custom",
        "task_id": "bg_99",
        "event_type": COMPACTION_COMPLETED_EVENT,
        "payload": {
            "boundary_message_id": boundary_id,
            "summary": summary,
            "pre_tokens": pre_tokens,
        },
    })
}

fn failed_event(boundary_id: &str, error_text: &str) -> serde_json::Value {
    json!({
        "kind": "custom",
        "task_id": "bg_99",
        "event_type": COMPACTION_FAILED_EVENT,
        "payload": {
            "boundary_message_id": boundary_id,
            "error": error_text,
        },
    })
}

fn mark_in_flight(store: &StateStore, boundary_id: &str) {
    let mut batch = MutationBatch::new();
    batch.update::<CompactionStateKey>(record_compaction_in_flight(CompactionInFlight {
        task_id: "bg_99".into(),
        boundary_message_id: boundary_id.into(),
        started_at_ms: 1,
    }));
    store.commit(batch).unwrap();
}

#[test]
fn try_consume_compaction_event_swaps_messages_and_records_boundary() {
    let store = store_with_compaction_plugin();
    let mut messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("OLD-1")),
        Arc::new(Message::assistant("OLD-2")),
        Arc::new(Message::user("BOUNDARY")),
        Arc::new(Message::assistant("AFTER")),
        // simulates a user message that arrived during the compaction window
        Arc::new(Message::user("RACE-NEW")),
    ];
    let boundary_id = messages[2].id.clone().unwrap();
    mark_in_flight(&store, &boundary_id);

    let consumed = try_consume_compaction_event(
        &mut messages,
        &completed_event(&boundary_id, "the summary", 4321),
        &store,
    );
    assert!(consumed, "must report the event was consumed");

    // Swap happened: summary at front, race-new message preserved.
    assert!(
        messages[0]
            .text()
            .contains("<conversation-summary>\nthe summary"),
        "summary not at front: {}",
        messages[0].text()
    );
    assert_eq!(messages[1].text(), "AFTER");
    assert_eq!(messages[2].text(), "RACE-NEW");
    assert_eq!(messages.len(), 3);

    let state = store.read::<CompactionStateKey>().unwrap();
    assert!(!state.is_compacting(), "in-flight must be cleared");
    assert_eq!(state.boundaries.len(), 1, "boundary must be recorded");
    assert!(state.failures.is_empty(), "success must not record failure");
    assert_eq!(state.boundaries[0].summary, "the summary");
}

#[test]
fn try_consume_compaction_event_skips_swap_when_boundary_no_longer_present() {
    let store = store_with_compaction_plugin();
    let mut messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("only-msg")),
        Arc::new(Message::assistant("only-reply")),
    ];
    mark_in_flight(&store, "ghost-boundary-id");

    let consumed = try_consume_compaction_event(
        &mut messages,
        &completed_event("ghost-boundary-id", "irrelevant", 0),
        &store,
    );
    assert!(consumed);

    // No mutation: skip is benign.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "only-msg");

    let state = store.read::<CompactionStateKey>().unwrap();
    assert!(
        !state.is_compacting(),
        "in-flight must clear even on benign skip"
    );
    assert!(
        state.boundaries.is_empty(),
        "no boundary should be recorded when swap was skipped"
    );
}

#[test]
fn try_consume_compaction_event_clears_in_flight_on_failure() {
    let store = store_with_compaction_plugin();
    let mut messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("x"))];
    mark_in_flight(&store, "any");

    let consumed =
        try_consume_compaction_event(&mut messages, &failed_event("any", "boom"), &store);
    assert!(consumed);

    let state = store.read::<CompactionStateKey>().unwrap();
    assert!(!state.is_compacting());
    assert!(
        state.boundaries.is_empty(),
        "failure must not record a boundary"
    );
    assert_eq!(state.failures.len(), 1, "failure must be recorded");
    assert_eq!(state.failures[0].task_id.as_deref(), Some("bg_99"));
    assert_eq!(state.failures[0].boundary_message_id, "any");
    assert_eq!(state.failures[0].error, "boom");
}

#[test]
fn try_consume_compaction_event_passes_through_unrelated_payloads() {
    let store = store_with_compaction_plugin();
    let mut messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("x"))];

    // Other Custom event: not for compaction.
    let other = json!({
        "kind": "custom",
        "task_id": "bg_42",
        "event_type": "task.heartbeat",
        "payload": {"pct": 50},
    });
    assert!(!try_consume_compaction_event(&mut messages, &other, &store));

    // Plain non-Custom payload.
    let task_completed = json!({
        "kind": "completed",
        "task_id": "bg_43",
        "result": null,
    });
    assert!(!try_consume_compaction_event(
        &mut messages,
        &task_completed,
        &store
    ));
}

#[test]
fn plan_compaction_returns_none_when_savings_below_threshold() {
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("hi")),
        Arc::new(Message::assistant("hello")),
        Arc::new(Message::user("how are you?")),
        Arc::new(Message::assistant("fine")),
    ];
    let policy = ContextWindowPolicy {
        compaction_raw_suffix_messages: 1,
        ..Default::default()
    };
    assert!(plan_compaction(&messages, &policy).is_none());
}

#[test]
fn plan_compaction_captures_boundary_message_id() {
    // Pad the head with enough tokens to clear MIN_COMPACTION_GAIN_TOKENS.
    let mut messages: Vec<Arc<Message>> = (0..6)
        .map(|i| {
            if i % 2 == 0 {
                long_user("filler ", 600)
            } else {
                Arc::new(Message::assistant("ack"))
            }
        })
        .collect();
    messages.push(Arc::new(Message::user("recent")));
    let policy = ContextWindowPolicy {
        compaction_raw_suffix_messages: 1,
        ..Default::default()
    };
    let plan = plan_compaction(&messages, &policy).expect("plan");
    // Boundary id must reference an actual message in the snapshot.
    assert!(
        messages
            .iter()
            .any(|m| m.id.as_deref() == Some(plan.boundary_message_id.as_str()))
    );
    assert!(plan.pre_tokens >= MIN_COMPACTION_GAIN_TOKENS);
    assert!(!plan.transcript.is_empty());
}

#[test]
fn apply_summary_swaps_when_boundary_present() {
    let mut messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("old1")),
        Arc::new(Message::assistant("old2")),
        Arc::new(Message::user("BOUNDARY")),
        Arc::new(Message::assistant("after-boundary")),
        Arc::new(Message::user("appended-during-window")),
    ];
    let boundary_id = messages[2].id.clone().unwrap();

    let applied = apply_summary(&mut messages, &boundary_id, "synthetic summary").unwrap();
    assert_eq!(applied.boundary_index, 2);
    assert!(applied.pre_tokens > 0);
    assert!(applied.post_tokens > 0);

    // First message must now be the summary; messages after the boundary
    // (including ones appended during the compaction window) are kept.
    assert!(
        messages[0]
            .text()
            .contains("<conversation-summary>\nsynthetic summary"),
        "summary missing or malformed: {}",
        messages[0].text()
    );
    assert_eq!(messages[1].text(), "after-boundary");
    assert_eq!(messages[2].text(), "appended-during-window");
    assert_eq!(messages.len(), 3);
}

#[test]
fn apply_summary_returns_none_when_boundary_already_gone() {
    let mut messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("a")),
        Arc::new(Message::assistant("b")),
    ];
    let original = messages.clone();
    assert!(apply_summary(&mut messages, "non-existent-id", "any").is_none());
    // Skip must be benign: the live list is unchanged.
    assert_eq!(messages.len(), original.len());
    for (a, b) in messages.iter().zip(original.iter()) {
        assert_eq!(a.text(), b.text());
    }
}

#[test]
fn find_compaction_boundary_respects_tool_pairs() {
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("start")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "search", json!({}))],
        )),
        Arc::new(Message::tool("c1", "found")),
        Arc::new(Message::user("next")), // safe boundary here (idx 3)
        Arc::new(Message::assistant("reply")),
    ];

    let boundary = find_compaction_boundary(&messages, 0, messages.len());
    // Should be at idx 3 or 4 (after tool pair is complete)
    assert!(boundary.is_some());
    let b = boundary.unwrap();
    assert!(b >= 3);
}

#[test]
fn trim_to_compaction_boundary_drops_pre_summary() {
    let mut messages = vec![
        Arc::new(Message::user("old msg 1")),
        Arc::new(Message::assistant("old reply")),
        Arc::new(Message::internal_system(
            "<conversation-summary>\nSummary of old messages\n</conversation-summary>",
        )),
        Arc::new(Message::user("new msg")),
        Arc::new(Message::assistant("new reply")),
    ];

    trim_to_compaction_boundary(&mut messages);
    assert_eq!(messages.len(), 3);
    assert!(messages[0].text().contains("conversation-summary"));
    assert_eq!(messages[1].text(), "new msg");
}

#[test]
fn trim_to_compaction_boundary_noop_without_summary() {
    let mut messages = vec![
        Arc::new(Message::user("hello")),
        Arc::new(Message::assistant("hi")),
    ];
    let len_before = messages.len();
    trim_to_compaction_boundary(&mut messages);
    assert_eq!(messages.len(), len_before);
}

#[test]
fn find_compaction_boundary_does_not_cut_open_tool_round() {
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("start")),
        Arc::new(Message::assistant("reply")),
        Arc::new(Message::user("next")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "search", json!({}))],
        )),
        // c1 has no result yet — open tool round
    ];

    let boundary = find_compaction_boundary(&messages, 0, messages.len());
    // Boundary should be before the open tool round (idx 2 at latest)
    if let Some(b) = boundary {
        assert!(b <= 2, "boundary should not include open tool round");
    }
}

#[test]
fn trim_to_compaction_boundary_idempotent() {
    let mut messages = vec![
        Arc::new(Message::user("old")),
        Arc::new(Message::internal_system(
            "<conversation-summary>\nSummary\n</conversation-summary>",
        )),
        Arc::new(Message::user("new")),
    ];

    trim_to_compaction_boundary(&mut messages);
    let len_after_first = messages.len();

    trim_to_compaction_boundary(&mut messages);
    assert_eq!(
        messages.len(),
        len_after_first,
        "second trim should be noop"
    );
}

#[test]
fn find_boundary_skips_open_tool_rounds() {
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("start")),
        Arc::new(Message::assistant("ok")),
        Arc::new(Message::user("do something")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "search", json!({}))],
        )),
        // c1 result is missing — open tool round
    ];

    let boundary = find_compaction_boundary(&messages, 0, messages.len());
    // Must not place boundary at or after the open tool call (idx 3)
    if let Some(b) = boundary {
        assert!(b < 3, "boundary {b} must be before open tool call at idx 3");
    }
}

#[test]
fn find_boundary_respects_suffix_messages() {
    // Search only within a sub-range, leaving suffix messages untouched
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("old1")),
        Arc::new(Message::assistant("reply1")),
        Arc::new(Message::user("old2")),
        Arc::new(Message::assistant("reply2")),
        // suffix: last 2 messages are "raw suffix"
        Arc::new(Message::user("recent")),
        Arc::new(Message::assistant("recent_reply")),
    ];

    let suffix_count = 2;
    let search_end = messages.len().saturating_sub(suffix_count);
    let boundary = find_compaction_boundary(&messages, 0, search_end);
    // Boundary must be within the searched range, not touching suffix
    if let Some(b) = boundary {
        assert!(
            b < search_end,
            "boundary {b} must be before suffix start {search_end}"
        );
    }
}

#[test]
fn find_boundary_returns_none_when_too_few_messages() {
    // Single message — no safe compaction point
    let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("only message"))];
    // Search range is empty (start == end)
    let boundary = find_compaction_boundary(&messages, 0, 0);
    assert!(boundary.is_none(), "empty range should yield no boundary");

    // Range with only an open tool call — no safe boundary
    let messages2: Vec<Arc<Message>> = vec![Arc::new(Message::assistant_with_tool_calls(
        "",
        vec![ToolCall::new("c1", "fn", json!({}))],
    ))];
    let boundary2 = find_compaction_boundary(&messages2, 0, messages2.len());
    assert!(
        boundary2.is_none(),
        "single open tool call should yield no boundary"
    );
}

#[test]
fn find_compaction_boundary_multiple_complete_tool_rounds() {
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("start")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c1", "search", json!({}))],
        )),
        Arc::new(Message::tool("c1", "found it")),
        Arc::new(Message::user("next")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new("c2", "read", json!({}))],
        )),
        Arc::new(Message::tool("c2", "content")),
        Arc::new(Message::user("last")),
        Arc::new(Message::assistant("done")),
    ];

    let boundary = find_compaction_boundary(&messages, 0, messages.len());
    assert!(boundary.is_some());
    // Should be at or after idx 6 (after second tool round)
    let b = boundary.unwrap();
    assert!(
        b >= 6,
        "boundary should be after all tool rounds: got {}",
        b
    );
}

#[test]
fn find_compaction_boundary_empty_range() {
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("hello")),
        Arc::new(Message::assistant("hi")),
    ];
    let boundary = find_compaction_boundary(&messages, 0, 0);
    assert!(boundary.is_none(), "empty range should yield no boundary");
}

#[test]
fn find_compaction_boundary_range_start_equals_end() {
    let messages: Vec<Arc<Message>> = vec![Arc::new(Message::user("only"))];
    let boundary = find_compaction_boundary(&messages, 1, 1);
    assert!(boundary.is_none());
}

#[test]
fn trim_to_compaction_boundary_uses_last_summary() {
    let mut messages = vec![
        Arc::new(Message::user("old msg 1")),
        Arc::new(Message::internal_system(
            "<conversation-summary>\nFirst summary\n</conversation-summary>",
        )),
        Arc::new(Message::user("mid msg")),
        Arc::new(Message::internal_system(
            "<conversation-summary>\nSecond summary\n</conversation-summary>",
        )),
        Arc::new(Message::user("new msg")),
    ];

    trim_to_compaction_boundary(&mut messages);
    // Should trim to the LAST summary (index 3)
    assert_eq!(messages.len(), 2);
    assert!(messages[0].text().contains("Second summary"));
    assert_eq!(messages[1].text(), "new msg");
}

#[test]
fn find_compaction_boundary_with_multiple_tool_calls_in_one_round() {
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("do things")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new("c1", "search", json!({})),
                ToolCall::new("c2", "read", json!({})),
            ],
        )),
        Arc::new(Message::tool("c1", "found")),
        Arc::new(Message::tool("c2", "content")),
        Arc::new(Message::user("thanks")),
    ];

    let boundary = find_compaction_boundary(&messages, 0, messages.len());
    assert!(boundary.is_some());
    // Both tool results are present, so boundary can be after them
    let b = boundary.unwrap();
    assert!(
        b >= 3,
        "boundary should be after all tool results: got {}",
        b
    );
}

#[test]
fn find_compaction_boundary_partial_tool_results() {
    // Two tool calls but only one result
    let messages: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("start")),
        Arc::new(Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new("c1", "search", json!({})),
                ToolCall::new("c2", "read", json!({})),
            ],
        )),
        Arc::new(Message::tool("c1", "found")),
        // c2 result missing
    ];

    let boundary = find_compaction_boundary(&messages, 0, messages.len());
    // Should not place boundary after the incomplete tool round
    if let Some(b) = boundary {
        assert!(b < 1, "boundary should not include incomplete tool round");
    }
}
