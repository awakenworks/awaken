// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Allocates observation positions for concurrent publishers and preserves the committed sequence.
//! Persist the existing native/ACP observation envelope before live publication.
use crate::{Error, Result, SessionStreams, StreamCursor};
use awaken_agent_contract::stream::{
    event::{Event, Observation},
    sink::Error as StreamError,
};
use awaken_dispatch_contract::ClaimedStreamPublisher;
use awaken_worker_contract::RunClaim;
use std::sync::Arc;
pub(crate) const OBSERVATIONS_CONTENT_TYPE: &str = "application/vnd.awaken.observations+json";

#[derive(serde::Serialize, serde::Deserialize)]
struct ObservationBatch {
    format: u32,
    #[serde(default)]
    publication_id: Option<uuid::Uuid>,
    observations: Vec<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    producer: Option<String>,
}
/// One Session's durable progress publisher. The existing claim-bound Runtime
/// sink works for both native and ACP executors; it adds no provider vocabulary.
pub struct SessionProgressPublisher {
    session: SessionStreams,
    downstream: Arc<dyn ClaimedStreamPublisher>,
    serial: tokio::sync::Mutex<()>,
    dispatch: Option<Arc<dyn awaken_dispatch_contract::DispatchCommitAuthority>>,
}
impl SessionStreams {
    pub fn progress_publisher(
        &self,
        downstream: Arc<dyn ClaimedStreamPublisher>,
    ) -> SessionProgressPublisher {
        SessionProgressPublisher {
            session: self.clone(),
            downstream,
            serial: tokio::sync::Mutex::new(()),
            dispatch: None,
        }
    }
    pub fn canonical_progress_publisher(
        &self,
        dispatch: Arc<dyn awaken_dispatch_contract::DispatchCommitAuthority>,
        downstream: Arc<dyn ClaimedStreamPublisher>,
    ) -> SessionProgressPublisher {
        let mut publisher = self.progress_publisher(downstream);
        publisher.dispatch = Some(dispatch);
        publisher
    }
    pub async fn stream_watermarks(&self, run: &str) -> Result<Vec<StreamCursor>> {
        let (head, _) = self.session.inner.load_head(&self.session.key).await?;
        let mut result = Vec::new();
        for log in self.stream_logs_at(&head).await?.values() {
            if let Some(identity) = &log.identity
                && identity.run == run
            {
                let mut cursor = identity.clone();
                cursor.next_frame = log.next_frame;
                result.push(cursor);
            }
        }
        Ok(result)
    }
    pub async fn read_observations(
        &self,
        cursor: &StreamCursor,
        limit: usize,
    ) -> Result<Vec<Observation>> {
        let mut observations = Vec::new();
        for batch in self.read_stream(cursor, limit).await? {
            let batch: ObservationBatch =
                serde_json::from_slice(&batch.bytes).map_err(crate::objects::corrupt)?;
            if batch.format != 1
                || batch
                    .observations
                    .iter()
                    .any(|o| o.event.run_id.0 != cursor.run)
            {
                return Err(Error::Corrupt(
                    "observation batch identity or format".into(),
                ));
            }
            observations.extend(batch.observations);
        }
        Ok(observations)
    }
}
impl SessionProgressPublisher {
    /// Batch-capable entry for transports already grouping model/media updates.
    /// Returning a cursor acknowledges durable publication; live delivery can fail independently.
    pub async fn persist_batch(
        &self,
        claim: &RunClaim,
        observations: Vec<Observation>,
    ) -> Result<StreamCursor> {
        if observations.is_empty() || observations.iter().any(|o| o.event.run_id != claim.run_id) {
            return Err(Error::Invalid(
                "empty or foreign-Run observation batch".into(),
            ));
        }
        let _guard = self.serial.lock().await;
        let condition = if let Some(dispatch) = &self.dispatch {
            match dispatch
                .acquire_commit_authority(claim)
                .await
                .map_err(|e| Error::Storage(e.to_string()))?
            {
                Some(awaken_dispatch_contract::ClaimAuthority::Conditional(authority)) => {
                    Some(authority.condition)
                }
                Some(awaken_dispatch_contract::ClaimAuthority::Locked(guard)) => {
                    guard.release().await.map_err(Error::Storage)?;
                    return Err(Error::Invalid(
                        "canonical S3 progress requires a conditional authority".into(),
                    ));
                }
                None => return Err(Error::Fenced),
            }
        } else {
            None
        };
        let (head, _) = self
            .session
            .session
            .inner
            .load_head(&self.session.session.key)
            .await?;
        let (thread, ordinal) = if let Some(condition) = &condition {
            self.session
                .session
                .canonical_inflight(&head, claim, condition)
                .await?
        } else {
            self.session.session.require_claim(&head, claim)?;
            let row = head.runs.get(&claim.run_id.0).ok_or(Error::Fenced)?;
            (row.thread.clone(), row.next_ordinal)
        };
        if observations
            .iter()
            .filter_map(|o| o.assistant_response.as_ref())
            .any(|c| c.thread_id.0 != thread)
        {
            return Err(Error::Invalid("observation Thread mismatch".into()));
        }
        let response = observations[0]
            .assistant_response
            .as_ref()
            .map_or(0, |c| c.response as u64);
        if observations.iter().any(|observation| {
            observation
                .assistant_response
                .as_ref()
                .map_or(0, |coordinate| coordinate.response as u64)
                != response
        }) {
            return Err(Error::Invalid(
                "one observation batch must identify one response stream".into(),
            ));
        }
        let mut cursor = StreamCursor {
            run: claim.run_id.0.clone(),
            step: ordinal,
            response,
            assistant_step: None,
            attempt: claim.epoch,
            next_frame: 0,
        };
        let key = crate::streams::stream_key(&cursor);
        cursor.next_frame = head.frames.get(&key).map_or(0, |log| log.next_frame);
        let bytes = bytes::Bytes::from(
            serde_json::to_vec(&ObservationBatch {
                format: 1,
                publication_id: Some(uuid::Uuid::new_v4()),
                observations,
                producer: None,
            })
            .map_err(crate::objects::corrupt)?,
        );
        // The same frame identity and bytes survive an unknown write response.
        for _ in 0..self.session.session.inner.limits.max_cas_attempts {
            let result = if let Some(condition) = &condition {
                self.session
                    .append_media_stream_conditionally(
                        claim,
                        cursor.clone(),
                        bytes.clone(),
                        OBSERVATIONS_CONTENT_TYPE,
                        condition,
                    )
                    .await
            } else {
                self.session
                    .append_media_stream(
                        claim,
                        cursor.clone(),
                        bytes.clone(),
                        OBSERVATIONS_CONTENT_TYPE,
                    )
                    .await
            };
            match result {
                Err(Error::OutcomeUnknown(_)) => continue,
                Err(error @ Error::Conflict(_)) => {
                    // Another publisher may have taken the proposed ordinal.
                    // Advance only after durable bytes prove this batch was not
                    // published there. Its publication ID survives unknown ACKs.
                    let (head, _) = self
                        .session
                        .session
                        .inner
                        .load_head(&self.session.session.key)
                        .await?;
                    let Some(log) = head.frames.get(&key) else {
                        return Err(error);
                    };
                    if cursor.next_frame >= log.next_frame {
                        return Err(error);
                    }
                    let stored = self
                        .session
                        .frame_range(log, cursor.next_frame, cursor.next_frame + 1)
                        .await?;
                    if stored[0].object.sha256 == crate::objects::digest(&bytes) {
                        return Err(error);
                    }
                    cursor.next_frame = log.next_frame;
                }
                result => return result,
            }
        }
        Err(Error::OutcomeUnknown(
            "progress publication could not be reconciled".into(),
        ))
    }
}
#[async_trait::async_trait]
impl ClaimedStreamPublisher for SessionProgressPublisher {
    async fn publish(
        &self,
        claim: &RunClaim,
        event: Event,
    ) -> std::result::Result<(), StreamError> {
        self.publish_observation(claim, event.into()).await
    }
    async fn publish_observation(
        &self,
        claim: &RunClaim,
        observation: Observation,
    ) -> std::result::Result<(), StreamError> {
        self.persist_batch(claim, vec![observation.clone()])
            .await
            .map_err(|_| StreamError::Closed)?;
        self.downstream
            .publish_observation(claim, observation)
            .await
    }
}

