use super::*;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

async fn page_state() -> (ManagedState, String) {
    let state = ManagedState::new(LifecycleRuntime::default());
    let session = state
        .create_session(
            serde_json::from_value(
                serde_json::json!({"agent":"coder","environment_id":"env_local"}),
            )
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    (state, session.id)
}

fn message(id: &str) -> Event {
    Event {
        id: id.into(),
        processed_at: None,
        kind: OutboundKind::AgentMessage {
            content: vec![ContentBlock::text(id)],
        },
    }
}

fn tool(id: &str) -> Event {
    Event {
        id: id.into(),
        processed_at: None,
        kind: OutboundKind::AgentToolUse {
            name: "read".into(),
            input: serde_json::json!({"path":"exact"}),
            evaluated_permission: Some(crate::types::EvaluatedPermission::Allow),
            evaluation: None,
            session_thread_id: None,
        },
    }
}

fn action(target: &str) -> Event {
    Event {
        id: "ask".into(),
        processed_at: None,
        kind: OutboundKind::SessionStatusIdle {
            stop_reason: StopReason::RequiresAction {
                event_ids: vec![target.into()],
            },
        },
    }
}

fn reference_page(
    record: &SessionRecord,
    view: &str,
    cursor: Option<&str>,
    limit: Option<usize>,
    descending: bool,
    include: impl Fn(&Event) -> bool,
) -> Option<serde_json::Value> {
    // Independent source of action references: do not consume the index under
    // test. The unchanged visibility kernel and canonical paginator own their
    // existing policies; this oracle retains the former full-scan composition.
    let answerable: HashSet<_> = record
        .events
        .iter()
        .filter(|event| {
            record
                .event_thread_owners
                .get(&event.id)
                .is_none_or(|owner| owner == &record.session.id)
        })
        .filter_map(|event| match &event.kind {
            OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids },
            }
            | OutboundKind::SessionThreadStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids },
                ..
            } => Some(event_ids),
            _ => None,
        })
        .flatten()
        .collect();
    let mut events: Vec<_> = record
        .events
        .iter()
        .filter_map(|event| {
            ManagedState::project_event_for_thread(
                &record.session.id,
                view,
                event.clone(),
                record
                    .event_thread_owners
                    .get(&event.id)
                    .map(String::as_str),
                answerable.contains(&event.id),
            )
        })
        .filter(&include)
        .collect();
    if descending {
        events.reverse();
    }
    let page = paginate_by_id(&events, cursor, limit, |event| event.id.as_str()).ok()?;
    serde_json::to_value(ListEventsResponse {
        data: page.items.to_vec(),
        next_page: page.next_page,
    })
    .ok()
}

