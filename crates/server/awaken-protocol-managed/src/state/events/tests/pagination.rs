// @formal evidence session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
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

#[tokio::test]
async fn history_time_v2_uses_typed_root_and_event_evidence_without_changing_v1() {
    // Synthetic in-memory candidate witness for the version codec/paginator;
    // the timeline suite separately crosses committed warm/cold rehydration.
    // Causes: R1 root/event lacks typed persisted time but v1 contains the
    // historical 2026 placeholder; R2 a real persisted observation equals
    // that same date; R3 time predicate intersects an unknown visible event;
    // R4 type filter excludes the unknown. Effects: R1 v2 null and v1 exact
    // bytes unchanged; R2 v2 retains the date; R3 503 before pagination;
    // R4 a complete known-only page. Cursor order is the existing paginator.
    let (state, id) = page_state().await;
    let mut record = state.sessions.lock().unwrap().get(&id).unwrap().clone();
    record.session.created_at = LEGACY_UNRECORDED_AT.into();
    record.session.updated_at = LEGACY_UNRECORDED_AT.into();
    record.root_time = RootTimeEvidence::default();
    let v1 = serde_json::to_vec(&record.session_projection()).unwrap();
    let v2 = crate::types::session::history_time_v2(
        &record.session_projection(),
        &[
            ("created_at", record.root_time.created_at_unix_ms),
            ("updated_at", record.root_time.updated_at_unix_ms),
        ],
    );
    assert!(
        v2["created_at"].is_null() && v2["updated_at"].is_null(),
        "R1"
    );
    assert_eq!(
        serde_json::to_vec(&record.session_projection()).unwrap(),
        v1,
        "R1"
    );
    let params = crate::routes::sessions::pagination::parse_session_list(Some(
        "created_at%5Bgte%5D=2026-01-01T00%3A00%3A00Z",
    ))
    .unwrap();
    let row = record.session_projection();
    let rows = vec![(row.clone(), RootTimeEvidence::default())];
    assert_eq!(
        crate::routes::sessions::pagination::ensure_v2_time_filter_answerable(&rows, &params)
            .unwrap_err()
            .0,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "R3 Session list includes a relevant unknown before paging",
    );
    let excluded = crate::routes::sessions::pagination::parse_session_list(Some(
        "created_at%5Bgte%5D=2026-01-01T00%3A00%3A00Z&agent_id=other",
    ))
    .unwrap();
    assert!(
        crate::routes::sessions::pagination::ensure_v2_time_filter_answerable(&rows, &excluded,)
            .is_ok(),
        "R4 unrelated unknown cannot poison filtered list"
    );

    let real_placeholder = 1_767_225_600_000_u64;
    assert!(
        crate::routes::sessions::pagination::ensure_v2_time_filter_answerable(
            &[(
                row,
                RootTimeEvidence {
                    created_at_unix_ms: Some(real_placeholder),
                    updated_at_unix_ms: None,
                    archived_at_unix_ms: None,
                }
            )],
            &params,
        )
        .is_ok(),
        "R2 a real persisted timestamp matching the legacy placeholder remains filterable"
    );
    let before_provenance = record.clone();
    record.root_time.created_at_unix_ms = Some(real_placeholder);
    assert!(
        !record.same_projected_content(&before_provenance).unwrap(),
        "R2 a typed-only correction must publish through the existing CAS"
    );
    let v2 = crate::types::session::history_time_v2(
        &record.session_projection(),
        &[("created_at", record.root_time.created_at_unix_ms)],
    );
    assert_eq!(v2["created_at"], LEGACY_UNRECORDED_AT, "R2");

    record.events = vec![message("known"), message("unknown")];
    for event in &mut record.events {
        event.processed_at = Some(LEGACY_UNRECORDED_AT.into());
    }
    record.event_times = HashMap::from([("known".into(), real_placeholder)]);
    record.rebuild_event_page_index();
    let v1_events =
        ManagedState::project_event_page(&record, &id, &id, None, None, false, |_| true).unwrap();
    let v1_bytes = serde_json::to_vec(&v1_events).unwrap();
    let v2_events = ManagedState::project_event_page_with_time(
        &record,
        &id,
        &id,
        None,
        None,
        false,
        true,
        false,
        |_| true,
        |_| true,
    )
    .unwrap();
    assert_eq!(v2_events.data.len(), 2, "R1 no-filter retains unknown");
    assert_eq!(
        v2_events.data[0].processed_at.as_deref(),
        Some(LEGACY_UNRECORDED_AT),
        "R2"
    );
    assert_eq!(v2_events.data[1].processed_at, None, "R1");
    assert_eq!(
        serde_json::to_vec(
            &ManagedState::project_event_page(&record, &id, &id, None, None, false, |_| true,)
                .unwrap()
        )
        .unwrap(),
        v1_bytes,
        "R1 v1 golden bytes"
    );
    let error = ManagedState::project_event_page_with_time(
        &record,
        &id,
        &id,
        None,
        Some(1),
        false,
        true,
        true,
        |event| event.processed_at.is_some(),
        |_| true,
    )
    .unwrap_err();
    assert_eq!(
        crate::routes::sessions::error_response(error).0,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "R3"
    );
    let filtered = ManagedState::project_event_page_with_time(
        &record,
        &id,
        &id,
        None,
        None,
        false,
        true,
        true,
        |event| event.id == "known" && event.processed_at.is_some(),
        |event| event.id == "known",
    )
    .unwrap();
    assert_eq!(
        filtered
            .data
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        vec!["known"],
        "R4"
    );

    // R5 primary Thread inherits typed root evidence; R6 child Thread uses
    // its own canonical lifecycle evidence. A missing child observation stays
    // null even when its v1 Thread inherited the root's placeholder string.
    record.root_time.updated_at_unix_ms = None;
    let child = ManagedState::child_thread(
        &record.session,
        "child",
        crate::types::SessionThreadAgent::from(&record.session.agent),
    );
    record.child_threads.push(child);
    record.child_times.insert(
        "child".into(),
        ThreadTimeEvidence {
            created_at_unix_ms: None,
            updated_at_unix_ms: Some(real_placeholder),
            archived_at_unix_ms: None,
        },
    );
    record.event_thread_owners = HashMap::from([
        ("known".into(), "child".into()),
        ("unknown".into(), "child".into()),
    ]);
    record.rebuild_event_page_index();
    state.sessions.lock().unwrap().insert(id.clone(), record);
    let threads = state.list_threads_with_time(&id).unwrap();
    let primary = threads
        .iter()
        .find(|(thread, _)| thread.parent_thread_id.is_none())
        .unwrap();
    let child = threads
        .iter()
        .find(|(thread, _)| thread.id == "child")
        .unwrap();
    assert_eq!(primary.1.created_at_unix_ms, Some(real_placeholder), "R5");
    assert_eq!(primary.1.updated_at_unix_ms, None, "R5");
    assert_eq!(child.1.created_at_unix_ms, None, "R6");
    assert_eq!(child.1.updated_at_unix_ms, Some(real_placeholder), "R6");
    let child_v2 = crate::types::session::history_time_v2(
        &child.0,
        &[
            ("created_at", child.1.created_at_unix_ms),
            ("updated_at", child.1.updated_at_unix_ms),
        ],
    );
    assert!(child_v2["created_at"].is_null(), "R6");
    assert_eq!(child_v2["updated_at"], LEGACY_UNRECORDED_AT, "R6");
    assert_eq!(
        state.get_thread_with_time(&id, "child").unwrap().1,
        child.1,
        "R6 list/get"
    );
    // R7 the Thread Event v2 time predicate crosses the same visibility and
    // unknown preflight as the Session Event list: relevant unknown => 503;
    // excluding that event by a non-time predicate permits the known row.
    let error = state
        .list_thread_events_filtered_with_time(
            &id,
            "child",
            None,
            None,
            true,
            true,
            |event| event.processed_at.is_some(),
            |_| true,
        )
        .unwrap_err();
    assert_eq!(
        crate::routes::sessions::error_response(error).0,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "R7"
    );
    let known = state
        .list_thread_events_filtered_with_time(
            &id,
            "child",
            None,
            None,
            true,
            true,
            |event| event.id == "known" && event.processed_at.is_some(),
            |event| event.id == "known",
        )
        .unwrap();
    assert_eq!(known.data.len(), 1, "R7");
    assert_eq!(known.data[0].id, "known", "R7");
}

