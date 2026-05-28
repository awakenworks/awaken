//! NATS JetStream + KV implementation of `MailboxStore`.
//!
//! Dispatch records live in the `dispatch-state` KV bucket (source of truth)
//! while JetStream `DISPATCH` serves as the delivery signal. An in-memory
//! index populated by `kv.watch_all()` answers list queries; point reads use
//! the authoritative KV record so control paths are not gated on watcher lag.
//!
//! # Example
//!
//! ```no_run
//! use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};
//!
//! # async fn wire() -> Result<(), Box<dyn std::error::Error>> {
//! let config = NatsMailboxConfig::new("nats://localhost:4222");
//! let store = NatsMailboxStore::connect(config).await?;
//! // Use `store` wherever a `MailboxStore` is expected.
//! # store.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod claim_guard;
mod codec;
mod config;
mod index;
mod keys;
mod kv_helpers;
mod metrics;
mod ops_claim;
mod ops_interrupt;
mod ops_maintenance;
mod ops_query;
mod ops_write;
mod sweeper;

pub use config::NatsMailboxConfig;

use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::contract::mailbox::{
    DispatchSignalEntry, DispatchSignalReceipt, LiveCommandReceipt, LiveDeliveryOutcome,
    LiveRunCommand, LiveRunCommandEntry, LiveRunCommandStream, LiveRunTarget, MailboxInterrupt,
    MailboxInterruptDetails, MailboxStore, RunDispatch, RunDispatchResult, RunDispatchStatus,
};
use awaken_server_contract::contract::storage::StorageError;
use bytes::Bytes;
use futures::StreamExt;

use async_nats::jetstream::{consumer, kv, stream};

use index::DispatchIndex;

#[doc(hidden)]
pub fn __test_encode_dispatch(dispatch: &RunDispatch) -> Vec<u8> {
    codec::encode(dispatch).unwrap().to_vec()
}

pub struct NatsMailboxStore {
    pub(crate) client: async_nats::Client,
    pub(crate) jetstream: async_nats::jetstream::Context,
    pub(crate) kv_dispatch: async_nats::jetstream::kv::Store,
    pub(crate) kv_epoch: async_nats::jetstream::kv::Store,
    pub(crate) kv_thread_index: async_nats::jetstream::kv::Store,
    pub(crate) consumer: async_nats::jetstream::consumer::PullConsumer,
    pub(crate) authoritative_scan_timeout: std::time::Duration,
    pub(crate) live_request_timeout: std::time::Duration,
    pub(crate) index: Arc<tokio::sync::RwLock<DispatchIndex>>,
    pub(crate) shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub(crate) _watcher: tokio::task::JoinHandle<()>,
    pub(crate) _sweeper: tokio::task::JoinHandle<()>,
}

impl NatsMailboxStore {
    /// Connect to NATS, create/verify stream + buckets + consumer.
    pub async fn connect(config: NatsMailboxConfig) -> Result<Self, StorageError> {
        let client =
            crate::nats_connect::connect(&config.url, config.credentials.as_deref()).await?;
        let jetstream = async_nats::jetstream::new(client.clone());

        // Stream for dispatch delivery signals.
        let stream_config = stream::Config {
            name: config.stream_name.clone(),
            subjects: vec!["dispatch.>".to_string()],
            retention: stream::RetentionPolicy::WorkQueue,
            duplicate_window: config.dedup_window,
            ..Default::default()
        };
        let stream = jetstream
            .get_or_create_stream(stream_config)
            .await
            .map_err(|e| StorageError::Io(format!("create stream: {e}")))?;

        // Durable pull consumer.
        let consumer_config = consumer::pull::Config {
            durable_name: Some(config.consumer_name.clone()),
            filter_subject: "dispatch.>".to_string(),
            ack_policy: consumer::AckPolicy::Explicit,
            ..Default::default()
        };
        let consumer = stream
            .get_or_create_consumer(&config.consumer_name, consumer_config)
            .await
            .map_err(|e| StorageError::Io(format!("create consumer: {e}")))?;

        let kv_dispatch = Self::get_or_create_bucket(&jetstream, &config.dispatch_bucket).await?;
        let kv_epoch = Self::get_or_create_bucket(&jetstream, &config.epoch_bucket).await?;
        let kv_thread_index =
            Self::get_or_create_bucket(&jetstream, &config.thread_index_bucket).await?;
        let watcher_initial_scan_timeout = config.watcher_initial_scan_timeout;
        let sweeper_republish_after = config.sweeper_republish_after;
        let authoritative_scan_timeout = config.authoritative_scan_timeout;
        let live_request_timeout = config.nats_request_timeout;

        let index = Arc::new(tokio::sync::RwLock::new(DispatchIndex::default()));
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);