#[tokio::test]
async fn indexed_pages_match_filtered_directional_visibility_and_cursor_rules() {
    // Causes: P1 root/child/other view; P2 action boundary unowned/root/child;
    // P3 no/type filter, including a perspective-changing thread message;
    // P4 ascending/descending; P5 absent/blank/padded/present/missing cursor;
    // P6 default/zero/small/max/oversized limit; P7 duplicate id with mixed kinds.
    // Effects: exact wire page/continuation, or the same error for missing,
    // hidden and filtered-out cursors. P7 chooses the first eligible occurrence
    // in the requested direction. Each Cartesian row is compared with the
    // independent scan composition, including its owner-sensitive references.
    let (state, id) = page_state().await;
    let events = vec![
        message("root"),
        message("hidden"),
        tool("tool"),
        action("tool"),
        tool("thread-tool"),
        Event {
            id: "child-idle".into(),
            processed_at: None,
            kind: OutboundKind::SessionThreadStatusIdle {
                session_thread_id: "child".into(),
                agent_name: "Child".into(),
                stop_reason: StopReason::RequiresAction {
                    event_ids: vec!["thread-tool".into()],
                },
            },
        },
        Event {
            id: "mail".into(),
            processed_at: None,
            kind: OutboundKind::AgentThreadMessageSent {
                to_session_thread_id: "child".into(),
                to_agent_name: None,
                content: vec![ContentBlock::text("mail")],
            },
        },
        Event {
            id: "duplicate".into(),
            processed_at: None,
            kind: OutboundKind::AgentThinking {},
        },
        message("duplicate"),
        message("tail"),
    ];
    for action_owner in [None, Some(id.as_str()), Some("child")] {
        state
            .publish_projection_update(&id, |record| {
                record.events = events.clone();
                record.event_thread_owners = HashMap::from([
                    ("hidden".into(), "child".into()),
                    ("tool".into(), "child".into()),
                    ("thread-tool".into(), "child".into()),
                ]);
                if let Some(owner) = action_owner {
                    record
                        .event_thread_owners
                        .insert("ask".into(), owner.into());
                    record
                        .event_thread_owners
                        .insert("child-idle".into(), owner.into());
                }
                Ok(())
            })
            .unwrap();
        let record = state.sessions.lock().unwrap().get(&id).unwrap().clone();
        for view in [id.as_str(), "child", "other"] {
            for kind in [
                None,
                Some("agent.message"),
                Some("agent.tool_use"),
                Some("agent.thread_message_received"),
            ] {
                let include = |event: &Event| kind.is_none_or(|kind| event.kind.type_str() == kind);
                for descending in [false, true] {
                    for limit in [None, Some(0), Some(1), Some(2), Some(500), Some(usize::MAX)] {
                        for cursor in [
                            None,
                            Some(""),
                            Some("  "),
                            Some("root"),
                            Some("hidden"),
                            Some("tool"),
                            Some("thread-tool"),
                            Some("child-idle"),
                            Some("ask"),
                            Some("mail"),
                            Some("duplicate"),
                            Some(" duplicate "),
                            Some("tail"),
                            Some("absent"),
                        ] {
                            let actual = ManagedState::project_event_page(
                                &record, &id, view, cursor, limit, descending, include,
                            )
                            .ok()
                            .map(|page| serde_json::to_value(page).unwrap());
                            assert_eq!(
                                actual,
                                reference_page(&record, view, cursor, limit, descending, include),
                                "P1-P7 owner={action_owner:?} view={view} kind={kind:?} descending={descending} limit={limit:?} cursor={cursor:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn indexed_deep_pages_scan_only_the_cursor_and_bounded_matching_suffix() {
    // S1 retained sizes 100/1000; S2 direction; S3 all/even filter. The cursor
    // lies halfway through history, is visible and includes whitespace. A page
    // of two requires its cursor and one matching lookahead: four visits for
    // all rows, seven for alternating rows, independent of retained prefix size.
    // This counts actual stable-filter evaluations, not elapsed machine time.
    let (state, id) = page_state().await;
    for size in [100, 1000] {
        state
            .publish_projection_update(&id, |record| {
                record.events = (0..size).map(|i| message(&format!("evt-{i}"))).collect();
                Ok(())
            })
            .unwrap();
        for descending in [false, true] {
            for filtered in [false, true] {
                let visits = Cell::new(0);
                let page = state
                    .list_events_filtered(
                        &id,
                        Some(&format!(" evt-{} ", size / 2)),
                        Some(2),
                        descending,
                        |event| {
                            visits.set(visits.get() + 1);
                            !filtered || event.id[4..].parse::<usize>().unwrap().is_multiple_of(2)
                        },
                    )
                    .unwrap();
                assert_eq!(page.data.len(), 2);
                assert!(page.next_page.is_some());
                assert_eq!(
                    visits.get(),
                    if filtered { 7 } else { 4 },
                    "S1-S3 {size}/{descending}/{filtered}"
                );
            }
        }
    }
}

#[tokio::test]
async fn page_index_tracks_published_references_owners_replacement_and_order() {
    // C1 action owner changes without changing row count; C2 referenced id
    // changes; C3 ids/owners are replaced at the same positions; C4 order changes;
    // C5 processed time changes. Each successful publication must project current
    // rows and visibility exactly. C6 no-op/error/stale CAS must retain the current
    // index and frame. Only the canonical publisher mutates this fixture.
    let (state, id) = page_state().await;
    state
        .publish_projection_update(&id, |record| {
            record.events = vec![tool("tool"), action("tool"), message("tail")];
            record
                .event_thread_owners
                .insert("tool".into(), "child".into());
            Ok(())
        })
        .unwrap();
    let original = state.sessions.lock().unwrap().get(&id).unwrap().clone();
    for change in 0..6 {
        state
            .publish_projection_update(&id, |record| {
                match change {
                    0 => {
                        record
                            .event_thread_owners
                            .insert("ask".into(), "child".into());
                    }
                    1 => {
                        record.event_thread_owners.insert("ask".into(), id.clone());
                    }
                    2 => {
                        record.events[1] = action("missing");
                    }
                    3 => {
                        record.events[0].id = "replacement".into();
                        record.events[1] = action("replacement");
                        record.event_thread_owners.remove("tool");
                        record
                            .event_thread_owners
                            .insert("replacement".into(), "child".into());
                    }
                    4 => {
                        record.events.rotate_right(1);
                    }
                    5 => {
                        record.events[0].processed_at = Some("2026-09-13T00:00:00Z".into());
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })
            .unwrap();
        let current = state.sessions.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(current.events.len(), 3, "C1-C5 fixed cardinality");
        for cursor in [
            None,
            Some("tool"),
            Some("ask"),
            Some("replacement"),
            Some("tail"),
        ] {
            for descending in [false, true] {
                let actual = ManagedState::project_event_page(
                    &current,
                    &id,
                    &id,
                    cursor,
                    Some(1),
                    descending,
                    |_| true,
                )
                .ok()
                .map(|page| serde_json::to_value(page).unwrap());
                assert_eq!(
                    actual,
                    reference_page(&current, &id, cursor, Some(1), descending, |_| true),
                    "C1-C5 change={change} cursor={cursor:?}"
                );
            }
        }
    }
    let current = state.sessions.lock().unwrap().get(&id).unwrap().clone();
    state.publish_projection_update(&id, |_| Ok(())).unwrap();
    assert!(
        state
            .publish_projection_update(&id, |record| {
                record.events.clear();
                Err(StateError::Run(RunError::unavailable(
                    "private candidate failed",
                )))
            })
            .is_err()
    );
    assert!(
        !state
            .publish_projection_candidate(&id, original.cache_revision, original, &HashSet::new())
            .unwrap()
    );
    let after = state.sessions.lock().unwrap().get(&id).unwrap().clone();
    refresh_failure::assert_record_frame_eq(&after, &current);
}
