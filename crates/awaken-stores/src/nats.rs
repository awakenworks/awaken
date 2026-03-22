//! NATS JetStream-buffered storage decorator.
//!
//! Wraps an inner [`ThreadRunStore`] and buffers checkpoint writes through
//! NATS JetStream for durability. The inner store receives the final
//! materialized state at flush time.
//!
//! # Flush strategy
//!
//! During a run, `checkpoint()` publishes messages and run data to a
//! JetStream subject `awaken.threads.{thread_id}.messages`. When
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
use awaken_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// JetStream stream name for awaken checkpoints.
const STREAM_NAME: &str = "AWAKEN_CHECKPOINTS";

/// Subject prefix. Full subject: `awaken.threads.{thread_id}.messages`.
const SUBJECT_PREFIX: &str = "awaken.threads";

/// Timeout for draining messages from a consumer.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn checkpoint_subject(thread_id: &str) -> String {
    format!("{SUBJECT_PREFIX}.{thread_id}.messages")
}

/// Envelope published to JetStream for each checkpoint.
#[derive(Debug, Serialize, Deserialize)]
struct CheckpointEnvelope {
    thread_id: String,
    messages: Vec<Message>,
    run: RunRecord,
}

/// A [`ThreadRunStore`] decorator that buffers checkpoints in NATS JetStream.
///
/// Reads always delegate to the inner (durable) store. Checkpoints are
/// published to JetStream and only flushed to the inner store when
/// [`flush`](Self::flush) is called.
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
                subjects: vec![format!("{SUBJECT_PREFIX}.*.messages")],
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
    /// for each thread to the inner store. Returns the number of recovered
    /// messages.
    pub async fn recover(&self) -> Result<usize, NatsBufferedWriterError> {
        let stream = self.stream().await?;
        let consumer_name = format!("recovery_{}", uuid::Uuid::now_v7().simple());
        let consumer = stream
            .create_consumer(jetstream::consumer::pull::Config {
                name: Some(consumer_name.clone()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                deliver_policy: jetstream::consumer::DeliverPolicy::All,
                filter_subject: format!("{SUBJECT_PREFIX}.*.messages"),
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
    ///
    /// Drains JetStream messages for the given thread, applies the latest
    /// checkpoint, and acknowledges all consumed messages.
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
impl ThreadStore for NatsBufferedWriter {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        self.inner.load_thread(thread_id).await
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(thread).await
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        self.inner.list_threads(offset, limit).await
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(thread_id).await
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.inner.save_messages(thread_id, messages).await
    }

    async fn delete_thread(&self, id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(id).await
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(thread_id).await
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: awaken_contract::thread::ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.inner.update_thread_metadata(id, metadata).await
    }
}

#[async_trait]
impl RunStore for NatsBufferedWriter {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.inner.create_run(record).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.load_run(run_id).await
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.inner.latest_run(thread_id).await
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        self.inner.list_runs(query).await
    }
}

#[async_trait]
impl ThreadRunStore for NatsBufferedWriter {
    /// Publish checkpoint to NATS JetStream instead of writing directly to
    /// the inner store. The envelope is durably stored in JetStream and will
    /// be applied to the inner store when [`flush`](Self::flush) is called.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_subject_format() {
        assert_eq!(
            checkpoint_subject("thread-123"),
            "awaken.threads.thread-123.messages"
        );
    }

    #[test]
    fn checkpoint_envelope_serde_roundtrip() {
        let envelope = CheckpointEnvelope {
            thread_id: "t-1".to_string(),
            messages: vec![Message::user("hello"), Message::assistant("hi")],
            run: RunRecord {
                run_id: "r-1".to_string(),
                thread_id: "t-1".to_string(),
                agent_id: "agent-1".to_string(),
                parent_run_id: None,
                status: awaken_contract::contract::lifecycle::RunStatus::Running,
                termination_code: None,
                created_at: 100,
                updated_at: 100,
                steps: 0,
                input_tokens: 0,
                output_tokens: 0,
                state: None,
            },
        };

        let json = serde_json::to_vec(&envelope).unwrap();
        let decoded: CheckpointEnvelope = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded.thread_id, "t-1");
        assert_eq!(decoded.messages.len(), 2);
        assert_eq!(decoded.run.run_id, "r-1");
    }

    #[test]
    fn nats_buffered_writer_error_display() {
        let err = NatsBufferedWriterError::JetStream("timeout".to_string());
        assert_eq!(err.to_string(), "jetstream error: timeout");

        let err = NatsBufferedWriterError::Storage(StorageError::NotFound("missing".to_string()));
        assert_eq!(err.to_string(), "storage error: not found: missing");
    }

    // Integration tests require a running NATS server.

    #[tokio::test]
    #[ignore]
    async fn nats_buffered_writer_checkpoint_and_flush() {
        let client = async_nats::connect("localhost:4222").await.unwrap();
        let js = async_nats::jetstream::new(client);

        let inner = Arc::new(crate::memory::InMemoryStore::new());
        let writer = NatsBufferedWriter::new(inner.clone(), js).await.unwrap();

        let messages = vec![Message::user("hello"), Message::assistant("world")];
        let run = RunRecord {
            run_id: "run-1".to_string(),
            thread_id: "t-1".to_string(),
            agent_id: "agent-1".to_string(),
            parent_run_id: None,
            status: awaken_contract::contract::lifecycle::RunStatus::Running,
            termination_code: None,
            created_at: 100,
            updated_at: 100,
            steps: 1,
            input_tokens: 10,
            output_tokens: 20,
            state: None,
        };

        // Checkpoint should go to NATS, not inner store
        writer.checkpoint("t-1", &messages, &run).await.unwrap();
        let loaded = ThreadStore::load_messages(&*inner, "t-1").await.unwrap();
        assert!(loaded.is_none(), "inner store should not have data yet");

        // Flush should persist to inner store
        let flushed = writer.flush("t-1").await.unwrap();
        assert!(flushed > 0);

        let loaded = ThreadStore::load_messages(&*inner, "t-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.len(), 2);

        let loaded_run = inner.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(loaded_run.thread_id, "t-1");
    }

    #[tokio::test]
    #[ignore]
    async fn nats_buffered_writer_recover() {
        let client = async_nats::connect("localhost:4222").await.unwrap();
        let js = async_nats::jetstream::new(client);

        let inner = Arc::new(crate::memory::InMemoryStore::new());
        let writer = NatsBufferedWriter::new(inner.clone(), js.clone())
            .await
            .unwrap();

        let messages = vec![Message::user("recover-me")];
        let run = RunRecord {
            run_id: "run-recover".to_string(),
            thread_id: "t-recover".to_string(),
            agent_id: "agent-1".to_string(),
            parent_run_id: None,
            status: awaken_contract::contract::lifecycle::RunStatus::Running,
            termination_code: None,
            created_at: 200,
            updated_at: 200,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        };

        writer
            .checkpoint("t-recover", &messages, &run)
            .await
            .unwrap();

        // Simulate crash: create a new writer and recover
        let writer2 = NatsBufferedWriter::new(inner.clone(), js).await.unwrap();
        let recovered = writer2.recover().await.unwrap();
        assert!(recovered > 0);

        let loaded = ThreadStore::load_messages(&*inner, "t-recover")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text(), "recover-me");
    }

    #[tokio::test]
    #[ignore]
    async fn nats_buffered_writer_delegates_reads() {
        let client = async_nats::connect("localhost:4222").await.unwrap();
        let js = async_nats::jetstream::new(client);

        let inner = Arc::new(crate::memory::InMemoryStore::new());

        // Pre-populate inner store
        let run = RunRecord {
            run_id: "pre-run".to_string(),
            thread_id: "t-pre".to_string(),
            agent_id: "agent-1".to_string(),
            parent_run_id: None,
            status: awaken_contract::contract::lifecycle::RunStatus::Done,
            termination_code: None,
            created_at: 50,
            updated_at: 50,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        };
        inner
            .checkpoint("t-pre", &[Message::user("pre")], &run)
            .await
            .unwrap();

        let writer = NatsBufferedWriter::new(inner, js).await.unwrap();

        // Reads should go through to inner
        let msgs = writer.load_messages("t-pre").await.unwrap().unwrap();
        assert_eq!(msgs.len(), 1);

        let loaded_run = writer.load_run("pre-run").await.unwrap().unwrap();
        assert_eq!(loaded_run.thread_id, "t-pre");

        let latest = writer.latest_run("t-pre").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "pre-run");

        // Nonexistent
        assert!(writer.load_messages("no-such").await.unwrap().is_none());
        assert!(writer.load_run("no-such").await.unwrap().is_none());
        assert!(writer.latest_run("no-such").await.unwrap().is_none());
    }
}
