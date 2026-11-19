// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: Batches bounded, best-effort live deltas with the exact claim that produced them.
//! Bounded, best-effort delivery of Worker preview deltas.

use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use awaken_agent_contract::stream::event::Observation;
use awaken_dispatch_contract::{
    MAX_STREAM_PREVIEW_BATCH_ITEMS, MAX_STREAM_PREVIEW_PAYLOAD_BYTES, STREAM_PREVIEW_BATCH_PATH,
    StreamEventRequest, StreamObservationRequest, StreamPreviewBatchRequest,
};
use awaken_time::mono::{self, Instant};
use awaken_worker_contract::{RunClaim, WorkerIdentity};
use awaken_worker_transport_security::WorkerRequestAuthorizer;
use reqwest::StatusCode;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{dispatch_response_error, send_dispatch_request};

const QUEUE_CAPACITY: usize = 128;
const MAX_PENDING_BATCHES: usize = 64;
const MAX_IN_FLIGHT: usize = 4;
const FLUSH_AFTER: Duration = Duration::from_millis(10);
const SEND_DEADLINE: Duration = Duration::from_millis(250);
const MAX_AGE: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct PreviewQueue(OnceLock<mpsc::Sender<Preview>>);

struct Preview {
    claim: RunClaim,
    observation: Observation,
    bytes: usize,
    expires: Instant,
}

pub(super) struct PreviewTransport {
    pub(super) base_url: String,
    pub(super) client: reqwest::Client,
    pub(super) identity: WorkerIdentity,
    pub(super) authorizer: Option<Arc<dyn WorkerRequestAuthorizer>>,
}

impl PreviewQueue {
    pub(super) fn offer(
        &self,
        claim: &RunClaim,
        observation: Observation,
        transport: impl FnOnce() -> PreviewTransport,
    ) {
        let sender = self.0.get_or_init(|| {
            let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
            tokio::spawn(transport().drain(receiver));
            sender
        });
        let Ok(permit) = sender.try_reserve() else {
            tracing::debug!(run_id = %claim.run_id.0, "Worker preview queue is full or closed");
            return;
        };
        let mut size = PreviewSize(1); // Include the JSON array separator.
        if serde_json::to_writer(&mut size, &observation).is_err() {
            tracing::debug!(run_id = %claim.run_id.0, "Worker preview exceeds the batch byte limit");
            return;
        }
        permit.send(Preview {
            claim: claim.clone(),
            observation,
            bytes: size.0,
            expires: mono::now() + MAX_AGE,
        });
    }
}

struct PreviewSize(usize);