#[tokio::test]
async fn new_archive_projects_one_typed_time_to_session_and_primary_thread() {
    // Causes: P1 live Idle Session archives through the real Managed command
    // and Session Application root CAS; P2 the same command is replayed.
    // Effects: P1 v1 archive string remains present and v2 Session/primary
    // Thread share the committed typed time; P2 changes neither observation.
    // Historical missing provenance is covered by the codec/SQLite tables.
    let (state, id) = page_state().await;
    let first = state.archive_session(&id).await.unwrap();
    let (root, _, time) = state.get_session_with_revision_and_time(&id).unwrap();
    let archived_at = time.archived_at_unix_ms.expect("P1 committed archive time");
    assert_eq!(root.archived_at, first.archived_at, "P1 v1");
    let v2 =
        crate::types::session::history_time_v2(&root, &[("archived_at", time.archived_at_unix_ms)]);
    assert_eq!(
        v2["archived_at"],
        awaken_session_application_contract::epoch_millis_to_rfc3339(archived_at),
        "P1 v2"
    );
    let primary_id = crate::state::public_thread_id(&id, &id);
    let (_, primary_time) = state.get_thread_with_time(&id, &primary_id).unwrap();
    assert_eq!(
        primary_time.archived_at_unix_ms,
        Some(archived_at),
        "P1 primary"
    );
    let replay = state.archive_session(&id).await.unwrap();
    assert_eq!(replay.archived_at, first.archived_at, "P2 v1");
    assert_eq!(
        state
            .get_session_with_revision_and_time(&id)
            .unwrap()
            .2
            .archived_at_unix_ms,
        Some(archived_at),
        "P2 v2"
    );
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
            stop_details: None,
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
                ..
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
    // in the requested direction. P7 is deliberately malformed delivery data:
    // exercise the read index directly, not the publisher that must refuse it.
    // Each Cartesian row is compared with the independent scan composition.
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
                stop_details: None,
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
        let mut record = state.sessions.lock().unwrap().get(&id).unwrap().clone();
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
        record.rebuild_event_page_index();
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
    // Causes: C1 owner changes without changing rows; C2 a referenced id is
    // replaced; C3 id/owner pairs are replaced; C4 old rows reorder; C5 a
    // None processed time completes; C6 no-op/error/stale CAS. Effects: E1
    // C1/C5 publish an index matching current visibility; E2 C2-C4 fail before
    // publication and preserve index/history; E3 C6 keeps the current frame.
    // Rules: R1=C1/C5=>E1; R2=C2-C4=>E2; R3=C6=>E3. This exercises the index
    // through its canonical publisher under append-only committed history.
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
        let before = state.sessions.lock().unwrap().get(&id).unwrap().clone();
        let result = state.publish_projection_update(&id, |record| {
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
                    record.events[1].id = "ask-missing".into();
                }
                3 => {
                    record.events[0].id = "replacement".into();
                    record.events[1] = action("replacement");
                    record.events[1].id = "ask-replacement".into();
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
        });
        if (2..=4).contains(&change) {
            let error = result.expect_err("R2 published id/order cannot be replaced");
            assert!(
                matches!(error, StateError::Run(ref run)
                if run.code == "session_event_history_conflict"),
                "R2 {error}"
            );
        } else {
            result.unwrap();
        }
        let current = state.sessions.lock().unwrap().get(&id).unwrap().clone();
        if (2..=4).contains(&change) {
            refresh_failure::assert_record_frame_eq(&current, &before);
        }
        assert_eq!(current.events.len(), 3, "C1-C5 fixed cardinality");
        for cursor in [
            None,
            Some("tool"),
            Some("ask"),
            Some("ask-missing"),
            Some("ask-replacement"),
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
            .publish_projection_candidate(&id, original.cache_revision, original)
            .unwrap()
    );
    let after = state.sessions.lock().unwrap().get(&id).unwrap().clone();
    refresh_failure::assert_record_frame_eq(&after, &current);
}