        // Spawn KV watcher to keep index in sync.
        let watcher_shutdown_rx = shutdown_tx.subscribe();
        let (watcher_ready_tx, watcher_ready_rx) = tokio::sync::oneshot::channel();
        let watcher_handle = index::spawn_watcher(
            kv_dispatch.clone(),
            kv_thread_index.clone(),
            Arc::clone(&index),
            watcher_shutdown_rx,
            watcher_ready_tx,
        );
        match tokio::time::timeout(watcher_initial_scan_timeout, watcher_ready_rx).await {
            Err(_) => {
                watcher_handle.abort();
                return Err(StorageError::Io(format!(
                    "mailbox index initial scan timed out after {watcher_initial_scan_timeout:?}"
                )));
            }
            Ok(ready) => match ready {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Err(StorageError::Io(format!(
                        "mailbox index initial scan: {error}"
                    )));
                }
                Err(_) => {
                    return Err(StorageError::Io(
                        "mailbox index watcher exited before initial scan".to_string(),
                    ));
                }
            },
        }

        let sweeper_shutdown_rx = shutdown_tx.subscribe();
        let sweeper_handle = sweeper::spawn_sweeper(
            jetstream.clone(),
            Arc::clone(&index),
            sweeper_shutdown_rx,
            config.sweeper_interval,
            sweeper_republish_after,
        );

        Ok(Self {
            client,
            jetstream,
            kv_dispatch,
            kv_epoch,
            kv_thread_index,
            consumer,
            authoritative_scan_timeout,
            live_request_timeout,
            index,
            shutdown_tx,
            _watcher: watcher_handle,
            _sweeper: sweeper_handle,
        })
    }

    async fn get_or_create_bucket(
        jetstream: &async_nats::jetstream::Context,
        name: &str,
    ) -> Result<kv::Store, StorageError> {
        match jetstream.get_key_value(name).await {
            Ok(store) => Ok(store),
            Err(_) => jetstream
                .create_key_value(kv::Config {
                    bucket: name.to_string(),
                    history: 1,
                    ..Default::default()
                })
                .await
                .map_err(|e| StorageError::Io(format!("create bucket {name}: {e}"))),
        }
    }

    /// Signal background tasks to shut down.
    pub async fn shutdown(&self) -> Result<(), StorageError> {
        let _ = self.shutdown_tx.send(true);
        Ok(())
    }

    #[doc(hidden)]
    pub fn kv_dispatch(&self) -> &async_nats::jetstream::kv::Store {
        &self.kv_dispatch
    }

    #[doc(hidden)]
    pub async fn index_contains(&self, dispatch_id: &str) -> bool {
        self.index.read().await.get(dispatch_id).is_some()
    }

    #[doc(hidden)]
    pub async fn __test_remove_dispatch_from_index(&self, dispatch_id: &str) {
        self.index.write().await.remove(dispatch_id);
    }

    #[doc(hidden)]
    pub async fn __test_purge_thread_index(&self, thread_id: &str) -> Result<(), StorageError> {
        self.kv_thread_index
            .purge(&keys::thread_index_key(thread_id))
            .await
            .map(|_| ())
            .map_err(|e| StorageError::Io(format!("purge thread index: {e}")))
    }

    #[doc(hidden)]
    pub async fn __test_purge_thread_claim(&self, thread_id: &str) -> Result<(), StorageError> {
        self.kv_thread_index
            .purge(&keys::thread_claim_key(thread_id))
            .await
            .map(|_| ())
            .map_err(|e| StorageError::Io(format!("purge thread claim: {e}")))
    }

    #[doc(hidden)]
    pub async fn __test_purge_dedupe_lock(
        &self,
        thread_id: &str,
        dedupe_key: &str,
    ) -> Result<(), StorageError> {
        self.kv_thread_index
            .purge(&keys::dedupe_lock_key(thread_id, dedupe_key))
            .await
            .map(|_| ())
            .map_err(|e| StorageError::Io(format!("purge dedupe lock: {e}")))
    }

    #[doc(hidden)]
    pub async fn __test_delete_dispatch_record(
        &self,
        dispatch_id: &str,
    ) -> Result<(), StorageError> {
        self.kv_dispatch
            .delete(&keys::dispatch_key(dispatch_id))
            .await
            .map(|_| ())
            .map_err(|e| StorageError::Io(format!("delete dispatch record: {e}")))
    }

    /// Test-only: force a dedupe lock into the KV bucket without going
    /// through `enqueue`, so integration tests can reproduce the
    /// crash-between-create-and-put orphan scenario.
    #[doc(hidden)]
    pub async fn __test_force_dedupe_lock(
        &self,
        thread_id: &str,
        dedupe_key: &str,
        holder_dispatch_id: &str,
    ) -> Result<(), StorageError> {
        let key = keys::dedupe_lock_key(thread_id, dedupe_key);
        let value = codec::encode_dedupe_lock(&codec::DedupeLockRecord {
            dispatch_id: holder_dispatch_id.to_string(),
            created_at: 0,
        })?;
        self.kv_thread_index
            .create(&key, value)
            .await
            .map(|_| ())
            .map_err(|e| StorageError::Io(format!("force lock: {e}")))
    }

    /// Test-only: attempt to release a dedupe lock as a specific dispatch
    /// owner. Used to reproduce delayed-release races.
    #[doc(hidden)]
    pub async fn __test_release_dedupe_lock_as(
        &self,
        thread_id: &str,
        dedupe_key: &str,
        holder_dispatch_id: &str,
    ) {
        ops_write::release_dedupe_lock(self, thread_id, dedupe_key, holder_dispatch_id).await;
    }

    #[doc(hidden)]
    pub async fn __test_dedupe_lock_holder(
        &self,
        thread_id: &str,
        dedupe_key: &str,
    ) -> Result<Option<String>, StorageError> {
        let key = keys::dedupe_lock_key(thread_id, dedupe_key);
        let entry = self
            .kv_thread_index
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("dedupe lock entry: {e}")))?;
        entry
            .filter(|entry| !kv_helpers::is_tombstone(entry))
            .map(|entry| codec::decode_dedupe_lock(&entry.value).map(|lock| lock.dispatch_id))
            .transpose()
    }

    /// Test-only: plant a dispatch in the authoritative KV store WITHOUT
    /// publishing the JetStream delivery signal. Reproduces the
    /// partial-failure path where `enqueue` committed to KV but the JS
    /// publish later failed — the sweeper should detect the missing
    /// signal and re-publish.
    #[doc(hidden)]
    pub async fn __test_plant_dispatch_without_publish(
        &self,
        dispatch: &RunDispatch,
    ) -> Result<(), StorageError> {
        let mut stamped = dispatch.clone();
        stamped.dispatch_epoch = match self
            .kv_epoch
            .entry(&keys::epoch_key(&stamped.thread_id))
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?
        {
            Some(entry) if kv_helpers::is_tombstone(&entry) => 0,
            Some(entry) => codec::decode_epoch(&entry.value)?,
            None => 0,
        };
        ops_write::append_thread_index(self, &stamped.thread_id, &stamped.dispatch_id).await?;
        let bytes = codec::encode(&stamped)?;
        let revision = self
            .kv_dispatch
            .put(keys::dispatch_key(&stamped.dispatch_id), bytes)
            .await
            .map_err(|e| StorageError::Io(format!("kv put: {e}")))?;
        self.index
            .write()
            .await
            .upsert_with_revision(stamped, revision);
        Ok(())
    }

    /// Test-only: plant a dispatch exactly as supplied. This is used to
    /// reproduce cross-node races where local indexes observe stale epoch
    /// dispatches that authoritative enqueue stamping would no longer create.
    #[doc(hidden)]
    pub async fn __test_plant_dispatch_exact(
        &self,
        dispatch: &RunDispatch,
    ) -> Result<(), StorageError> {
        ops_write::append_thread_index(self, &dispatch.thread_id, &dispatch.dispatch_id).await?;
        let bytes = codec::encode(dispatch)?;
        let revision = self
            .kv_dispatch
            .put(keys::dispatch_key(&dispatch.dispatch_id), bytes)
            .await
            .map_err(|e| StorageError::Io(format!("kv put: {e}")))?;
        self.index
            .write()
            .await
            .upsert_with_revision(dispatch.clone(), revision);
        Ok(())
    }

    /// Test-only: overwrite the local in-memory index without changing KV.
    /// This reproduces watcher-lag races where local state is older than
    /// the authoritative dispatch record.
    #[doc(hidden)]
    pub async fn __test_upsert_index_only(&self, dispatch: &RunDispatch) {
        self.index.write().await.force_upsert(dispatch.clone());
    }
}

