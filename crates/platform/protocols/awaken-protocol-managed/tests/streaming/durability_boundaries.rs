//! A cold Managed projection recovers committed facts without retaining volatile text.
use super::*;

#[tokio::test]
async fn cold_replay_can_lose_uncommitted_previews_without_losing_committed_messages() {
    // A1 the first HTTP client really receives volatile deltas; A2 a Run then
    // commits; A3 another adapter has never observed that preview. Warm/cold
    // canonical Events must agree, while only the first body contains A1.
    let (runtime, warm, cold) = independent_replicas().await;
    let app = router(warm.clone());
    let id = http_create(&app).await;
    let uri = format!("/v1/sessions/{id}/events/stream?event_deltas[]=agent.message");
    let stream_app = app.clone();
    let stream_uri = uri.clone();
    let consumed = Arc::new(tokio::sync::Notify::new());
    let body_consumed = consumed.clone();
    let live =
        tokio::spawn(
            async move { http_sse(&stream_app, &stream_uri, &[], Some(body_consumed)).await },
        );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        runtime.wait_for_live_subscription(),
    )
    .await
    .unwrap();
    runtime.publish_root_live_text(&id, "volatile-prefix-");
    runtime.publish_root_live_text(&id, "volatile-tail");
    tokio::time::timeout(std::time::Duration::from_secs(10), consumed.notified())
        .await
        .expect("the first client actually receives both preview deltas");

    state_send_user(&warm, &runtime, &id, "committed after preview loss").await;
    let live = tokio::time::timeout(std::time::Duration::from_secs(10), live)
        .await
        .unwrap()
        .unwrap()
        .expect(StatusCode::OK);
    assert!(live.contains("volatile-prefix-") && live.contains("volatile-tail"));
    let committed = http_json(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let cold_app = router(cold);
    let restored = http_json(
        &cold_app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        restored["data"], committed["data"],
        "only committed truth survives adapter reconstruction"
    );
    assert!(
        restored["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "agent.message")
    );
    assert!(!restored.to_string().contains("volatile-prefix-"));
    let replay = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        http_sse(&cold_app, &uri, &[], None),
    )
    .await
    .unwrap()
    .expect(StatusCode::OK);
    assert!(!replay.contains("event: event_start") && !replay.contains("event: event_delta"));
    let expected = restored["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        sse_committed_ids(&replay),
        expected,
        "cold SSE delivers every committed identity exactly once"
    );
}
