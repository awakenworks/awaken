//! NATS JetStream-buffered storage decorator.
//!
//! Wraps an inner [`ThreadRunStore`] and buffers checkpoint writes through
//! NATS JetStream for durability. The inner store receives the final
//! materialized state at flush time.
//!
//! # Flush strategy
//!
//! During a run, `checkpoint()` publishes messages and run data to a
//! JetStream subject `awaken.thread.{thread_id}.checkpoint`. When
//! `flush()` is called (typically at run end), buffered data is replayed
//! and persisted to the inner store.
//!
//! # Crash recovery
//!
//! Call [`NatsBufferedWriter::recover`] on startup to replay unacked
//! messages left from interrupted runs.

use async_nats::jetstream;
use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// JetStream stream name.
const STREAM_NAME: &str = "AWAKEN_CHECKPOINTS";
/// Subject prefix. Full subject: `awaken.thread.{thread_id}.checkpoint`.
const SUBJECT_PREFIX: &str = "awaken.thread";
/// Timeout for draining messages from a consumer.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn checkpoint_subject(thread_id: &str) -> String {
    format!("{SUBJECT_PREFIX}.{thread_id}.checkpoint")
}

/// Envelope published to JetStream for each checkpoint.
#[derive(Debug, Serialize, Deserialize)]
struct CheckpointEnvelope {
    thread_id: String,
    messages: Vec<Message>,
    run: RunRecord,
}

/// A [`ThreadRunStore`] decorator that buffers checkpoints in NATS JetStream.
pub struct NatsBufferedWriter {
    inner: Arc<dyn ThreadRunStore>,
    jetstream: jetstream::Context,
}

/// Errors specific to the NATS buffered writer.
#[derive(Debug, thiserror::Error)]
pub enum NatsBufferedWriterError {
    /// JetStream operation failed.
    #[error("jetstream error: {0}")]
    JetStream(String),
    /// Underlying storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl NatsBufferedWriter {
    /// Create a new buffered writer.
    ///
    /// Ensures the JetStream stream exists (idempotent).
    pub async fn new(
        inner: Arc<dyn ThreadRunStore>,
        jetstream: jetstream::Context,
    ) -> Result<Self, async_nats::Error> {
        jetstream
            .get_or_create_stream(jetstream::stream::Config {
                name: STREAM_NAME.to_string(),
                subjects: vec![format!("{SUBJECT_PREFIX}.*.checkpoint")],
                retention: jetstream::stream::RetentionPolicy::WorkQueue,
                storage: jetstream::stream::StorageType::File,
                max_age: std::time::Duration::from_secs(24 * 3600),
                ..Default::default()
            })
            .await?;

        Ok(Self { inner, jetstream })
    }

    /// Recover incomplete runs after a crash.
    ///
    /// Replays unacked checkpoint messages, applies the latest checkpoint
    /// for each thread to the inner store.
    pub async fn recover(&self) -> Result<usize, NatsBufferedWriterError> {
        let stream = self.stream().await?;
        let consumer_name = format!("recovery_{}", uuid::Uuid::now_v7().simple());
        let consumer = stream
            .create_consumer(jetstream::consumer::pull::Config {
                name: Some(consumer_name.clone()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                deliver_policy: jetstream::consumer::DeliverPolicy::All,
                filter_subject: format!("{SUBJECT_PREFIX}.*.checkpoint"),
                ..Default::default()
            })
            .await
            .map_err(|e| NatsBufferedWriterError::JetStream(e.to_string()))?;

        let mut pending: HashMap<String, Vec<(CheckpointEnvelope, jetstream::Message)>> =
            HashMap::new();
        let mut messages = consumer
            .messages()
            .await
            .map_err(|e| NatsBufferedWriterError::JetStream(e.to_string()))?;

        while let Ok(Some(Ok(msg))) = tokio::time::timeout(DRAIN_TIMEOUT, messages.next()).await {
            match serde_json::from_slice::<CheckpointEnvelope>(&msg.payload) {
                Ok(envelope) => {
                    let thread_id = envelope.thread_id.clone();
                    pending.entry(thread_id).or_default().push((envelope, msg));
                }
                Err(_) => {
                    let _ = msg.double_ack().await;
                }
            }
        }

        let mut recovered = 0usize;
        for (thread_id, envelopes) in pending {
            // Apply the last checkpoint for this thread
            if let Some((last, _)) = envelopes.last() {
                match self
                    .inner
                    .checkpoint(&thread_id, &last.messages, &last.run)
                    .await
                {
                    Ok(()) => {
                        for (_, msg) in &envelopes {
                            let _ = msg.double_ack().await;
                            recovered += 1;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            thread_id = %thread_id,
                            error = %e,
                            "recovery: failed to checkpoint thread"
                        );
                    }
                }
            }
        }

        let _ = stream.delete_consumer(&consumer_name).await;
        Ok(recovered)
    }

    /// Flush buffered checkpoints for a specific thread to the inner store.
    pub async fn flush(&self, thread_id: &str) -> Result<usize, NatsBufferedWriterError> {
        let stream = self.stream().await?;
        let consumer_name = format!("flush_{}", uuid::Uuid::now_v7().simple());
        let consumer = stream
            .create_consumer(jetstream::consumer::pull::Config {
                name: Some(consumer_name.clone()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                deliver_policy: jetstream::consumer::DeliverPolicy::All,
                filter_subject: checkpoint_subject(thread_id),
                ..Default::default()
            })
            .await
            .map_err(|e| NatsBufferedWriterError::JetStream(e.to_string()))?;

        let mut envelopes: Vec<(CheckpointEnvelope, jetstream::Message)> = Vec::new();
        let mut messages = consumer
            .messages()
            .await
            .map_err(|e| NatsBufferedWriterError::JetStream(e.to_string()))?;

        while let Ok(Some(Ok(msg))) = tokio::time::timeout(DRAIN_TIMEOUT, messages.next()).await {
            match serde_json::from_slice::<CheckpointEnvelope>(&msg.payload) {
                Ok(envelope) => envelopes.push((envelope, msg)),
                Err(_) => {
                    let _ = msg.double_ack().await;
                }
            }
        }

        if envelopes.is_empty() {
            let _ = stream.delete_consumer(&consumer_name).await;
            return Ok(0);
        }

        // Apply the latest checkpoint
        let (last_envelope, _) = envelopes.last().unwrap();
        self.inner
            .checkpoint(thread_id, &last_envelope.messages, &last_envelope.run)
            .await?;

        let mut flushed = 0;
        for (_, msg) in envelopes {
            let _ = msg.double_ack().await;
            flushed += 1;
        }

        let _ = stream.delete_consumer(&consumer_name).await;
        Ok(flushed)
    }

    async fn stream(&self) -> Result<jetstream::stream::Stream, NatsBufferedWriterError> {
        self.jetstream
            .get_stream(STREAM_NAME)
            .await
            .map_err(|e| NatsBufferedWriterError::JetStream(e.to_string()))
    }
}

#[async_trait]
impl ThreadRunStore for NatsBufferedWriter {
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(thread_id).await
    }

    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        let envelope = CheckpointEnvelope {
            thread_id: thread_id.to_owned(),
            messages: messages.to_vec(),
            run: run.clone(),
        };
        let payload = serde_json::to_vec(&envelope)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        self.jetstream
            .publish(checkpoint_subject(thread_id), payload.into())
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.load_run(run_id).await
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.latest_run(thread_id).await
    }
}