#[async_trait]
impl MailboxStore for NatsMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        ops_write::enqueue(self, dispatch).await
    }
    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        ops_claim::claim(self, thread_id, consumer_id, lease_ms, now, limit).await
    }
    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        ops_claim::claim_dispatch(self, dispatch_id, consumer_id, lease_ms, now).await
    }
    async fn ack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        ops_write::ack(self, dispatch_id, claim_token, now).await
    }
    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        ops_write::record_dispatch_start(self, dispatch_id, claim_token, dispatch_instance_id, now)
            .await
    }
    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError> {
        ops_write::record_run_result(self, dispatch_id, claim_token, result, now).await
    }
    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        ops_write::nack(self, dispatch_id, claim_token, retry_at, error, now).await
    }
    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        ops_write::dead_letter(self, dispatch_id, claim_token, error, now).await
    }
    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        ops_write::cancel(self, dispatch_id, now).await
    }
    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        ops_write::extend_lease(self, dispatch_id, claim_token, extension_ms, now).await
    }
    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        self.interrupt_detailed(thread_id, now)
            .await
            .map(Into::into)
    }
    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        ops_interrupt::interrupt(self, thread_id, now).await
    }
    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        ops_write::current_thread_epoch(self, thread_id).await
    }
    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        ops_write::supersede_claimed(self, dispatch_id, claim_token, now, reason).await
    }
    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
        ops_query::load_dispatch(self, dispatch_id).await
    }
    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        ops_query::list_dispatches(self, thread_id, status_filter, limit, offset).await
    }
    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        ops_query::count_dispatches_by_status(self, status).await
    }
    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        ops_query::list_terminal_dispatches(self, limit, offset).await
    }
    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        ops_maintenance::reclaim_expired_leases(self, now, limit).await
    }
    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
        ops_maintenance::purge_terminal(self, older_than).await
    }
    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
        ops_query::queued_thread_ids(self).await
    }

    fn supports_dispatch_signals(&self) -> bool {
        true
    }

    async fn pull_dispatch_signals(
        &self,
        max: usize,
        expires: std::time::Duration,
    ) -> Result<Vec<DispatchSignalEntry>, StorageError> {
        if max == 0 {
            return Ok(Vec::new());
        }

        let mut messages = self
            .consumer
            .fetch()
            .max_messages(max)
            .expires(expires)
            .messages()
            .await
            .map_err(|e| StorageError::Io(format!("dispatch signal fetch: {e}")))?;

        let mut entries = Vec::new();
        while let Some(next) = messages.next().await {
            let message =
                next.map_err(|e| StorageError::Io(format!("dispatch signal message: {e}")))?;
            let dispatch_id = match String::from_utf8(message.payload.to_vec()) {
                Ok(dispatch_id) if !dispatch_id.trim().is_empty() => dispatch_id,
                _ => {
                    let _ = message.ack().await;
                    continue;
                }
            };
            let Some(dispatch) = ops_query::load_dispatch(self, &dispatch_id).await? else {
                let _ = message.ack().await;
                continue;
            };
            if dispatch.status.is_terminal() {
                ops_write::cleanup_thread_index(self, &dispatch).await;
            } else {
                ops_write::append_thread_index(self, &dispatch.thread_id, &dispatch.dispatch_id)
                    .await?;
            }
            self.index.write().await.upsert(dispatch.clone());
            entries.push(DispatchSignalEntry {
                thread_id: dispatch.thread_id,
                dispatch_id,
                receipt: Box::new(NatsDispatchSignalReceipt { message }),
            });
        }
        Ok(entries)
    }

    // Live channel uses core NATS request-reply (not JetStream). `LiveRunCommand`
    // is ephemeral by contract: no persistence, no redelivery. Request-reply
    // gives the producer a subscriber-presence signal: `NoResponders` =
    // nobody listening and `TimedOut` = listener couldn't ack; both map to
    // `NoSubscriber` so callers fall back to durable dispatch. Other request
    // errors are surfaced because they do not prove the command was unobserved.

    async fn deliver_live(
        &self,
        thread_id: &str,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        deliver_live_subject(self, keys::live_subject(thread_id), cmd).await
    }

    async fn deliver_live_to(
        &self,
        target: &LiveRunTarget,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        deliver_live_subject(
            self,
            keys::live_target_subject(
                &target.thread_id,
                &target.run_id,
                target.dispatch_id.as_deref(),
            ),
            cmd,
        )
        .await
    }

    async fn open_live_channel(
        &self,
        thread_id: &str,
    ) -> Result<LiveRunCommandStream, StorageError> {
        open_live_subject(self, keys::live_subject(thread_id)).await
    }

    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, StorageError> {
        open_live_subject(
            self,
            keys::live_target_subject(
                &target.thread_id,
                &target.run_id,
                target.dispatch_id.as_deref(),
            ),
        )
        .await
    }
}

