// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Coordinator-owned PostgreSQL and NATS hints use the one shared WakeSignal port.
// @formal reason: Lost or duplicate hints only affect latency; durable claim and polling retain authority.
//! Coordinator-owned PostgreSQL and NATS wake transports over the shared hint port.

#[cfg(feature = "durable")]
use async_trait::async_trait;
#[cfg(feature = "durable")]
use awaken_dispatch_contract::DispatchError;
#[cfg(feature = "durable")]
use awaken_dispatch_contract::WakeSignal;
#[cfg(feature = "durable")]
use std::sync::Arc;
#[cfg(feature = "durable")]
use tokio::sync::Notify;
#[cfg(feature = "durable")]
use tokio_util::sync::CancellationToken;

/// NATS-backed wake signal: publishes and awaits an empty message on a subject,
/// fanning the hint across a node fleet. Core NATS (not JetStream) — these are
/// fire-and-forget hints, exactly the at-most-once semantics a hint allows.
#[cfg(feature = "nats")]
pub struct NatsWakeSignal {
    client: async_nats::Client,
    subject: String,
    queue: tokio::sync::Mutex<Option<async_nats::Subscriber>>,
}

#[cfg(feature = "nats")]
impl NatsWakeSignal {
    /// Connect to a NATS server and publish/await hints on `subject`.
    pub async fn connect(url: &str, subject: impl Into<String>) -> Result<Self, DispatchError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|err| DispatchError::Rejected(err.to_string()))?;
        Ok(Self {
            client,
            subject: subject.into(),
            queue: tokio::sync::Mutex::new(None),
        })
    }
}

#[cfg(feature = "nats")]
#[async_trait]
impl WakeSignal for NatsWakeSignal {
    async fn publish(&self) -> Result<(), DispatchError> {
        self.client
            .publish(self.subject.clone(), Vec::new().into())
            .await
            .map_err(|err| DispatchError::Rejected(err.to_string()))
    }

    async fn publish_shard(&self, shard: u8) -> Result<(), DispatchError> {
        self.client
            .publish(self.subject.clone(), vec![shard].into())
            .await
            .map_err(|error| DispatchError::unavailable(error.to_string()))
    }

    async fn wait_shard(&self) -> Option<u8> {
        use futures_lite::StreamExt;
        let mut queue = self.queue.lock().await;
        if queue.is_none() {
            *queue = self
                .client
                .queue_subscribe(self.subject.clone(), format!("{}.claimers", self.subject))
                .await
                .ok();
        }
        let completed = match queue.as_mut() {
            Some(subscriber) => subscriber.next().await,
            None => None,
        };
        if let Some(message) = completed {
            return message.payload.first().copied();
        }
        *queue = None;
        drop(queue);
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        None
    }

    async fn wait(&self) {
        use futures_lite::StreamExt;
        // A lost subscription is tolerable (the poll fallback still drains).
        if let Ok(mut sub) = self.client.subscribe(self.subject.clone()).await {
            let _ = sub.next().await;
        }
    }
}

/// Postgres `LISTEN`/`NOTIFY` wake signal (B-P2, ADR-0021 §9). When the dispatch
/// store *is* Postgres, this fans the hint across every node connected to the same
/// database with **no extra infrastructure** — `publish` fires `pg_notify` (emit it
/// inside the enqueue transaction for same-commit delivery) and `wait` blocks on a
/// `PgListener`. Like every [`WakeSignal`] it is a hint; the poll fallback stays
/// authoritative, so a dropped notification only defers a drain.
#[cfg(feature = "durable")]
pub struct PgNotifyWake {
    pool: sqlx::postgres::PgPool,
    channel: String,
    /// In-process fan-out: the single background listener nudges this, every drain
    /// task waits on it. Keeps `wait` off the connection pool.
    local: Arc<Notify>,
    shard: Arc<std::sync::atomic::AtomicU16>,
    shutdown: CancellationToken,
    listener: Option<tokio::task::JoinHandle<()>>,
}

#[cfg(feature = "durable")]
impl PgNotifyWake {
    /// Wake over `channel` on the same database as the dispatch store `pool`.
    ///
    /// Each instance reserves one extra database connection for LISTEN, using
    /// the dispatch pool's connection options. It never takes an authority
    /// checkout while listening or reconnecting. NOTIFY uses the dispatch pool.
    pub fn new(pool: sqlx::postgres::PgPool, channel: impl Into<String>) -> Self {
        let channel = channel.into();
        let local = Arc::new(Notify::new());
        let listen_pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(1)
            .connect_lazy_with((*pool.connect_options()).clone());
        let listen_channel = channel.clone();
        let waiters = local.clone();
        let shard = Arc::new(std::sync::atomic::AtomicU16::new(0));
        let latest = shard.clone();
        let shutdown = CancellationToken::new();
        let cancelled = shutdown.clone();
        let listener = tokio::spawn(async move {
            let listen = async {
                loop {
                    if let Ok(mut listener) =
                        sqlx::postgres::PgListener::connect_with(&listen_pool).await
                        && listener.listen(&listen_channel).await.is_ok()
                    {
                        // Keep reconnects under this task's backoff and cancellation.
                        listener.eager_reconnect(false);
                        while let Ok(Some(message)) = listener.try_recv().await {
                            let payload = message
                                .payload()
                                .parse::<u8>()
                                .ok()
                                .map_or(0, |v| u16::from(v) + 1);
                            latest.store(payload, std::sync::atomic::Ordering::Relaxed);
                            waiters.notify_waiters();
                        }
                    }
                    // Reconnect after a failed connection or LISTEN. The durable
                    // polling path covers any hints missed during this backoff.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            };
            tokio::select! {
                biased;
                _ = cancelled.cancelled() => {}
                _ = listen => {}
            }
            // SQLx unregisters LISTEN while returning the connection. Bound
            // shutdown even when the database has stopped answering.
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(5), listen_pool.close()).await;
        });
        Self {
            pool,
            channel,
            local,
            shard,
            shutdown,
            listener: Some(listener),
        }
    }

    async fn notify(&self, payload: &str) -> Result<(), DispatchError> {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.channel)
            .bind(payload)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| DispatchError::unavailable(error.to_string()))
    }

    /// Stop and join this signal's sole listener. Consuming ownership prevents
    /// one shared `Arc` reference from shutting down another live owner.
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(listener) = self.listener.take() {
            let _ = listener.await;
        }
    }
}

#[cfg(feature = "durable")]
impl Drop for PgNotifyWake {
    fn drop(&mut self) {
        // Dropping the last owner cancels acquisition, LISTEN, recv, or backoff.
        // These are only notification work; no durable operation is discarded.
        self.shutdown.cancel();
    }
}

#[cfg(feature = "durable")]
#[async_trait]
impl WakeSignal for PgNotifyWake {
    async fn publish(&self) -> Result<(), DispatchError> {
        self.notify("").await
    }

    async fn publish_shard(&self, shard: u8) -> Result<(), DispatchError> {
        self.notify(&shard.to_string()).await
    }

    async fn wait_shard(&self) -> Option<u8> {
        self.wait().await;
        self.shard
            .swap(0, std::sync::atomic::Ordering::Relaxed)
            .checked_sub(1)
            .and_then(|shard| u8::try_from(shard).ok())
    }

    async fn wait(&self) {
        self.local.notified().await;
    }
}

#[cfg(all(test, feature = "nats"))]
#[path = "../tests/support/nats.rs"]
mod nats_tests;