impl crate::StreamReadPage {
    /// Decode observation batches in durable arrival order. Other media remain in batches.
    pub fn observations(&self) -> Result<Vec<Observation>> {
        let mut observations = Vec::new();
        for frame in &self.batches {
            if frame.content_type != OBSERVATIONS_CONTENT_TYPE {
                continue;
            }
            let batch: ObservationBatch =
                serde_json::from_slice(&frame.bytes).map_err(crate::objects::corrupt)?;
            if batch.format != 1
                || batch
                    .observations
                    .iter()
                    .any(|o| o.event.run_id.0 != self.next_cursor.run)
            {
                return Err(Error::Corrupt(
                    "observation batch identity or format".into(),
                ));
            }
            observations.extend(batch.observations);
        }
        Ok(observations)
    }
}

impl SessionStreams {
    pub(crate) fn encode_progress(
        claim: &RunClaim,
        batch: awaken_dispatch_contract::ProgressBatch,
    ) -> Result<(StreamCursor, bytes::Bytes)> {
        let cursor = StreamCursor {
            run: claim.run_id.0.clone(),
            step: batch.commit_ordinal,
            response: batch.response.response as u64,
            assistant_step: Some(batch.response.step as u64),
            attempt: claim.epoch,
            next_frame: batch.batch_index,
        };
        let bytes = bytes::Bytes::from(
            serde_json::to_vec(&ObservationBatch {
                format: 1,
                publication_id: None,
                observations: batch.observations,
                producer: Some(claim.owner.clone()),
            })
            .map_err(crate::objects::corrupt)?,
        );
        Ok((cursor, bytes))
    }

    pub(crate) async fn replay_progress_receipt(
        &self,
        cursor: &StreamCursor,
        bytes: &bytes::Bytes,
    ) -> Result<Option<StreamCursor>> {
        let (head, _) = self.session.inner.load_head(&self.session.key).await?;
        let key = crate::streams::stream_key(cursor);
        let archived;
        let log = if let Some(log) = head.frames.get(&key) {
            Some(log)
        } else {
            archived = self.stream_logs_at(&head).await?;
            archived.get(&key)
        };
        let Some(log) = log.filter(|log| cursor.next_frame < log.next_frame) else {
            return Ok(None);
        };
        let frames = self
            .frame_range(log, cursor.next_frame, cursor.next_frame + 1)
            .await?;
        Ok(frames.into_iter().find_map(|frame| {
            (frame.object.sha256 == crate::objects::digest(bytes)
                && frame.content_type == OBSERVATIONS_CONTENT_TYPE)
                .then_some(frame.cursor)
        }))
    }
}