async fn deliver_live_subject(
    store: &NatsMailboxStore,
    subject: String,
    cmd: LiveRunCommand,
) -> Result<LiveDeliveryOutcome, StorageError> {
    let payload = serde_json::to_vec(&cmd)
        .map_err(|e| StorageError::Serialization(format!("LiveRunCommand encode: {e}")))?;
    let result = match tokio::time::timeout(
        store.live_request_timeout,
        store.client.request(subject, Bytes::from(payload)),
    )
    .await
    {
        Err(_) => Ok(LiveDeliveryOutcome::NoSubscriber),
        Ok(result) => match result {
            Ok(_) => Ok(LiveDeliveryOutcome::Delivered),
            Err(err) => match err.kind() {
                async_nats::client::RequestErrorKind::NoResponders
                | async_nats::client::RequestErrorKind::TimedOut => {
                    Ok(LiveDeliveryOutcome::NoSubscriber)
                }
                _ => Err(StorageError::Io(format!("nats request: {err}"))),
            },
        },
    };
    metrics::inc_live_delivery(&result);
    result
}

async fn open_live_subject(
    store: &NatsMailboxStore,
    subject: String,
) -> Result<LiveRunCommandStream, StorageError> {
    let subscriber = store
        .client
        .subscribe(subject)
        .await
        .map_err(|e| StorageError::Io(format!("nats subscribe: {e}")))?;
    let client = store.client.clone();
    let stream = subscriber.filter_map(move |msg| {
        let client = client.clone();
        async move {
            // Decode BEFORE handing the reply subject to the consumer.
            // Malformed payload → drop and leave the producer without
            // an ack so its request times out and falls back to durable
            // dispatch.
            let command = match serde_json::from_slice::<LiveRunCommand>(&msg.payload) {
                Ok(cmd) => cmd,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed LiveRunCommand payload");
                    return None;
                }
            };
            let receipt: Box<dyn LiveCommandReceipt> = Box::new(NatsReplyReceipt {
                client,
                reply: msg.reply.clone(),
            });
            Some(LiveRunCommandEntry { command, receipt })
        }
    });
    Ok(Box::pin(stream))
}

