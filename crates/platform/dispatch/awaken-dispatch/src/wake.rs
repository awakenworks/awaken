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
    shutdown: CancellationToken,
    listener: Option<tokio::task::JoinHandle<()>>,
}

#[cfg(feature = "durable")]
impl PgNotifyWake {
    /// Wake over `channel` on the same database as the dispatch store `pool`.
    ///
    /// Spawns ONE background listener holding a single dedicated connection; each
    /// `pg_notify` fans out to every waiter through an in-process [`Notify`]. This is
    /// deliberate: `PgListener::connect_with` takes a pool connection and holds it for
    /// the whole `recv`, so a per-`wait` listener (the previous design) had every one
    /// of the pool's `available_parallelism()` drain tasks pin a connection while idle
    /// — starving the claim/commit/settle path in a fleet, so a concurrent burst
    /// stranded. With one listener, `wait` costs no pool connection. A missed hint
    /// (listener reconnect, or a notify with no waiter yet) only defers a drain to the
    /// poll fallback, which stays authoritative.
    pub fn new(pool: sqlx::postgres::PgPool, channel: impl Into<String>) -> Self {
        let channel = channel.into();
        let local = Arc::new(Notify::new());
        let listen_pool = pool.clone();
        let listen_channel = channel.clone();
        let waiters = local.clone();
        let shutdown = CancellationToken::new();
        let cancelled = shutdown.clone();
        let listener = tokio::spawn(async move {
            let listen = async {
                loop {
                    if let Ok(mut listener) =
                        sqlx::postgres::PgListener::connect_with(&listen_pool).await
                        && listener.listen(&listen_channel).await.is_ok()
                    {
                        while listener.recv().await.is_ok() {
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
        });
        Self {
            pool,
            channel,
            local,
            shutdown,
            listener: Some(listener),
        }
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
        sqlx::query("SELECT pg_notify($1, '')")
            .bind(&self.channel)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|err| DispatchError::Rejected(err.to_string()))
    }

    async fn wait(&self) {
        self.local.notified().await;
    }
}

#[cfg(all(test, feature = "nats"))]
#[path = "../tests/support/nats.rs"]
mod nats_tests;
