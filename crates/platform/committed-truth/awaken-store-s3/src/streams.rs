// @formal entrypoint store_history.s3_sealed_collection_preserves_reachable_objects
use crate::{
    BlobRef, Error, Result, SessionStorage, StreamBatch, StreamCursor,
    model::{CheckpointRef, Record, StreamFrame, next},
    objects::digest,
};
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_worker_contract::RunClaim;
use bytes::Bytes;

#[derive(Clone)]
pub struct SessionStreams {
    pub(crate) session: SessionStorage,
}
impl SessionStreams {
    pub fn checkpoints(&self, claim: RunClaim) -> BoundCheckpoints {
        BoundCheckpoints {
            session: self.clone(),
            claim,
        }
    }
    pub async fn put_checkpoint(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
    ) -> Result<()> {
        self.put_checkpoint_authorized(claim, checkpoint, None)
            .await
    }
    pub async fn put_checkpoint_conditionally(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<()> {
        self.put_checkpoint_authorized(claim, checkpoint, Some(condition))
            .await
    }
    async fn put_checkpoint_authorized(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
        condition: Option<&awaken_session_types::SessionPublicationCondition>,
    ) -> Result<()> {
        if checkpoint.run_id != claim.run_id.0 {
            return Err(Error::Invalid("checkpoint Run mismatch".into()));
        }
        let initial = self.session.inner.load_head(&self.session.key).await?.0;
        let (thread, step) = if let Some(condition) = condition {
            self.session
                .canonical_inflight(&initial, claim, condition)
                .await?
        } else {
            self.session.require_claim(&initial, claim)?;
            let row = initial.runs.get(&claim.run_id.0).ok_or(Error::Fenced)?;
            (row.thread.clone(), row.next_ordinal)
        };
        if thread != checkpoint.thread_id {
            return Err(Error::Invalid("checkpoint Thread mismatch".into()));
        }
        for _ in 0..self.session.inner.limits.max_cas_attempts {
            let (mut head, version) = self.session.inner.load_head(&self.session.key).await?;
            let current_step = if let Some(condition) = condition {
                self.session
                    .canonical_inflight(&head, claim, condition)
                    .await?
                    .1
            } else {
                self.session.require_claim(&head, claim)?;
                head.runs
                    .get(&claim.run_id.0)
                    .ok_or(Error::Fenced)?
                    .next_ordinal
            };
            if current_step != step {
                return Err(Error::Conflict(
                    "checkpoint step advanced during write".into(),
                ));
            }
            if let Some(run) = self.session.replay(&head).await?.run(&claim.run_id)
                && !matches!(
                    run.state,
                    awaken_agent_contract::agent::run::RunState::Running
                )
            {
                return Err(Error::Conflict("Run no longer inferring".into()));
            }
            let object = self
                .session
                .inner
                .put_json(
                    &self.session.key,
                    head.gc_generation,
                    "streams",
                    &Record::Checkpoint(checkpoint.clone()),
                )
                .await?;
            head.checkpoints.insert(
                claim.run_id.0.clone(),
                CheckpointRef {
                    object,
                    claim: claim.clone(),
                    stream: None,
                },
            );
            if self.session.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
    pub async fn checkpoint(&self, run: &str) -> Observed<StreamCheckpoint> {
        match self.session.inner.load_head(&self.session.key).await {
            Ok((head, _)) => self.checkpoint_at(&head, run).await,
            Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
        }
    }
    pub(crate) async fn checkpoint_at(
        &self,
        head: &crate::model::Head,
        run: &str,
    ) -> Observed<StreamCheckpoint> {
        let Some(reference) = head.checkpoints.get(run) else {
            return Observed::Absent;
        };
        let result: Result<StreamCheckpoint> = async {
            match self
                .session
                .inner
                .read_json(&self.session.key, &reference.object)
                .await?
            {
                Record::Checkpoint(checkpoint) if checkpoint.run_id == run => {
                    let expected = if let Some(row) = head.runs.get(run) {
                        row.thread.clone()
                    } else {
                        let request: awaken_dispatch_contract::RunDispatch =
                            serde_json::from_value(
                                self.session
                                    .component_record_at(head, "dispatch", &format!("run:{run}"))
                                    .await?
                                    .ok_or_else(|| {
                                        Error::Corrupt("checkpoint Run binding absent".into())
                                    })?,
                            )
                            .map_err(crate::objects::corrupt)?;
                        request.thread_id().0.clone()
                    };
                    if checkpoint.thread_id != expected {
                        return Err(Error::Corrupt("checkpoint Thread binding mismatch".into()));
                    }
                    Ok(checkpoint)
                }
                _ => Err(Error::Corrupt("checkpoint identity or record kind".into())),
            }
        }
        .await;
        match result {
            Ok(checkpoint) => Observed::Present(checkpoint),
            Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
        }
    }
    pub async fn delete_checkpoint(&self, claim: &RunClaim, run: &str) -> Result<()> {
        self.delete_checkpoint_authorized(claim, run, None).await
    }
    pub async fn delete_checkpoint_conditionally(
        &self,
        claim: &RunClaim,
        run: &str,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<()> {
        self.delete_checkpoint_authorized(claim, run, Some(condition))
            .await
    }
    async fn delete_checkpoint_authorized(
        &self,
        claim: &RunClaim,
        run: &str,
        condition: Option<&awaken_session_types::SessionPublicationCondition>,
    ) -> Result<()> {
        if run != claim.run_id.0 {
            return Err(Error::Invalid("checkpoint Run mismatch".into()));
        }
        let initial = self.session.inner.load_head(&self.session.key).await?.0;
        if let Some(condition) = condition {
            self.session
                .inner
                .validate_claim_condition(&initial, claim, condition, false)
                .await?;
        } else {
            self.session.require_claim(&initial, claim)?;
        }
        let expected = initial.checkpoints.get(run).map(|r| r.object.clone());
        for _ in 0..self.session.inner.limits.max_cas_attempts {
            let (mut head, version) = self.session.inner.load_head(&self.session.key).await?;
            if let Some(condition) = condition {
                self.session
                    .inner
                    .validate_claim_condition(&head, claim, condition, false)
                    .await?;
            } else {
                self.session.require_claim(&head, claim)?;
            }
            let Some(reference) = head.checkpoints.get(run) else {
                return Ok(());
            };
            if Some(&reference.object) != expected.as_ref() {
                return Err(Error::Conflict("checkpoint changed during delete".into()));
            }
            head.checkpoints.remove(run);
            if self.session.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    /// Persist one binary batch (text, audio, image or provider framing).
    /// next_frame is the batch's zero-based append ordinal, independent of commit order.
    pub async fn append_stream(
        &self,
        claim: &RunClaim,
        cursor: StreamCursor,
        bytes: Bytes,
    ) -> Result<StreamCursor> {
        self.append_stream_authorized(claim, cursor, bytes, "application/octet-stream", None)
            .await
    }
    pub async fn append_stream_conditionally(
        &self,
        claim: &RunClaim,
        cursor: StreamCursor,
        bytes: Bytes,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<StreamCursor> {
        self.append_stream_authorized(
            claim,
            cursor,
            bytes,
            "application/octet-stream",
            Some(condition),
        )
        .await
    }
    pub async fn append_media_stream(
        &self,
        claim: &RunClaim,
        cursor: StreamCursor,
        bytes: Bytes,
        content_type: &str,
    ) -> Result<StreamCursor> {
        self.append_stream_authorized(claim, cursor, bytes, content_type, None)
            .await
    }
    pub async fn append_media_stream_conditionally(
        &self,
        claim: &RunClaim,
        cursor: StreamCursor,
        bytes: Bytes,
        content_type: &str,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<StreamCursor> {
        self.append_stream_authorized(claim, cursor, bytes, content_type, Some(condition))
            .await
    }
    async fn append_stream_authorized(
        &self,
        claim: &RunClaim,
        cursor: StreamCursor,
        bytes: Bytes,
        content_type: &str,
        condition: Option<&awaken_session_types::SessionPublicationCondition>,
    ) -> Result<StreamCursor> {
        if content_type.is_empty()
            || content_type.len() > 256
            || content_type.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(Error::Invalid("invalid stream content type".into()));
        }
        if cursor.run != claim.run_id.0 {
            return Err(Error::Invalid("stream Run mismatch".into()));
        }
        let identity = stream_key(&cursor);
        for _ in 0..self.session.inner.limits.max_cas_attempts {
            let (mut head, version) = self.session.inner.load_head(&self.session.key).await?;
            let ordinal = if let Some(condition) = condition {
                self.session
                    .canonical_inflight(&head, claim, condition)
                    .await?
                    .1
            } else {
                self.session.require_claim(&head, claim)?;
                head.runs
                    .get(&cursor.run)
                    .ok_or(Error::Fenced)?
                    .next_ordinal
            };
            if condition.is_some() && cursor.attempt != claim.epoch {
                return Err(Error::Fenced);
            }
            if cursor.step != ordinal {
                return Err(Error::Conflict("stream step is no longer in flight".into()));
            }
            let frames = head.frames.entry(identity.clone()).or_default();
            if cursor.next_frame < frames.next_frame {
                let existing = self
                    .frame_range(frames, cursor.next_frame, cursor.next_frame + 1)
                    .await?;
                let stored = &existing[0];
                if stored.cursor.next_frame != cursor.next_frame + 1
                    || stored.object.sha256 != digest(&bytes)
                    || stored.content_type != content_type
                {
                    return Err(Error::Conflict(
                        "stream ordinal payload or content type mismatch".into(),
                    ));
                }
                return Ok(stored.cursor.clone());
            }
            if cursor.next_frame != frames.next_frame {
                return Err(Error::Conflict("stream gap".into()));
            }
            let object = self
                .session
                .inner
                .put_bytes(
                    &self.session.key,
                    head.gc_generation,
                    "streams",
                    bytes.clone(),
                )
                .await?;
            let mut after = cursor.clone();
            after.next_frame = next(cursor.next_frame)?;
            let sequence = next(head.stream_sequence)?;
            head.stream_sequence = sequence;
            self.append_frame_index(
                head.gc_generation,
                frames,
                StreamFrame {
                    cursor: after.clone(),
                    object,
                    sequence,
                    content_type: content_type.into(),
                },
            )
            .await?;
            if self.session.publish_head(&mut head, version).await? {
                return Ok(after);
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    pub async fn read_stream(
        &self,
        cursor: &StreamCursor,
        limit: usize,
    ) -> Result<Vec<StreamBatch>> {
        let (head, _) = self.session.inner.load_head(&self.session.key).await?;
        let Some(frames) = self.stream_log_at(&head, &stream_key(cursor)).await? else {
            return if cursor.next_frame == 0 {
                Ok(Vec::new())
            } else {
                Err(Error::Invalid("stream cursor unavailable".into()))
            };
        };
        if cursor.next_frame > frames.next_frame {
            return Err(Error::Invalid("stream cursor ahead".into()));
        }
        let mut batches = Vec::new();
        let end = cursor
            .next_frame
            .saturating_add(limit as u64)
            .min(frames.next_frame);
        for frame in self.frame_range(&frames, cursor.next_frame, end).await? {
            if stream_key(&frame.cursor) != stream_key(cursor) {
                return Err(Error::Corrupt("stream frame identity mismatch".into()));
            }
            batches.push(StreamBatch {
                cursor: frame.cursor,
                sequence: frame.sequence,
                content_type: frame.content_type,
                bytes: self
                    .session
                    .inner
                    .read_bytes(&self.session.key, &frame.object)
                    .await?,
            });
        }
        Ok(batches)
    }

    /// Upload an attachment before a commit references it. Unreferenced uploads are collectible.
    pub async fn upload_blob(&self, bytes: Bytes, media_type: &str) -> Result<BlobRef> {
        if media_type.is_empty() || media_type.len() > 256 {
            return Err(Error::Invalid("invalid media type".into()));
        }
        let (head, _) = self.session.inner.load_head(&self.session.key).await?;
        let size = bytes.len() as u64;
        let object = self
            .session
            .inner
            .put_bytes(&self.session.key, head.gc_generation, "blobs", bytes)
            .await?;
        Ok(BlobRef {
            object,
            media_type: media_type.into(),
            size,
        })
    }
    pub async fn read_blob(&self, reference: &BlobRef) -> Result<Bytes> {
        let bytes = self
            .session
            .inner
            .read_bytes(&self.session.key, &reference.object)
            .await?;
        if bytes.len() as u64 != reference.size {
            return Err(Error::Corrupt("blob size mismatch".into()));
        }
        Ok(bytes)
    }
    /// Pin explicit Session resources. Message-owned artifacts should ride commit_with_blobs.
    pub async fn pin_blob(&self, name: &str, blob: BlobRef) -> Result<()> {
        if name.is_empty() || name.len() > 200 {
            return Err(Error::Invalid("invalid blob alias".into()));
        }
        self.read_blob(&blob).await?;
        for _ in 0..self.session.inner.limits.max_cas_attempts {
            let (mut head, version) = self.session.inner.load_head(&self.session.key).await?;
            if !crate::maintenance::blob_pin_admitted(
                blob.object.generation,
                head.gc_generation,
                || head.blobs.values().any(|b| b == &blob),
            ) {
                return Err(Error::Conflict("upload epoch sealed; upload again".into()));
            }
            head.blobs.insert(name.into(), blob.clone());
            if self.session.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
}
pub(crate) fn stream_key(cursor: &StreamCursor) -> String {
    let mut key = format!(
        "{}:{}:{}:{}",
        cursor.run, cursor.step, cursor.response, cursor.attempt
    );
    if let Some(step) = cursor.assistant_step {
        use std::fmt::Write as _;
        let _ = write!(key, ":assistant:{step}");
    }
    key
}

/// Runtime's existing checkpoint contract, with the exact claim bound at construction.
#[derive(Clone)]
pub struct BoundCheckpoints {
    session: SessionStreams,
    claim: RunClaim,
}
#[async_trait::async_trait]
impl StreamCheckpointStore for BoundCheckpoints {
    async fn get(&self, run: &str) -> Observed<StreamCheckpoint> {
        if run != self.claim.run_id.0 {
            return Observed::Unknown(UnknownCause::Fenced("checkpoint Run mismatch".into()));
        }
        let (head, _) = match self
            .session
            .session
            .inner
            .load_head(&self.session.session.key)
            .await
        {
            Ok(head) => head,
            Err(error) => return Observed::Unknown(UnknownCause::Storage(error.to_string())),
        };
        if let Err(error) = self.session.session.require_claim(&head, &self.claim) {
            return Observed::Unknown(UnknownCause::Fenced(error.to_string()));
        }
        self.session.checkpoint_at(&head, run).await
    }
    async fn put(
        &self,
        checkpoint: StreamCheckpoint,
    ) -> std::result::Result<(), StreamCheckpointError> {
        self.session
            .put_checkpoint(&self.claim, checkpoint)
            .await
            .map_err(checkpoint_error)
    }
    async fn delete(&self, run: &str) -> std::result::Result<(), StreamCheckpointError> {
        self.session
            .delete_checkpoint(&self.claim, run)
            .await
            .map_err(checkpoint_error)
    }
}
fn checkpoint_error(error: Error) -> StreamCheckpointError {
    match error {
        Error::Fenced => StreamCheckpointError::Fenced(error.to_string()),
        error => StreamCheckpointError::Storage(error.to_string()),
    }
}
