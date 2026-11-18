// @formal evidence store_history.s3_sealed_collection_preserves_reachable_objects
mod support;
use awaken_store_s3::{Error, StreamCursor, StreamReadCursor};
use bytes::Bytes;

fn cursor(run: &str, response: u64, next_frame: u64) -> StreamCursor {
    StreamCursor {
        run: run.into(),
        step: 0,
        response,
        assistant_step: None,
        attempt: 0,
        next_frame,
    }
}
#[tokio::test]
async fn interleaved_multimodal_pages_freeze_watermarks_and_preserve_local_cursors() {
    let (store, clock, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let streams = session.streams();
    for (response, ordinal, mime, data) in [
        (1, 0, "text/plain", b"hello".as_slice()),
        (0, 0, "application/json", b"{\"control\":true}".as_slice()),
        (2, 0, "audio/pcm", b"\x00\xff\x01\x80".as_slice()),
        (1, 1, "text/plain", b" world".as_slice()),
    ] {
        streams
            .append_media_stream(
                &claim,
                cursor("run", response, ordinal),
                Bytes::copy_from_slice(data),
                mime,
            )
            .await
            .unwrap();
    }
    let cold = support::engine(store, clock)
        .session(session.key().clone())
        .streams();
    let start = StreamReadCursor {
        key: session.key().clone(),
        run: "run".into(),
        after: 0,
    };
    let first = cold.read_run_page(&start, 2, None).await.unwrap();
    assert_eq!(
        first.batches.iter().map(|b| b.sequence).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(first.next_cursor.after, 2);
    assert_eq!(first.watermark, 4);
    streams
        .append_media_stream(
            &claim,
            cursor("run", 1, 2),
            Bytes::from_static(b"!"),
            "text/plain",
        )
        .await
        .unwrap();
    let second = cold
        .read_run_page(&first.next_cursor, 2, Some(first.watermark))
        .await
        .unwrap();
    assert_eq!(
        second
            .batches
            .iter()
            .map(|b| b.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(second.batches[0].content_type, "audio/pcm");
    assert_eq!(second.batches[0].bytes.as_ref(), b"\x00\xff\x01\x80");
    assert_eq!(second.next_cursor.after, 4);
    assert!(
        cold.read_run_page(&second.next_cursor, 2, Some(4))
            .await
            .unwrap()
            .batches
            .is_empty()
    );
    assert_eq!(
        cold.read_run_page(&second.next_cursor, 2, None)
            .await
            .unwrap()
            .batches[0]
            .sequence,
        5
    );
    assert_eq!(
        cold.read_stream(&cursor("run", 1, 0), 10)
            .await
            .unwrap()
            .iter()
            .map(|b| b.sequence)
            .collect::<Vec<_>>(),
        vec![1, 4, 5]
    );
    let mut foreign = start.clone();
    foreign.key = awaken_store_s3::SessionKey::new("other", "session").unwrap();
    assert!(matches!(
        cold.read_run_page(&foreign, 1, None).await,
        Err(Error::Invalid(_))
    ));
    let zero = cold.read_run_page(&start, 0, None).await.unwrap();
    assert_eq!(zero.next_cursor, start);
}
#[tokio::test]
async fn stream_duplicate_checks_mime_and_does_not_consume_ingest_sequence() {
    let (store, _, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let streams = session.streams();
    let c = cursor("run", 0, 0);
    store.fault.store(1, std::sync::atomic::Ordering::SeqCst);
    let first = streams
        .append_media_stream(&claim, c.clone(), Bytes::from_static(b"pcm"), "audio/pcm")
        .await;
    assert!(first.is_ok() || matches!(first, Err(Error::OutcomeUnknown(_))));
    streams
        .append_media_stream(&claim, c.clone(), Bytes::from_static(b"pcm"), "audio/pcm")
        .await
        .unwrap();
    assert!(matches!(
        streams
            .append_media_stream(&claim, c, Bytes::from_static(b"pcm"), "text/plain")
            .await,
        Err(Error::Conflict(_))
    ));
    let start = StreamReadCursor {
        key: session.key().clone(),
        run: "run".into(),
        after: 0,
    };
    let page = streams.read_run_page(&start, 10, None).await.unwrap();
    assert_eq!(page.watermark, 1);
    assert_eq!(page.batches.len(), 1);
}
#[tokio::test]
async fn live_stream_subscription_repairs_cross_instance_wake_and_resumes() {
    let (store, clock, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let streams = session.streams();
    let start = StreamReadCursor {
        key: session.key().clone(),
        run: "run".into(),
        after: 0,
    };
    let mut subscription = streams.subscribe_run(start).await.unwrap();
    let other = support::engine(store, clock)
        .session(session.key().clone())
        .streams();
    other
        .append_stream(&claim, cursor("run", 0, 0), Bytes::from_static(b"first"))
        .await
        .unwrap();
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        streams.next_stream_batches(&mut subscription, 1),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.batches[0].bytes.as_ref(), b"first");
    let mut resumed = streams.subscribe_run(subscription.cursor()).await.unwrap();
    other
        .append_stream(&claim, cursor("run", 1, 0), Bytes::from_static(b"second"))
        .await
        .unwrap();
    assert_eq!(
        streams
            .next_stream_batches(&mut resumed, 1)
            .await
            .unwrap()
            .batches[0]
            .bytes
            .as_ref(),
        b"second"
    );
}
#[tokio::test]
async fn cold_indexed_stream_range_uses_ingest_bounds_without_losing_interleaving() {
    let (store, clock, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    for i in 0..140 {
        let response = i % 2;
        session
            .streams()
            .append_stream(
                &claim,
                cursor("run", response, i / 2),
                Bytes::from(i.to_string()),
            )
            .await
            .unwrap();
    }
    let cold = support::engine(store, clock)
        .session(session.key().clone())
        .streams();
    let start = StreamReadCursor {
        key: session.key().clone(),
        run: "run".into(),
        after: 125,
    };
    let page = cold.read_run_page(&start, 7, None).await.unwrap();
    assert_eq!(
        page.batches.iter().map(|b| b.sequence).collect::<Vec<_>>(),
        (126..133).collect::<Vec<_>>()
    );
    assert_eq!(page.next_cursor.after, 132);
    let next = cold
        .read_run_page(&page.next_cursor, 10, Some(page.watermark))
        .await
        .unwrap();
    assert_eq!(
        next.batches.iter().map(|b| b.sequence).collect::<Vec<_>>(),
        (133..141).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn completed_responses_leave_the_hot_head_and_replay_after_collection() {
    use object_store::ObjectStoreExt as _;
    let (store, clock, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    for step in 0..100 {
        for ordinal in 0..3 {
            session
                .streams()
                .append_stream(
                    &claim,
                    StreamCursor {
                        run: "run".into(),
                        step,
                        response: step,
                        assistant_step: None,
                        attempt: 0,
                        next_frame: ordinal,
                    },
                    Bytes::from(format!("{step}:{ordinal}")),
                )
                .await
                .unwrap();
        }
        session
            .commit(
                &claim,
                support::operation("run", "session", step, step, "step"),
            )
            .await
            .unwrap();
    }
    let head: serde_json::Value = serde_json::from_slice(
        &store
            .get(&object_store::path::Path::from(
                "test/v1/scopes/tenant/sessions/session/head.json",
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(head["frames"].as_object().unwrap().is_empty());
    assert!(head["stream_archive"].is_object());
    session.collect_garbage().await.unwrap();
    let cold = support::engine(store, clock)
        .session(session.key().clone())
        .streams();
    let old = StreamCursor {
        run: "run".into(),
        step: 40,
        response: 40,
        assistant_step: None,
        attempt: 0,
        next_frame: 1,
    };
    assert_eq!(
        cold.read_stream(&old, 2)
            .await
            .unwrap()
            .iter()
            .map(|b| b.bytes.clone())
            .collect::<Vec<_>>(),
        vec![Bytes::from_static(b"40:1"), Bytes::from_static(b"40:2")]
    );
    let mut cursor = StreamReadCursor {
        key: session.key().clone(),
        run: "run".into(),
        after: 0,
    };
    let mut sequences = Vec::new();
    loop {
        let page = cold.read_run_page(&cursor, 17, Some(300)).await.unwrap();
        sequences.extend(page.batches.iter().map(|frame| frame.sequence));
        cursor = page.next_cursor;
        if cursor.after == 300 {
            break;
        }
    }
    assert_eq!(sequences, (1..=300).collect::<Vec<_>>());
}
