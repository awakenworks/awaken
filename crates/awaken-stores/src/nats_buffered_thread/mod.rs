//! NATS-buffered `ThreadRunStore` decorator.
//!
//! Buffers `checkpoint()` writes in a JetStream WAL + KV hot state, with a
//! background flusher that coalesces per-thread writes into the inner store.
//! Reads serve read-your-writes consistency via a WAL overlay (DB when caught
//! up, last WAL entry otherwise).
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use awaken_stores::{InMemoryStore, NatsBufferedThreadConfig, NatsBufferedThreadStore};
//!
//! # async fn wire() -> Result<(), Box<dyn std::error::Error>> {
//! let inner = Arc::new(InMemoryStore::new());
//! let config = NatsBufferedThreadConfig::new("nats://localhost:4222");
//! let buffered = NatsBufferedThreadStore::connect(inner, config).await?;
//! // Use `buffered` wherever a `ThreadRunStore` is expected.
//! # buffered.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod config;
mod entry;
mod flusher;
mod hot_meta;
mod keys;
mod reader;
mod recovery;
mod writer;

pub use config::{NatsBufferedThreadConfig, ReadConsistency};

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::{Thread, ThreadMetadata};

use async_nats::jetstream::{consumer, kv, stream};

pub struct NatsBufferedThreadStore<T: ThreadRunStore + Send + Sync + 'static> {
    pub(crate) inner: Arc<T>,
    #[allow(dead_code)]
    pub(crate) client: async_nats::Client,
    pub(crate) jetstream: async_nats::jetstream::Context,
    pub(crate) stream: async_nats::jetstream::stream::Stream,
    pub(crate) kv_hot: async_nats::jetstream::kv::Store,
    #[allow(dead_code)]
    pub(crate) consumer: async_nats::jetstream::consumer::PullConsumer,
    pub(crate) config: config::NatsBufferedThreadConfig,
    pub(crate) shutdown_tx: tokio::sync::watch::Sender<bool>,
    #[allow(dead_code)]
    pub(crate) flusher_handle: tokio::task::JoinHandle<()>,
}