impl Write for PreviewSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let size = self.0 + bytes.len();
        if size > MAX_STREAM_PREVIEW_PAYLOAD_BYTES {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.0 = size;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Batch {
    claim: RunClaim,
    observations: Vec<Observation>,
    bytes: usize,
    expires: Instant,
    flush_at: Instant,
}

fn push_preview(batches: &mut Vec<Batch>, preview: Preview) {
    // Use the most recent batch for this claim, so a full batch never causes
    // later fragments to overtake earlier fragments of the same Run.
    if let Some(batch) = batches
        .iter_mut()
        .rev()
        .find(|batch| batch.claim == preview.claim)
        && batch.observations.len() < MAX_STREAM_PREVIEW_BATCH_ITEMS
        && batch.bytes + preview.bytes <= MAX_STREAM_PREVIEW_PAYLOAD_BYTES
    {
        batch.bytes += preview.bytes;
        batch.observations.push(preview.observation);
    } else {
        batches.push(Batch {
            claim: preview.claim,
            observations: vec![preview.observation],
            bytes: preview.bytes,
            expires: preview.expires,
            flush_at: mono::now() + FLUSH_AFTER,
        });
    }
}

impl PreviewTransport {
    async fn drain(self, mut receiver: mpsc::Receiver<Preview>) {
        let transport = Arc::new(self);
        let mut batches: Vec<Batch> = Vec::new();
        let mut active = HashSet::new();
        let mut sends = JoinSet::new();
        let mut closed = false;
        loop {
            let now = mono::now();
            batches.retain(|batch| {
                let fresh = batch.expires > now;
                if !fresh {
                    tracing::debug!(
                        items = batch.observations.len(),
                        "Worker preview batch expired"
                    );
                }
                fresh
            });
            let mut wake_at = None;
            if sends.len() < MAX_IN_FLIGHT {
                // The first eligible batch for each Run always has the earliest
                // flush time. Skip Runs with an HTTP request already in flight.
                let ready = batches
                    .iter()
                    .enumerate()
                    .filter(|(_, batch)| !active.contains(&batch.claim.run_id))
                    .min_by_key(|(_, batch)| batch.flush_at);
                if let Some((index, batch)) = ready {
                    if batch.flush_at <= now {
                        let batch = batches.remove(index);
                        let run = batch.claim.run_id.clone();
                        active.insert(run.clone());
                        let transport = transport.clone();
                        sends.spawn(async move {
                            transport.publish_batch(batch).await;
                            run
                        });
                        continue;
                    }
                    wake_at = Some(batch.flush_at);
                }
            }
            if closed && batches.is_empty() && sends.is_empty() {
                break;
            }
            tokio::select! {
                Some(result) = sends.join_next() => {
                    let Ok(run) = result else {
                        tracing::warn!("Worker preview delivery task failed");
                        return;
                    };
                    active.remove(&run);
                }
                preview = receiver.recv(), if !closed && batches.len() < MAX_PENDING_BATCHES => {
                    if let Some(preview) = preview {
                        push_preview(&mut batches, preview);
                    } else {
                        closed = true;
                    }
                }
                () = async {
                    if let Some(at) = wake_at {
                        tokio::time::sleep_until(at).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
        }
    }

    async fn publish_batch(&self, batch: Batch) {
        let now = mono::now();
        let deadline = batch.expires.min(now + SEND_DEADLINE);
        let items = batch.observations.len();
        tracing::trace!(
            items,
            queue_us = (now + MAX_AGE - batch.expires).as_micros() as u64,
            "Worker preview batch dequeued"
        );
        if deadline <= now {
            tracing::debug!(items, "Worker preview batch expired");
            return;
        }
        match tokio::time::timeout_at(deadline, self.send_batch(batch)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::debug!(%error, items, "Worker preview batch was not delivered")
            }
            Err(_) => tracing::debug!(items, "Worker preview batch timed out"),
        }
    }

    async fn send_batch(
        &self,
        batch: Batch,
    ) -> Result<(), awaken_dispatch_contract::DispatchError> {
        let request = StreamPreviewBatchRequest {
            claim: batch.claim,
            identity: self.identity.clone(),
            observations: batch.observations,
        };
        let status = send_dispatch_request(
            &self.client,
            &self.base_url,
            self.authorizer.as_deref(),
            STREAM_PREVIEW_BATCH_PATH,
            &self.identity.worker_id,
            &request,
        )
        .await?;
        if matches!(
            status.status(),
            StatusCode::NOT_FOUND | StatusCode::NOT_IMPLEMENTED
        ) {
            // A rolling deployment can still send to an older Coordinator.
            // Ambiguous delivery is never replayed through the old endpoint.
            for observation in request.observations {
                let response = send_dispatch_request(
                    &self.client,
                    &self.base_url,
                    self.authorizer.as_deref(),
                    "/v1/worker/dispatch/stream",
                    &self.identity.worker_id,
                    &StreamObservationRequest {
                        request: StreamEventRequest {
                            claim: request.claim.clone(),
                            identity: self.identity.clone(),
                            event: observation.event,
                        },
                        assistant_response: observation.assistant_response,
                        preview_sequence: observation.preview_sequence,
                    },
                )
                .await?;
                if !response.status().is_success() {
                    return Err(dispatch_response_error(response).await);
                }
            }
        } else if !status.status().is_success() {
            return Err(dispatch_response_error(status).await);
        }
        Ok(())
    }
}
