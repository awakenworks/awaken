use super::*;
use std::time::Duration;

#[tokio::test]
async fn committed_subscriptions_release_only_the_last_observer_and_failed_opens() {
    // Causes: two observers share a Session; the last leaves; another opens;
    // an unknown Session fails admission. Each registry entry must have a live owner.
    let state = Arc::new(ManagedState::new(LifecycleRuntime::default()).await);
    let session = state
        .create_session(
            serde_json::from_value(
                serde_json::json!({"agent":"coder", "environment_id":"env_local"}),
            )
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let (_, first) = state.stream_subscribe(&session.id).unwrap();
    let (_, mut second) = state.stream_subscribe(&session.id).unwrap();
    drop(first);
    assert_eq!(state.live.lock().len(), 1);
    let event = Event {
        id: "notice".into(),
        processed_at: None,
        kind: OutboundKind::AgentThinking {},
    };
    state.broadcast_committed_delta(&session.id, std::slice::from_ref(&event));
    assert_eq!(second.try_recv().unwrap(), event);
    drop(second);
    assert!(
        state.live.lock().is_empty(),
        "last observer must release the channel"
    );
    for _ in 0..128 {
        let (_, receiver) = state.stream_subscribe(&session.id).unwrap();
        drop(receiver);
    }
    assert!(state.live.lock().is_empty());
    assert!(state.stream_subscribe("unknown-session").is_err());
    assert!(
        state.live.lock().is_empty(),
        "failed admission must not retain a channel"
    );
    let app = crate::router(state.clone()).layer(axum::Extension(
        awaken_runtime_contract::runs::scope::WorkspaceScope(
            awaken_tenancy::DEFAULT_WORKSPACE_ID.to_owned(),
        ),
    ));
    let mut abandoned = Vec::new();
    for _ in 0..24 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/sessions/{}/events/stream", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        abandoned.push(response.into_body());
    }
    assert_eq!(state.live.lock().len(), 1);
    drop(abandoned);
    assert!(
        state.live.lock().is_empty(),
        "dropping unpolled HTTP bodies releases every receiver"
    );
}

#[tokio::test]
async fn long_history_subscribers_share_frozen_payloads_and_tail_pages_stay_bounded() {
    // Synthetic projection capacity, not a store durability test: 10k retained
    // messages and 128 slow observers share one exact version; a concurrent
    // publication leaves it frozen and returns only bounded change pages.
    let state = ManagedState::new(LifecycleRuntime::default()).await;
    let session = state
        .create_session(
            serde_json::from_value(
                serde_json::json!({"agent":"coder", "environment_id":"env_local"}),
            )
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let make_event = |index| Event {
        id: format!("long-history-{index}"),
        processed_at: None,
        kind: OutboundKind::AgentMessage {
            content: vec![ContentBlock::text("x".repeat(256))],
        },
    };
    {
        let mut sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|error| panic!("{error}"));
        let record = sessions.get_mut(&session.id).unwrap();
        record.events = (0..10_000).map(make_event).collect();
        record.rebuild_event_page_index();
    }
    let old_index = {
        let sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|error| panic!("{error}"));
        Arc::downgrade(&sessions.get(&session.id).unwrap().event_page_index)
    };
    let observers = (0..128)
        .map(|_| {
            let (snapshot, receiver, revision) =
                state.stream_projection_subscribe(&session.id).unwrap();
            (snapshot.into_events(), receiver, revision)
        })
        .collect::<Vec<_>>();
    assert!(
        observers
            .iter()
            .all(|(snapshot, _, _)| Arc::ptr_eq(snapshot, &observers[0].0))
    );
    {
        let mut sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|error| panic!("{error}"));
        let record = sessions.get_mut(&session.id).unwrap();
        let previous = record.clone();
        let delta = (10_000..10_300).map(make_event).collect::<Vec<_>>();
        record.events.extend(delta.iter().cloned());
        record.advance_cache_revision().unwrap();
        record.rebuild_event_page_index();
        record.index_delivery_delta(&previous, &delta);
    }
    assert!(
        observers
            .iter()
            .all(|(snapshot, _, _)| snapshot.len() == 10_000)
    );
    assert!(
        old_index.upgrade().is_none(),
        "initial replay must not pin obsolete lookup indexes"
    );
    let mut cursor = (observers[0].2, usize::MAX);
    let mut delivered = Vec::new();
    loop {
        let page = state.stream_delivery_page(&session.id, cursor).unwrap();
        assert!(page.len() <= ManagedState::STREAM_DELIVERY_PAGE_SIZE);
        if page.is_empty() {
            break;
        }
        for (position, event) in page {
            assert!(position > cursor);
            cursor = position;
            delivered.push(event.id);
        }
    }
    assert_eq!(
        delivered,
        (10_000..10_300)
            .map(|i| format!("long-history-{i}"))
            .collect::<Vec<_>>()
    );
    // Staggered slow readers retain distinct immutable versions. Exercise that
    // cost explicitly; same-version sharing alone cannot prove a memory bound.
    let mut staggered = Vec::new();
    for version in 0..24 {
        let (snapshot, receiver, revision) =
            state.stream_projection_subscribe(&session.id).unwrap();
        staggered.push((snapshot.into_events(), receiver, revision));
        let mut sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|error| panic!("{error}"));
        let record = sessions.get_mut(&session.id).unwrap();
        let previous = record.clone();
        let event = make_event(10_300 + version);
        record.events.push(event.clone());
        record.advance_cache_revision().unwrap();
        record.rebuild_event_page_index();
        record.index_delivery_delta(&previous, &[event]);
    }
    for (version, (snapshot, _, _)) in staggered.iter().enumerate() {
        assert_eq!(
            snapshot.len(),
            10_300 + version,
            "slow replay version remains frozen"
        );
    }
    drop(staggered);
    drop(observers);
    assert!(state.live.lock().is_empty());
}