impl<T: ThreadRunStore + Send + Sync + 'static> NatsBufferedThreadStore<T> {
    pub async fn connect(
        inner: Arc<T>,
        config: config::NatsBufferedThreadConfig,
    ) -> Result<Self, StorageError> {
        let client = async_nats::connect(&config.url)
            .await
            .map_err(|e| StorageError::Io(format!("connect: {e}")))?;
        let jetstream = async_nats::jetstream::new(client.clone());

        let stream_config = stream::Config {
            name: config.stream_name.clone(),
            subjects: vec!["thread.>".to_string()],
            retention: stream::RetentionPolicy::Limits,
            max_age: config.max_age,
            storage: stream::StorageType::File,
            ..Default::default()
        };
        let stream = jetstream
            .get_or_create_stream(stream_config)
            .await
            .map_err(|e| StorageError::Io(format!("create stream: {e}")))?;

        let consumer_config = consumer::pull::Config {
            durable_name: Some(config.consumer_name.clone()),
            filter_subject: "thread.>".to_string(),
            ack_policy: consumer::AckPolicy::Explicit,
            ack_wait: config.ack_wait,
            ..Default::default()
        };
        let consumer = stream
            .get_or_create_consumer(&config.consumer_name, consumer_config)
            .await
            .map_err(|e| StorageError::Io(format!("create consumer: {e}")))?;

        let kv_hot = match jetstream.get_key_value(&config.hot_bucket).await {
            Ok(s) => s,
            Err(_) => jetstream
                .create_key_value(kv::Config {
                    bucket: config.hot_bucket.clone(),
                    history: 1,
                    ..Default::default()
                })
                .await
                .map_err(|e| StorageError::Io(format!("create bucket: {e}")))?,
        };

        let (shutdown_tx, _) = tokio::sync::watch::channel(false);

        let shutdown_rx = shutdown_tx.subscribe();
        let flusher_handle = flusher::spawn_flusher(
            Arc::clone(&inner),
            consumer.clone(),
            kv_hot.clone(),
            config.clone(),
            shutdown_rx,
        );

        Ok(Self {
            inner,
            client,
            jetstream,
            stream,
            kv_hot,
            consumer,
            config,
            shutdown_tx,
            flusher_handle,
        })
    }

    pub async fn shutdown(&self) -> Result<(), StorageError> {
        let _ = self.shutdown_tx.send(true);
        Ok(())
    }

    /// Test-only: publish a `CheckpointEntry` to the WAL with a
    /// caller-chosen `thread_seq`, returning the JetStream stream
    /// sequence assigned to the entry. Used to reproduce the
    /// concurrent-writer race where JS arrival order diverges from
    /// reservation order.
    #[doc(hidden)]
    pub async fn __test_plant_wal_entry(
        &self,
        thread_id: &str,
        run: &RunRecord,
        messages: &[Message],
        thread_seq: u64,
    ) -> Result<u64, StorageError> {
        let wal_entry = entry::CheckpointEntry {
            thread_id: thread_id.to_string(),
            run: run.clone(),
            messages: messages.to_vec(),
            thread_seq,
            written_at: 0,
        };
        let payload = entry::encode(&wal_entry)?;
        let ack = self
            .jetstream
            .publish(keys::thread_subject(thread_id), payload)
            .await
            .map_err(|e| StorageError::Io(format!("publish: {e}")))?
            .await
            .map_err(|e| StorageError::Io(format!("publish ack: {e}")))?;
        Ok(ack.sequence)
    }

    /// Test-only: force `ThreadHotMetadata` to specific values, skipping
    /// the CAS-promote guard. Used together with `__test_plant_wal_entry`
    /// to simulate a committed seq/JS-seq pair without running the
    /// writer path.
    #[doc(hidden)]
    pub async fn __test_force_hot_meta(
        &self,
        thread_id: &str,
        reserved_seq: u64,
        latest_seq: u64,
        latest_js_seq: u64,
    ) -> Result<(), StorageError> {
        let meta = hot_meta::ThreadHotMetadata {
            reserved_seq,
            latest_seq,
            latest_js_seq,
            updated_at: 0,
        };
        let bytes = hot_meta::encode_meta(&meta)?;
        self.kv_hot
            .put(keys::hot_meta_key(thread_id), bytes)
            .await
            .map_err(|e| StorageError::Io(format!("kv put: {e}")))?;
        Ok(())
    }

    #[doc(hidden)]
    pub async fn __test_cache_run_if_newer(
        &self,
        run: &RunRecord,
        thread_seq: u64,
    ) -> Result<(), StorageError> {
        hot_meta::cache_run_if_newer(&self.kv_hot, run, thread_seq).await
    }

    #[doc(hidden)]
    pub async fn __test_read_flushed_seq(&self, thread_id: &str) -> Result<u64, StorageError> {
        hot_meta::read_flushed_seq(&self.kv_hot, thread_id).await
    }

    /// Block until the flusher has drained all pending entries for the given thread.
    pub async fn force_flush(&self, thread_id: &str) -> Result<(), StorageError> {
        let target = hot_meta::read_latest_seq(&self.kv_hot, thread_id).await?;
        if target == 0 {
            return Ok(());
        }
        let timeout = std::time::Duration::from_secs(10);
        let start = std::time::Instant::now();
        loop {
            let flushed = hot_meta::read_flushed_seq(&self.kv_hot, thread_id).await?;
            if flushed >= target {
                return Ok(());
            }
            if start.elapsed() >= timeout {
                return Err(StorageError::Io(format!(
                    "force_flush timeout (thread={thread_id}, target={target}, flushed={flushed})"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

#[async_trait]
impl<T: ThreadRunStore + Send + Sync + 'static> ThreadStore for NatsBufferedThreadStore<T> {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        self.inner.load_thread(thread_id).await
    }
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(thread).await
    }
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(thread_id).await
    }
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        self.inner.list_threads(offset, limit).await
    }
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        reader::load_messages(self, thread_id).await
    }
    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.inner.save_messages(thread_id, messages).await
    }
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(thread_id).await
    }
    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.inner.update_thread_metadata(id, metadata).await
    }
}

#[async_trait]
impl<T: ThreadRunStore + Send + Sync + 'static> RunStore for NatsBufferedThreadStore<T> {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.inner.create_run(record).await
    }
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        reader::load_run(self, run_id).await
    }
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        reader::latest_run(self, thread_id).await
    }
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        self.inner.list_runs(query).await
    }
}

#[async_trait]
impl<T: ThreadRunStore + Send + Sync + 'static> ThreadRunStore for NatsBufferedThreadStore<T> {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        writer::checkpoint(self, thread_id, messages, run).await
    }
}
