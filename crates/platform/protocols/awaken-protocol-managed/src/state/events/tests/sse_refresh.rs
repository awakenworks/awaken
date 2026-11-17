use super::*;
use std::time::Duration;

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
