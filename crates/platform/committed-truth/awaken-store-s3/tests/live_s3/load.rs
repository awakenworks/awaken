// @formal evidence store_history.s3_publication_refines_committed_history
use super::{live, support};
use awaken_agent_contract::{
    agent::{
        message::{Id as MessageId, Message, Role},
        run::Id as RunId,
        thread::Id as ThreadId,
    },
    thread::{
        commit::operation::{CommitOperation, commit_payload_hash},
        read::{
            recovery::RunRecoverySource,
            transcript::{TranscriptPageRequest, TranscriptSnapshotRef, TranscriptView},
        },
    },
};
use awaken_store_s3::{Error, Limits, SessionStorage, StreamBatch, StreamCursor, StreamReadCursor};
use bytes::Bytes;
use std::num::NonZeroUsize;

fn message(ordinal: u64, length: usize) -> String {
    let text: String = live::bytes(17, ordinal, length)
        .iter()
        .map(|byte| (b' ' + byte % 95) as char)
        .collect();
    format!("{ordinal}:{text}")
}
// Capacity is measured in retained messages/bytes; each real operation carries
// a small batch so this drill stays within the normal per-test deadline.
const LONG_BATCH: usize = 8;
fn capacity_operation(ordinal: u64, length: usize) -> CommitOperation {
    let mut operation = support::operation("run", "session", ordinal, ordinal, "");
    operation.commit.messages = (0..LONG_BATCH as u64)
        .map(|offset| {
            let position = ordinal * LONG_BATCH as u64 + offset;
            Message::text(
                MessageId(format!("capacity-{position}")),
                Role::Assistant,
                message(position, length),
            )
        })
        .collect();
    operation.payload_hash = commit_payload_hash(&operation.commit).unwrap();
    operation
}
async fn check_history(
    session: &SessionStorage,
    snapshot: TranscriptSnapshotRef,
    count: usize,
    length: usize,
) {
    let mut start = 0;
    while start < count {
        let page = session
            .read_page(TranscriptPageRequest {
                thread_id: ThreadId("session".into()),
                view: TranscriptView::RawCommitted,
                snapshot: Some(snapshot.clone()),
                start: start as u64,
                limit: NonZeroUsize::new(73).unwrap(),
            })
            .await
            .unwrap();
        assert!(!page.messages.is_empty(), "no gaps before the frozen end");
        for item in &page.messages {
            assert_eq!(item.text_content(), message(start as u64, length));
            start += 1;
        }
        assert!(start <= count);
    }
    assert!(
        session
            .read_page(TranscriptPageRequest {
                thread_id: ThreadId("session".into()),
                view: TranscriptView::RawCommitted,
                snapshot: Some(snapshot),
                start: count as u64,
                limit: NonZeroUsize::new(1).unwrap(),
            })
            .await
            .unwrap()
            .messages
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn long_session_survives_pack_gc_and_cold_frozen_pagination() {
    tokio::time::timeout(std::time::Duration::from_secs(240), long_session_case())
        .await
        .expect("long Session must finish inside the standard required-test deadline");
}

async fn long_session_case() {
    let count = live::size("AWAKEN_TEST_S3_LONG_COMMITS", 256, 128, 512);
    let messages = count * LONG_BATCH;
    let length = live::size("AWAKEN_TEST_S3_LONG_MESSAGE_BYTES", 2_048, 1_024, 16_384);
    let limits = Limits {
        recent_commits: 16,
        read_ahead_bytes: 64 * 1024,
        ..Default::default()
    };
    let fixture = live::Fixture::new(limits.clone()).await;
    let claim = support::activate(&fixture.session, "run", "session")
        .await
        .run_claim();
    let started = std::time::Instant::now();
    let mut midpoint = None;
    let mut peak = 0;
    for ordinal in 0..count as u64 {
        fixture
            .session
            .commit(&claim, capacity_operation(ordinal, length))
            .await
            .unwrap();
        if ordinal as usize + 1 == count / 2 {
            midpoint = Some(
                fixture
                    .session
                    .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
                    .await
                    .unwrap(),
            );
        }
        let bytes = fixture.head_bytes().await;
        peak = peak.max(bytes.len());
        assert!(bytes.len() <= limits.max_head_bytes);
        if ordinal % 64 == 0 {
            let head: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(head["recent"].as_array().unwrap().len() <= limits.recent_commits);
        }
        if (ordinal + 1) % 64 == 0 {
            let requests = fixture.engine.request_counts();
            println!(
                "live capacity phase=append commits={} elapsed_ms={} get={} put={}",
                ordinal + 1,
                started.elapsed().as_millis(),
                requests.get,
                requests.put
            );
        }
    }
    let full = fixture
        .session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    fixture.session.pack().await.unwrap();
    fixture.session.collect_garbage().await.unwrap();
    let mut cold_limits = limits;
    cold_limits.cache_bytes = 1024 * 1024;
    let (_, _, cold) = fixture.cold(1_000, cold_limits);
    println!(
        "live capacity phase=cold-read elapsed_ms={}",
        started.elapsed().as_millis()
    );
    check_history(&cold, midpoint.unwrap(), messages / 2, length).await;
    check_history(&cold, full, messages, length).await;
    let receipt = cold
        .commit(&claim, capacity_operation(0, length))
        .await
        .unwrap();
    assert!(receipt.duplicate);
    assert_eq!(receipt.commit_sequence, 1);
    assert_eq!(cold.head().await.unwrap().commit_sequence, count as u64);
    assert!(
        peak < messages * length / 4,
        "the hot head must not contain full history bodies"
    );
    println!(
        "live long-session commits={count} messages={messages} message_bytes={length} peak_head_bytes={peak} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn capacity_rejection_is_atomic_and_same_operation_recovers_after_packing() {
    let limits = Limits {
        max_head_bytes: 8 * 1024,
        recent_commits: 512,
        ..Default::default()
    };
    let fixture = live::Fixture::new(limits).await;
    let claim = support::activate(&fixture.session, "run", "session")
        .await
        .run_claim();
    let mut rejected = None;
    for ordinal in 0..128 {
        let before = fixture
            .session
            .recovery_snapshot(&ThreadId("session".into()), &RunId("run".into()))
            .await
            .unwrap();
        let head = fixture.head_bytes().await;
        let operation =
            support::operation("run", "session", ordinal, ordinal, &message(ordinal, 1_024));
        match fixture.session.commit(&claim, operation.clone()).await {
            Ok(_) => {}
            Err(Error::Capacity(_)) => {
                assert!(
                    ordinal > 0,
                    "capacity fixture must admit at least one operation"
                );
                assert_eq!(
                    fixture.head_bytes().await,
                    head,
                    "no publication on capacity rejection"
                );
                let after = fixture
                    .session
                    .recovery_snapshot(&ThreadId("session".into()), &RunId("run".into()))
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(before).unwrap(),
                    serde_json::to_value(after).unwrap()
                );
                rejected = Some(operation);
                break;
            }
            Err(error) => panic!("unexpected rejection: {error}"),
        }
    }
    let operation = rejected.expect("the configured small head must reach capacity");
    fixture.session.pack().await.unwrap();
    let receipt = fixture
        .session
        .commit(&claim, operation.clone())
        .await
        .unwrap();
    assert!(!receipt.duplicate);
    assert!(
        fixture
            .session
            .commit(&claim, operation)
            .await
            .unwrap()
            .duplicate
    );
    fixture.session.collect_garbage().await.unwrap();
    assert_eq!(
        fixture.session.head().await.unwrap().commit_sequence,
        receipt.commit_sequence
    );
    fixture.cleanup().await;
}

fn frame(producer: u64, ordinal: u64, length: usize) -> Bytes {
    let bytes = live::bytes(producer, ordinal, length);
    if producer.is_multiple_of(2) {
        Bytes::from(bytes.iter().map(|b| b'a' + b % 26).collect::<Vec<_>>())
    } else {
        bytes
    }
}
fn mime(producer: u64) -> &'static str {
    if producer.is_multiple_of(2) {
        "text/plain"
    } else {
        "audio/pcm"
    }
}
fn validate(batch: &StreamBatch, next: &mut [u64], length: usize, sequence: u64) {
    assert_eq!(
        batch.sequence, sequence,
        "one contiguous global publication order"
    );
    let producer = batch.cursor.response;
    assert!((producer as usize) < next.len());
    let ordinal = next[producer as usize];
    assert_eq!(
        batch.cursor.next_frame,
        ordinal + 1,
        "per-response continuation boundary"
    );
    assert_eq!(batch.content_type, mime(producer));
    assert!(
        batch.bytes == frame(producer, ordinal, length),
        "changed multimodal frame bytes"
    );
    next[producer as usize] += 1;
}
async fn append(
    session: &SessionStorage,
    claim: &awaken_worker_contract::RunClaim,
    cursor: StreamCursor,
    bytes: Bytes,
    content_type: &str,
) {
    // awaken-allow(wait): retry only the same durable frame publication after an ambiguous storage result.
    for attempt in 0..128 {
        match session
            .streams()
            .append_media_stream(claim, cursor.clone(), bytes.clone(), content_type)
            .await
        {
            Ok(_) => return,
            // Match the production commit port: Storage is unavailable, while
            // fencing, malformed input, corruption and capacity remain fatal.
            Err(Error::Storage(_)) | Err(Error::OutcomeUnknown(_)) => {}
            Err(error) => panic!("non-retryable stream failure: {error}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(1 + attempt % 7)).await;
    }
    panic!("stream publication did not converge under bounded contention or unavailable I/O");
}

fn observed_frame(batch: &StreamBatch) -> serde_json::Value {
    use sha2::{Digest, Sha256};
    serde_json::json!({
        "sequence": batch.sequence, "step": batch.cursor.step,
        "response": batch.cursor.response, "attempt": batch.cursor.attempt,
        "after": batch.cursor.next_frame, "mime": batch.content_type,
        "sha256": format!("{:x}", Sha256::digest(&batch.bytes)),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn concurrent_multimodal_producers_live_reader_and_cold_replay_agree() {
    let producers = live::size("AWAKEN_TEST_S3_STREAM_PRODUCERS", 4, 2, 16);
    let frames = live::size("AWAKEN_TEST_S3_STREAM_FRAMES", 128, 32, 2_048);
    let length = live::size(
        "AWAKEN_TEST_S3_STREAM_FRAME_BYTES",
        16 * 1024,
        512,
        256 * 1024,
    );
    let limits = Limits {
        max_cas_attempts: 64,
        cache_bytes: 1024 * 1024,
        ..Default::default()
    };
    let fixture = live::Fixture::new(limits.clone()).await;
    let claim = support::activate(&fixture.session, "run", "session")
        .await
        .run_claim();
    fixture
        .session
        .streams()
        .put_checkpoint(
            &claim,
            awaken_agent_contract::stream::checkpoint::StreamCheckpoint {
                run_id: "run".into(),
                thread_id: "session".into(),
                model: "multimodal".into(),
                partial_text: "stream in progress".into(),
                partial_tools: vec![],
                retry_count: 0,
            },
        )
        .await
        .unwrap();
    let checkpoint_before = matches!(
        fixture.session.streams().checkpoint("run").await,
        awaken_agent_contract::observation::Observed::Present(_)
    );
    assert!(checkpoint_before);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(producers));
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = (producers * frames) as u64;
    let mut work = tokio::task::JoinSet::new();
    let (_, _, reader) = fixture.cold(1_000, limits.clone());
    work.spawn(async move {
        let streams = reader.streams();
        let mut subscription = streams
            .subscribe_run(StreamReadCursor {
                key: reader.key().clone(),
                run: "run".into(),
                after: 0,
            })
            .await
            .unwrap();
        let mut next = vec![0_u64; producers];
        let mut sequence = 1;
        let mut live_frames = Vec::new();
        while sequence <= total {
            let page = streams
                .next_stream_batches(&mut subscription, 17)
                .await
                .unwrap();
            for batch in &page.batches {
                validate(batch, &mut next, length, sequence);
                live_frames.push(observed_frame(batch));
                sequence += 1;
            }
        }
        assert_eq!(subscription.cursor().after, total);
        Some((next, live_frames, subscription.cursor().after))
    });
    for producer in 0..producers as u64 {
        let (_, _, writer) = fixture.cold(1_000, limits.clone());
        let claim = claim.clone();
        let barrier = barrier.clone();
        let in_flight = in_flight.clone();
        let peak = peak.clone();
        work.spawn(async move {
            barrier.wait().await;
            for ordinal in 0..frames as u64 {
                let cursor = StreamCursor {
                    run: "run".into(),
                    step: 0,
                    response: producer,
                    assistant_step: None,
                    attempt: 0,
                    next_frame: ordinal,
                };
                let active = in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                peak.fetch_max(active, std::sync::atomic::Ordering::SeqCst);
                append(
                    &writer,
                    &claim,
                    cursor.clone(),
                    frame(producer, ordinal, length),
                    mime(producer),
                )
                .await;
                in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                if ordinal % 31 == 0 {
                    append(
                        &writer,
                        &claim,
                        cursor,
                        frame(producer, ordinal, length),
                        mime(producer),
                    )
                    .await;
                }
            }
            None
        });
    }
    let (completed, live_frames, watermark) =
        tokio::time::timeout(std::time::Duration::from_secs(240), async {
            let mut reader = None;
            while let Some(result) = work.join_next().await {
                if let Some(counts) = result.unwrap() {
                    reader = Some(counts);
                }
            }
            reader.unwrap()
        })
        .await
        .expect("bounded concurrent S3 streaming load");
    assert_eq!(completed, vec![frames as u64; producers]);
    append(
        &fixture.session,
        &claim,
        StreamCursor {
            run: "run".into(),
            step: 0,
            response: 0,
            assistant_step: None,
            attempt: 0,
            next_frame: frames as u64,
        },
        frame(0, frames as u64, length),
        mime(0),
    )
    .await;
    fixture
        .session
        .commit(
            &claim,
            support::operation("run", "session", 0, 0, "finalize streams"),
        )
        .await
        .unwrap();
    fixture.session.collect_garbage().await.unwrap();
    let (_, _, cold) = fixture.cold(1_000, limits);
    let mut cursor = StreamReadCursor {
        key: cold.key().clone(),
        run: "run".into(),
        after: 0,
    };
    let mut next = vec![0_u64; producers];
    let mut sequence = 1;
    let mut frozen_frames = Vec::new();
    while cursor.after < total {
        let page = cold
            .streams()
            .read_run_page(&cursor, 23, Some(total))
            .await
            .unwrap();
        assert!(!page.batches.is_empty());
        for batch in &page.batches {
            validate(batch, &mut next, length, sequence);
            frozen_frames.push(observed_frame(batch));
            sequence += 1;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(sequence, total + 1);
    assert_eq!(next, completed);
    let tail = cold
        .streams()
        .read_run_page(&cursor, 23, None)
        .await
        .unwrap();
    assert_eq!(tail.batches.len(), 1);
    assert_eq!(tail.batches[0].sequence, total + 1);
    assert!(tail.batches[0].bytes == frame(0, frames as u64, length));
    let checkpoint_after = matches!(
        cold.streams().checkpoint("run").await,
        awaken_agent_contract::observation::Observed::Present(_)
    );
    let peak_writers = peak.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        peak_writers >= 2,
        "the load must overlap independent append futures"
    );
    assert_eq!(in_flight.load(std::sync::atomic::Ordering::SeqCst), 0);
    awaken_reliability_testkit::write_observation_trace(
        "s3-concurrent-stream",
        "ordered-stream",
        serde_json::json!([{"rule": "concurrent-stream", "trace": {
            "stream_count": producers, "frames_per_stream": frames, "peak_writers": peak_writers,
            "watermark": watermark, "live": live_frames, "frozen": frozen_frames,
            "tail": tail.batches.iter().map(observed_frame).collect::<Vec<_>>(),
            "checkpoint_before": checkpoint_before, "checkpoint_after": checkpoint_after,
        }}]),
    );
    println!(
        "live stream producers={producers} frames_per_producer={frames} frame_bytes={length} total_bytes={}",
        total * length as u64
    );
    fixture.cleanup().await;
}