#[tokio::test]
async fn last_observer_departure_cannot_remove_a_concurrently_opened_subscription() {
    let state = Arc::new(ManagedState::new(LifecycleRuntime::default()).await);
    let session = state
        .create_session(
            serde_json::from_value(
                serde_json::json!({"agent":"coder", "environment_id":"env_local"}),
            )
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    for _ in 0..32 {
        let (_, old) = state.stream_subscribe(&session.id).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        let left = gate.clone();
        let mut new = std::thread::scope(|scope| {
            scope.spawn(move || {
                left.wait();
                drop(old);
            });
            gate.wait();
            state.stream_subscribe(&session.id).unwrap().1
        });
        let event = Event {
            id: "still-attached".into(),
            kind: OutboundKind::AgentThinking {},
            processed_at: None,
        };
        state.broadcast_committed_delta(&session.id, std::slice::from_ref(&event));
        assert_eq!(new.try_recv().unwrap(), event);
        drop(new);
        assert!(state.live.lock().is_empty());
    }
}

#[tokio::test]
async fn concurrent_session_and_thread_streams_share_the_in_flight_refresh() {
    let runtime = LifecycleRuntime::default();
    let state = Arc::new(ManagedState::new(runtime.clone()).await);
    let create = || async {
        state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .unwrap()
    };
    state.disable_warm_cold_oracle();
    let session = create().await;
    let other = create().await;
    state.refresh_committed_events(&session.id).await.unwrap();
    let (first, _, revision) = state.stream_projection_subscribe(&session.id).unwrap();
    for _ in 0..24 {
        let (next, _, next_revision) = state.stream_projection_subscribe(&session.id).unwrap();
        assert_eq!(next_revision, revision);
        assert!(
            Arc::ptr_eq(&first.events, &next.events),
            "each subscriber must share the same published history payload"
        );
    }
    let primary = state.list_threads(&session.id).unwrap()[0].id.clone();
    let app = crate::router(state).layer(axum::Extension(
        awaken_runtime_contract::runs::scope::WorkspaceScope(
            awaken_tenancy::DEFAULT_WORKSPACE_ID.to_string(),
        ),
    ));
    let get = |uri: String| {
        app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
    };
    let (entered, release) = runtime.reads.pause_next("recovery");
    let leader = tokio::spawn(get(format!("/v1/sessions/{}/events/stream", session.id)));
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the Session stream reached the canonical read");
    let mut follower = Box::pin(get(format!(
        "/v1/sessions/{}/threads/{primary}/stream",
        session.id
    )));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut follower)
            .await
            .is_err(),
        "the Thread stream must share the outstanding Session refresh",
    );
    let independent = tokio::time::timeout(
        Duration::from_secs(2),
        get(format!("/v1/sessions/{}/events/stream", other.id)),
    )
    .await
    .expect("another Session can refresh")
    .unwrap();
    assert_eq!(independent.status(), StatusCode::OK);

    let reads = runtime.reads.count("recovery");
    release.notify_one();
    assert_eq!(leader.await.unwrap().unwrap().status(), StatusCode::OK);
    assert_eq!(follower.await.unwrap().status(), StatusCode::OK);
    assert_eq!(runtime.reads.count("recovery") - reads, 1);
}
