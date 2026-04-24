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
mod hierarchy_claim;
mod hot_meta;
mod keys;
mod metrics;
mod reader;
mod recovery;
mod wal_state;
mod writer;

pub use config::{NatsBufferedThreadConfig, ReadConsistency};

use std::sync::Arc;

use async_nats::jetstream::{consumer, kv, stream};
use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    ChildThreadDeleteStrategy, MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, RunStore,
    StorageError, ThreadPage, ThreadQuery, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::{Thread, ThreadMetadata};

#[derive(Debug, Clone, Default)]
struct HierarchyMutationTestHooks {
    after_inner_delete_pause: Arc<tokio::sync::Mutex<Option<DeletePausePoint>>>,
    before_inner_save_validated_pause: Arc<tokio::sync::Mutex<Option<SavePausePoint>>>,
}

#[derive(Debug, Clone)]
struct DeletePausePoint {
    thread_id: String,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Clone)]
struct SavePausePoint {
    thread_id: String,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl HierarchyMutationTestHooks {
    async fn set_pause_after_inner_delete(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.after_inner_delete_pause.lock().await = Some(DeletePausePoint {
            thread_id: thread_id.to_string(),
            reached,
            release,
        });
    }

    async fn pause_after_inner_delete_if_configured(&self, thread_id: &str) {
        let pause = {
            let mut slot = self.after_inner_delete_pause.lock().await;
            match slot.as_ref() {
                Some(pause) if pause.thread_id == thread_id => slot.take(),
                _ => None,
            }
        };
        let Some(pause) = pause else {
            return;
        };
        pause.reached.notify_waiters();
        pause.release.notified().await;
    }

    async fn set_pause_before_inner_save_validated(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.before_inner_save_validated_pause.lock().await = Some(SavePausePoint {
            thread_id: thread_id.to_string(),
            reached,
            release,
        });
    }

    async fn pause_before_inner_save_validated_if_configured(&self, thread_id: &str) {
        let pause = {
            let mut slot = self.before_inner_save_validated_pause.lock().await;
            match slot.as_ref() {
                Some(pause) if pause.thread_id == thread_id => slot.take(),
                _ => None,
            }
        };
        let Some(pause) = pause else {
            return;
        };
        pause.reached.notify_waiters();
        pause.release.notified().await;
    }
}

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
    pub(crate) hierarchy_write_lock: tokio::sync::Mutex<()>,
    pub(crate) hierarchy_claim_options: hierarchy_claim::ClaimOptions,
    pub(crate) writer_test_hooks: writer::WriterTestHooks,
    pub(crate) flush_claim_options: hierarchy_claim::ClaimOptions,
    pub(crate) flusher_test_hooks: flusher::FlusherTestHooks,
    hierarchy_mutation_test_hooks: HierarchyMutationTestHooks,
    pub(crate) flush_notify: Arc<tokio::sync::Notify>,
    pub(crate) shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub(crate) flusher_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<T: ThreadRunStore + Send + Sync + 'static> NatsBufferedThreadStore<T> {
    pub async fn connect(
        inner: Arc<T>,
        config: config::NatsBufferedThreadConfig,
    ) -> Result<Self, StorageError> {
        let client =
            crate::nats_connect::connect(&config.url, config.credentials.as_deref()).await?;
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

        let flush_notify = Arc::new(tokio::sync::Notify::new());
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let flush_claim_options = hierarchy_claim::ClaimOptions::default();
        let flusher_test_hooks = flusher::FlusherTestHooks::default();

        let shutdown_rx = shutdown_tx.subscribe();
        let flusher_handle = flusher::spawn_flusher(flusher::FlusherLoop {
            inner: Arc::clone(&inner),
            consumer: consumer.clone(),
            kv_hot: kv_hot.clone(),
            config: config.clone(),
            claim_options: flush_claim_options.clone(),
            test_hooks: flusher_test_hooks.clone(),
            flush_notify: Arc::clone(&flush_notify),
            shutdown_rx,
        });

        Ok(Self {
            inner,
            client,
            jetstream,
            stream,
            kv_hot,
            consumer,
            config,
            hierarchy_write_lock: tokio::sync::Mutex::new(()),
            hierarchy_claim_options: hierarchy_claim::ClaimOptions::default(),
            writer_test_hooks: writer::WriterTestHooks::default(),
            flush_claim_options,
            flusher_test_hooks,
            hierarchy_mutation_test_hooks: HierarchyMutationTestHooks::default(),
            flush_notify,
            shutdown_tx,
            flusher_handle: tokio::sync::Mutex::new(Some(flusher_handle)),
        })
    }