/// Receipt backed by a NATS reply subject. `ack` publishes an empty
/// payload to the subject the producer's `client.request()` is awaiting
/// on. Dropping the receipt without ack leaves the request to time out →
/// producer observes `NoSubscriber`.
struct NatsReplyReceipt {
    client: async_nats::Client,
    reply: Option<async_nats::Subject>,
}

impl LiveCommandReceipt for NatsReplyReceipt {
    fn ack(self: Box<Self>) {
        let Some(reply) = self.reply else {
            return;
        };
        let client = self.client;
        tokio::spawn(async move {
            if let Err(err) = client.publish(reply, Bytes::new()).await {
                tracing::warn!(error = %err, "live-channel ack publish failed");
            }
        });
    }
}

struct NatsDispatchSignalReceipt {
    message: async_nats::jetstream::Message,
}

#[async_trait]
impl DispatchSignalReceipt for NatsDispatchSignalReceipt {
    fn redelivery_attempts(&self) -> Option<u64> {
        self.message
            .info()
            .ok()
            .and_then(|info| u64::try_from(info.delivered).ok())
    }

    async fn ack(self: Box<Self>) -> Result<(), StorageError> {
        self.message
            .ack()
            .await
            .map_err(|e| StorageError::Io(format!("dispatch signal ack: {e}")))
    }

    async fn nack(self: Box<Self>) -> Result<(), StorageError> {
        self.message
            .ack_with(async_nats::jetstream::AckKind::Nak(None))
            .await
            .map_err(|e| StorageError::Io(format!("dispatch signal nack: {e}")))
    }

    async fn nack_with_delay(
        self: Box<Self>,
        delay: std::time::Duration,
    ) -> Result<(), StorageError> {
        self.message
            .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
            .await
            .map_err(|e| StorageError::Io(format!("dispatch signal delayed nack: {e}")))
    }
}
