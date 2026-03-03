//! NATS JetStream-buffered storage decorator.
//!
//! Wraps an inner [`ThreadWriter`] (typically PostgreSQL) and routes delta
//! writes through NATS JetStream instead of hitting the database per-delta.
//!
//! # Run-end flush strategy
//!
//! During a run the [`AgentOs::run_stream`] checkpoint background task calls
//! `append()` for each delta.  This storage publishes those deltas to a
//! JetStream subject `thread.{thread_id}.deltas` so they are durably buffered
//! in NATS.  No database writes happen during the run.
//!
//! When the run emits `CheckpointReason::RunFinished`, `append()` triggers a
//! flush for that thread: buffered deltas are materialized and persisted to the
//! inner storage via a single `save()`. The buffered NATS messages are then
//! acknowledged.
//!
//! `save()` remains available for explicit run-end flush when callers already
//! have a final materialized state.
//!
//! # Crash recovery
//!
//! On startup, call [`NatsBufferedThreadWriter::recover`] to replay any unacked
//! deltas left over from interrupted runs.

use async_nats::jetstream;
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tirea_contract::storage::{
    Committed, ThreadHead, ThreadListPage, ThreadListQuery, ThreadReader, ThreadStore,
    ThreadStoreError, ThreadWriter, VersionPrecondition,
};
use tirea_contract::{CheckpointReason, Thread, ThreadChangeSet};

/// NATS JetStream stream name for thread deltas.
const STREAM_NAME: &str = "THREAD_DELTAS";

/// Subject prefix.  Full subject: `thread.{thread_id}.deltas`.
const SUBJECT_PREFIX: &str = "thread";
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

fn delta_subject(thread_id: &str) -> String {
    format!("{SUBJECT_PREFIX}.{thread_id}.deltas")
}

/// A [`ThreadWriter`] decorator that buffers deltas in NATS JetStream and
/// flushes the final thread to the inner storage at run end.
///
/// # Query consistency (CQRS)
///
/// [`load`](ThreadReader::load) always reads from the inner (durable) storage.
/// During an active run, queries return the **last-flushed snapshot** — they do
/// not include deltas that are buffered in NATS but not yet flushed.
///
/// Real-time data for in-progress runs is delivered through the SSE/NATS event
/// stream.  Callers that need up-to-date messages during a run should consume
/// the event stream rather than polling the query API.
pub struct NatsBufferedThreadWriter {
    inner: Arc<dyn ThreadStore>,
    jetstream: jetstream::Context,
}

impl NatsBufferedThreadWriter {
    /// Create a new buffered storage.
    ///
    /// `inner` is the durable backend (e.g. PostgreSQL) used for `create`,
    /// `load`, `delete`, and the final `save` at run end.
    ///
    /// `jetstream` is an already-connected JetStream context.
    pub async fn new(
        inner: Arc<dyn ThreadStore>,
        jetstream: jetstream::Context,
    ) -> Result<Self, async_nats::Error> {
        // Ensure the stream exists (idempotent).
        jetstream
            .get_or_create_stream(jetstream::stream::Config {
                name: STREAM_NAME.to_string(),
                subjects: vec![format!("{SUBJECT_PREFIX}.*.deltas")],
                retention: jetstream::stream::RetentionPolicy::WorkQueue,
                storage: jetstream::stream::StorageType::File,
                max_age: std::time::Duration::from_secs(24 * 3600), // 24h TTL
                ..Default::default()
            })
            .await?;

        Ok(Self { inner, jetstream })
    }

    /// Recover incomplete runs after a crash.
    ///
    /// Replays any unacked deltas from the JetStream stream, applies them to
    /// the corresponding threads loaded from the inner storage, and saves the
    /// result.  Acked messages are then purged.
    pub async fn recover(&self) -> Result<usize, NatsBufferedThreadWriterError> {
        let stream = self.stream().await?;
        let consumer_name = format!("recovery_{}", uuid::Uuid::now_v7().simple());
        let consumer = stream
            .create_consumer(jetstream::consumer::pull::Config {
                name: Some(consumer_name.clone()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                deliver_policy: jetstream::consumer::DeliverPolicy::All,
                filter_subject: format!("{SUBJECT_PREFIX}.*.deltas"),
                ..Default::default()
            })
            .await
            .map_err(|e| NatsBufferedThreadWriterError::JetStream(e.to_string()))?;

        let mut pending: HashMap<String, Vec<(ThreadChangeSet, jetstream::Message)>> =
            HashMap::new();
        let mut messages = consumer
            .messages()
            .await
            .map_err(|e| NatsBufferedThreadWriterError::JetStream(e.to_string()))?;

        while let Ok(Some(Ok(msg))) = tokio::time::timeout(DRAIN_TIMEOUT, messages.next()).await {
            let subject = msg.subject.to_string();
            let parts: Vec<&str> = subject.split('.').collect();
            if parts.len() != 3 {
                let _ = msg.double_ack().await;
                continue;
            }
            let thread_id = parts[1].to_string();
            match serde_json::from_slice::<ThreadChangeSet>(&msg.payload) {
                Ok(delta) => pending.entry(thread_id).or_default().push((delta, msg)),
                Err(_) => {
                    let _ = msg.double_ack().await;
                }
            }
        }

        let mut recovered = 0usize;
        for (thread_id, deltas_with_msgs) in pending {
            match self
                .materialize_and_save_thread(&thread_id, deltas_with_msgs)
                .await
            {
                Ok(acked) => recovered += acked,
                Err(e) => {
                    tracing::error!(
                        thread_id = %thread_id,
                        error = %e,
                        "recovery: failed to materialize thread"
                    );
                }
            }
        }

        let _ = stream.delete_consumer(&consumer_name).await;
        Ok(recovered)
    }

    async fn stream(&self) -> Result<jetstream::stream::Stream, NatsBufferedThreadWriterError> {
        self.jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|e| NatsBufferedThreadWriterError::JetStream(e.to_string()))
    }

