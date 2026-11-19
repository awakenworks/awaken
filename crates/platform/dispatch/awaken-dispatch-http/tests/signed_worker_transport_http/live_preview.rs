use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Observation as StreamObservation;
use awaken_dispatch_contract::ClaimedStreamPublisher;
use awaken_store_inmem::MemoryStreamSink;
use awaken_worker_contract::{RunClaim, WorkerIdentity};
use awaken_worker_transport_security::WorkerUpstream;

pub(super) async fn verify_signed_preview(
    live_publisher: &dyn ClaimedStreamPublisher,
    upstream: &WorkerUpstream,
    identity: &WorkerIdentity,
    claim: &RunClaim,
    live_stream: &MemoryStreamSink,
) {
    let live_event = |thread_id: &str| {
        let mut observation = StreamObservation::assistant_delta(
            claim.run_id.clone(),
            ThreadId(thread_id.into()),
            0,
            0,
            AgentEvent::Delta(Delta::TextDelta {
                delta: "live".into(),
            }),
        );
        observation.preview_sequence = Some(2);
        observation
    };
    live_publisher
        .publish_observation(claim, live_event("signed-thread"))
        .await
        .expect("T29 live publication remains best effort");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while live_stream.events().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("T29 signed batch reached the stream sink");
    assert_eq!(live_stream.events().len(), 1, "T29");
    assert_eq!(
        live_stream.observations()[0].preview_sequence,
        Some(2),
        "T29 retains the Worker sequence"
    );
    let path = awaken_dispatch_contract::STREAM_PREVIEW_BATCH_PATH;
    let invalid = upstream
        .authorize(
            "POST",
            path,
            upstream
                .client()
                .post(format!("{}{path}", upstream.base_url())),
        )
        .unwrap()
        .json(&awaken_dispatch_contract::StreamPreviewBatchRequest {
            claim: claim.clone(),
            identity: identity.clone(),
            observations: vec![live_event("signed-thread"), live_event("forged-thread")],
        })
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST, "T30");
    assert_eq!(live_stream.events().len(), 1, "T30 rejects the full batch");
}