    pub async fn shutdown(&self) -> Result<(), StorageError> {
        let flush_result = self.force_flush_all_pending().await;
        let _ = self.shutdown_tx.send(true);
        self.flush_notify.notify_waiters();
        if let Some(handle) = self.flusher_handle.lock().await.take() {
            if flush_result.is_ok() {
                handle
                    .await
                    .map_err(|e| StorageError::Io(format!("flusher task join: {e}")))?;
            } else {
                handle.abort();
            }
        }
        flush_result
    }

    pub async fn force_flush_all_pending(&self) -> Result<(), StorageError> {
        recovery::reconcile_all_thread_tails(self).await?;
        for thread_id in hot_meta::pending_thread_ids(&self.kv_hot).await? {
            self.force_flush(&thread_id).await?;
        }
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
            projected_thread: None,
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
        wal_state::put_committed_state(&self.kv_hot, thread_id, thread_seq, ack.sequence, 0)
            .await?;
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
    pub async fn __test_publish_raw_wal(
        &self,
        thread_id: &str,
        payload: &[u8],
    ) -> Result<u64, StorageError> {
        let ack = self
            .jetstream
            .publish(keys::thread_subject(thread_id), payload.to_vec().into())
            .await
            .map_err(|e| StorageError::Io(format!("publish raw WAL: {e}")))?
            .await
            .map_err(|e| StorageError::Io(format!("publish raw WAL ack: {e}")))?;
        Ok(ack.sequence)
    }

    #[doc(hidden)]
    pub async fn __test_list_poison_wal_records(
        &self,
    ) -> Result<Vec<(String, serde_json::Value)>, StorageError> {
        use futures::StreamExt;

        let mut key_stream = self
            .kv_hot
            .keys()
            .await
            .map_err(|error| StorageError::Io(format!("list poison WAL keys: {error}")))?;
        let mut records = Vec::new();
        while let Some(key_result) = key_stream.next().await {
            let key = key_result
                .map_err(|error| StorageError::Io(format!("poison WAL key stream: {error}")))?;
            if !key.starts_with(keys::poison_wal_prefix()) {
                continue;
            }
            let entry = match self.kv_hot.entry(&key).await {
                Ok(Some(entry)) => entry,
                Ok(None) => continue,
                Err(error) => {
                    return Err(StorageError::Io(format!(
                        "load poison WAL entry {key}: {error}"
                    )));
                }
            };
            if matches!(
                entry.operation,
                async_nats::jetstream::kv::Operation::Delete
                    | async_nats::jetstream::kv::Operation::Purge
            ) {
                continue;
            }
            let value = serde_json::from_slice(&entry.value).map_err(|error| {
                StorageError::Serialization(format!(
                    "decode poison WAL entry {key} from kv bucket: {error}"
                ))
            })?;
            records.push((key, value));
        }
        records.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(records)
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

    #[doc(hidden)]
    pub async fn __test_read_wal_js_seq(
        &self,
        thread_id: &str,
        thread_seq: u64,
    ) -> Result<Option<u64>, StorageError> {
        Ok(wal_state::load_state(&self.kv_hot, thread_id, thread_seq)
            .await?
            .and_then(|state| state.js_seq))
    }

    #[doc(hidden)]
    pub async fn __test_read_wal_state(
        &self,
        thread_id: &str,
        thread_seq: u64,
    ) -> Result<Option<(String, Option<u64>)>, StorageError> {
        Ok(wal_state::load_state(&self.kv_hot, thread_id, thread_seq)
            .await?
            .map(|state| {
                let status = match state.status {
                    wal_state::WalEntryStatus::Prepared => "prepared",
                    wal_state::WalEntryStatus::Committed => "committed",
                    wal_state::WalEntryStatus::Aborted => "aborted",
                };
                (status.to_string(), state.js_seq)
            }))
    }

    #[doc(hidden)]
    pub async fn __test_force_flushed_seq(
        &self,
        thread_id: &str,
        seq: u64,
    ) -> Result<(), StorageError> {
        hot_meta::write_flushed_seq(&self.kv_hot, thread_id, seq).await
    }

    #[doc(hidden)]
    pub fn __test_set_hierarchy_claim_timing(&self, lease_ms: u64, renew_interval_ms: Option<u64>) {
        self.hierarchy_claim_options
            .set_for_tests(lease_ms, renew_interval_ms);
    }

    #[doc(hidden)]
    pub fn __test_set_flush_claim_timing(&self, lease_ms: u64, renew_interval_ms: Option<u64>) {
        self.flush_claim_options
            .set_for_tests(lease_ms, renew_interval_ms);
    }

    #[doc(hidden)]
    pub async fn __test_pause_checkpoint_after_wal_publish(
        &self,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        self.writer_test_hooks
            .set_post_publish_pause(reached, release)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_pause_checkpoint_after_post_publish_claim_check(
        &self,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        self.writer_test_hooks
            .set_post_publish_claim_check_pause(reached, release)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_pause_checkpoint_before_wal_publish(
        &self,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        self.writer_test_hooks
            .set_pre_publish_pause(reached, release)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_fail_checkpoint_after_mark_committed(&self, message: impl Into<String>) {
        self.writer_test_hooks
            .set_fail_after_mark_committed(message)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_pause_flusher_after_read_flushed_seq(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        self.flusher_test_hooks
            .set_pause_after_read_flushed(thread_id, reached, release)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_pause_flusher_after_claim_check(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        self.flusher_test_hooks
            .set_pause_after_claim_check(thread_id, reached, release)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_pause_delete_after_inner_delete(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        self.hierarchy_mutation_test_hooks
            .set_pause_after_inner_delete(thread_id, reached, release)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_pause_save_thread_validated_before_inner_save(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        self.hierarchy_mutation_test_hooks
            .set_pause_before_inner_save_validated(thread_id, reached, release)
            .await;
    }

    #[doc(hidden)]
    pub async fn __test_flush_committed_thread_seqs(
        &self,
        thread_id: &str,
        thread_seqs: &[u64],
    ) -> Result<(), StorageError> {
        let mut entries = Vec::with_capacity(thread_seqs.len());
        for &thread_seq in thread_seqs {
            let state = wal_state::load_state(&self.kv_hot, thread_id, thread_seq)
                .await?
                .ok_or_else(|| {
                    StorageError::NotFound(format!(
                        "WAL state missing for thread={thread_id}, seq={thread_seq}"
                    ))
                })?;
            if state.status != wal_state::WalEntryStatus::Committed {
                return Err(StorageError::Validation(format!(
                    "WAL state not committed for thread={thread_id}, seq={thread_seq}"
                )));
            }
            let js_seq = state.js_seq.ok_or_else(|| {
                StorageError::NotFound(format!(
                    "WAL js_seq missing for thread={thread_id}, seq={thread_seq}"
                ))
            })?;
            let raw = self.stream.get_raw_message(js_seq).await.map_err(|error| {
                StorageError::Io(format!(
                    "load raw WAL entry for thread={thread_id}, seq={thread_seq}: {error}"
                ))
            })?;
            entries.push((entry::decode(&raw.payload)?, js_seq));
        }
        flusher::flush_test_entries(
            &self.inner,
            &self.kv_hot,
            &self.flush_claim_options,
            &self.flusher_test_hooks,
            thread_id,
            entries,
        )
        .await
    }

    #[doc(hidden)]
    pub async fn __test_process_wal_stream_seqs(
        &self,
        thread_id: &str,
        stream_seqs: &[u64],
    ) -> Result<(), StorageError> {
        let mut entries = Vec::with_capacity(stream_seqs.len());
        for &stream_seq in stream_seqs {
            let raw = self.stream.get_raw_message(stream_seq).await.map_err(|error| {
                StorageError::Io(format!(
                    "load raw WAL entry for thread={thread_id}, stream_seq={stream_seq}: {error}"
                ))
            })?;
            let checkpoint = entry::decode(&raw.payload)?;
            if checkpoint.thread_id != thread_id {
                return Err(StorageError::Validation(format!(
                    "WAL stream_seq {stream_seq} belongs to thread={}, not {thread_id}",
                    checkpoint.thread_id
                )));
            }
            entries.push((checkpoint, stream_seq));
        }
        flusher::process_test_entries(
            &self.inner,
            &self.kv_hot,
            &self.flush_claim_options,
            &self.flusher_test_hooks,
            thread_id,
            entries,
        )
        .await
    }

    /// Block until the flusher has drained all pending entries for the given thread.
    pub async fn force_flush(&self, thread_id: &str) -> Result<(), StorageError> {
        recovery::reconcile_thread_tail(self, thread_id).await?;
        let target = hot_meta::read_latest_seq(&self.kv_hot, thread_id).await?;
        if target == 0 {
            return Ok(());
        }
        let timeout = std::time::Duration::from_secs(10);
        let start = std::time::Instant::now();
        loop {
            self.flush_notify.notify_waiters();
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

    async fn clear_hot_thread_state(&self, thread_id: &str) -> Result<(), StorageError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let watermark = hot_meta::write_delete_tombstone(&self.kv_hot, thread_id, now).await?;

        if watermark == 0 {
            for key in [
                keys::hot_meta_key(thread_id),
                keys::flushed_seq_key(thread_id),
            ] {
                if self
                    .kv_hot
                    .entry(&key)
                    .await
                    .map_err(|error| StorageError::Io(format!("kv entry {key}: {error}")))?
                    .is_none()
                {
                    continue;
                }
                self.kv_hot
                    .delete(&key)
                    .await
                    .map_err(|error| StorageError::Io(format!("kv delete {key}: {error}")))?;
            }
        }

        for state in wal_state::list_thread_states(&self.kv_hot, thread_id).await? {
            wal_state::delete_state(&self.kv_hot, &state.thread_id, state.thread_seq).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl<T: ThreadRunStore + Send + Sync + 'static> ThreadStore for NatsBufferedThreadStore<T> {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        reader::load_thread(self, thread_id).await
    }
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.force_flush(&thread.id).await?;
        self.inner.save_thread(thread).await
    }
    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError> {
        let _guard = self.hierarchy_write_lock.lock().await;
        self.force_flush_all_pending().await?;
        let claim = hierarchy_claim::acquire(&self.kv_hot, &self.hierarchy_claim_options).await?;
        let result = async {
            self.force_flush_all_pending().await?;
            claim.ensure_current(&self.kv_hot).await?;
            self.hierarchy_mutation_test_hooks
                .pause_before_inner_save_validated_if_configured(&thread.id)
                .await;
            claim.ensure_current(&self.kv_hot).await?;
            self.inner.save_thread_validated(thread).await?;
            claim.ensure_current(&self.kv_hot).await?;
            Ok(())
        }
        .await;
        let release_result = hierarchy_claim::release(&self.kv_hot, claim).await;
        match result {
            Ok(()) => {
                release_result?;
                Ok(())
            }
            Err(error) => {
                if let Err(release_error) = release_result {
                    tracing::warn!(
                        operation = "save_thread_validated",
                        error = %release_error,
                        "failed to release distributed hierarchy claim after operation error"
                    );
                }
                Err(error)
            }
        }
    }
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        let _guard = self.hierarchy_write_lock.lock().await;
        self.force_flush_all_pending().await?;
        let claim = hierarchy_claim::acquire(&self.kv_hot, &self.hierarchy_claim_options).await?;
        let result = async {
            self.force_flush_all_pending().await?;
            claim.ensure_current(&self.kv_hot).await?;
            self.inner.delete_thread(thread_id).await?;
            self.hierarchy_mutation_test_hooks
                .pause_after_inner_delete_if_configured(thread_id)
                .await;
            claim.ensure_current(&self.kv_hot).await?;
            self.clear_hot_thread_state(thread_id).await?;
            claim.ensure_current(&self.kv_hot).await?;
            Ok(())
        }
        .await;
        let release_result = hierarchy_claim::release(&self.kv_hot, claim).await;
        match result {
            Ok(()) => {
                release_result?;
                Ok(())
            }
            Err(error) => {
                if let Err(release_error) = release_result {
                    tracing::warn!(
                        operation = "delete_thread",
                        error = %release_error,
                        "failed to release distributed hierarchy claim after operation error"
                    );
                }
                Err(error)
            }
        }
    }
    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        let _guard = self.hierarchy_write_lock.lock().await;
        self.force_flush_all_pending().await?;
        let claim = hierarchy_claim::acquire(&self.kv_hot, &self.hierarchy_claim_options).await?;
        let result = async {
            self.force_flush_all_pending().await?;
            claim.ensure_current(&self.kv_hot).await?;
            self.inner
                .delete_thread_with_strategy(thread_id, strategy)
                .await?;
            self.hierarchy_mutation_test_hooks
                .pause_after_inner_delete_if_configured(thread_id)
                .await;
            claim.ensure_current(&self.kv_hot).await?;
            self.clear_hot_thread_state(thread_id).await?;
            claim.ensure_current(&self.kv_hot).await?;
            Ok(())
        }
        .await;
        let release_result = hierarchy_claim::release(&self.kv_hot, claim).await;
        match result {
            Ok(()) => {
                release_result?;
                Ok(())
            }
            Err(error) => {
                if let Err(release_error) = release_result {
                    tracing::warn!(
                        operation = "delete_thread_with_strategy",
                        error = %release_error,
                        "failed to release distributed hierarchy claim after operation error"
                    );
                }
                Err(error)
            }
        }
    }
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        reader::list_threads(self, offset, limit).await
    }
    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError> {
        reader::list_threads_query(self, query).await
    }
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        reader::load_messages(self, thread_id).await
    }
    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(MessagePage::empty());
        };
        Ok(awaken_contract::contract::storage::paginate_message_records(records, query))
    }
    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.force_flush(thread_id).await?;
        self.inner.save_messages(thread_id, messages).await
    }
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.force_flush(thread_id).await?;
        self.inner.delete_messages(thread_id).await
    }
    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.force_flush(id).await?;
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
        reader::list_runs(self, query).await
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
        let _guard = self.hierarchy_write_lock.lock().await;
        let claim = hierarchy_claim::acquire(&self.kv_hot, &self.hierarchy_claim_options).await?;
        let result = writer::checkpoint(self, &claim, thread_id, messages, run).await;
        let release_result = hierarchy_claim::release(&self.kv_hot, claim).await;

        match result {
            Ok(()) => {
                release_result?;
                Ok(())
            }
            Err(error) => {
                if let Err(release_error) = release_result {
                    tracing::warn!(
                        operation = "checkpoint",
                        error = %release_error,
                        "failed to release distributed hierarchy claim after operation error"
                    );
                }
                Err(error)
            }
        }
    }
}