    async fn materialize_and_save_thread(
        &self,
        thread_id: &str,
        deltas_with_msgs: Vec<(ThreadChangeSet, jetstream::Message)>,
    ) -> Result<usize, NatsBufferedThreadWriterError> {
        if deltas_with_msgs.is_empty() {
            return Ok(0);
        }

        let mut thread = match self.inner.load(thread_id).await? {
            Some(head) => head.thread,
            None => Thread::new(thread_id.to_string()),
        };

        for (delta, _) in &deltas_with_msgs {
            delta.apply_to(&mut thread);
        }

        self.inner.save(&thread).await?;

        let mut acked = 0usize;
        for (_, msg) in deltas_with_msgs {
            let _ = msg.double_ack().await;
            acked += 1;
        }
        Ok(acked)
    }

    async fn flush_thread_buffer(
        &self,
        thread_id: &str,
    ) -> Result<usize, NatsBufferedThreadWriterError> {
        let stream = self.stream().await?;
        let consumer_name = format!("flush_{}", uuid::Uuid::now_v7().simple());
        let consumer = stream
            .create_consumer(jetstream::consumer::pull::Config {
                name: Some(consumer_name.clone()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                deliver_policy: jetstream::consumer::DeliverPolicy::All,
                filter_subject: delta_subject(thread_id),
                ..Default::default()
            })
            .await
            .map_err(|e| NatsBufferedThreadWriterError::JetStream(e.to_string()))?;

        let mut deltas_with_msgs = Vec::new();
        let mut messages = consumer
            .messages()
            .await
            .map_err(|e| NatsBufferedThreadWriterError::JetStream(e.to_string()))?;

        while let Ok(Some(Ok(msg))) = tokio::time::timeout(DRAIN_TIMEOUT, messages.next()).await {
            match serde_json::from_slice::<ThreadChangeSet>(&msg.payload) {
                Ok(delta) => deltas_with_msgs.push((delta, msg)),
                Err(_) => {
                    let _ = msg.double_ack().await;
                }
            }
        }

        let result = self
            .materialize_and_save_thread(thread_id, deltas_with_msgs)
            .await;
        let _ = stream.delete_consumer(&consumer_name).await;
        result
    }
}

#[async_trait]
impl ThreadWriter for NatsBufferedThreadWriter {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        self.inner.create(thread).await
    }

    /// Publish delta to NATS JetStream instead of writing to database.
    ///
    /// The delta is durably stored in JetStream and will be purged after the
    /// run-end `save()` succeeds.  If publishing fails the error is mapped to
    /// [`ThreadStoreError::Io`].
    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        let payload = serde_json::to_vec(delta)
            .map_err(|e| ThreadStoreError::Serialization(e.to_string()))?;

        self.jetstream
            .publish(delta_subject(thread_id), payload.into())
            .await
            .map_err(|e| ThreadStoreError::Io(std::io::Error::other(e)))?
            .await
            .map_err(|e| ThreadStoreError::Io(std::io::Error::other(e)))?;

        if delta.reason == CheckpointReason::RunFinished {
            self.flush_thread_buffer(thread_id)
                .await
                .map_err(|e| match e {
                    NatsBufferedThreadWriterError::JetStream(msg) => {
                        ThreadStoreError::Io(std::io::Error::other(msg))
                    }
                    NatsBufferedThreadWriterError::Storage(err) => err,
                })?;
        }

        let version = match precondition {
            VersionPrecondition::Any => 0,
            VersionPrecondition::Exact(v) => v.saturating_add(1),
        };
        Ok(Committed { version })
    }

    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
        self.inner.delete(thread_id).await
    }

    /// Run-end flush: saves the final materialized thread to the inner storage
    /// and purges the corresponding NATS JetStream messages.
    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        // Write to durable storage.
        self.inner.save(thread).await?;

        // Best-effort purge of buffered deltas for this thread.
        if let Ok(stream) = self.jetstream.get_stream(STREAM_NAME).await {
            let _ = stream.purge().filter(delta_subject(&thread.id)).await;
        }

        Ok(())
    }
}

#[async_trait]
impl ThreadReader for NatsBufferedThreadWriter {
    async fn load(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        self.inner.load(thread_id).await
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        self.inner.list_threads(query).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NatsBufferedThreadWriterError {
    #[error("jetstream error: {0}")]
    JetStream(String),

    #[error("storage error: {0}")]
    Storage(#[from] ThreadStoreError),
}
