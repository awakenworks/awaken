//! Background sweeper: re-publishes available Queued dispatches.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use awaken_server_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use tokio::sync::RwLock;

use super::{index::DispatchIndex, keys, metrics};

#[derive(Default)]
struct SweeperState {
    published: HashMap<String, PublishedSignal>,
}

#[derive(Clone, Copy)]
struct PublishedSignal {
    last_published_at: u64,
    publish_count: u64,
}

pub fn spawn_sweeper(
    jetstream: async_nats::jetstream::Context,
    index: Arc<RwLock<DispatchIndex>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    interval: Duration,
    republish_after: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let state = Arc::new(RwLock::new(SweeperState::default()));
        let mut ticker = tokio::time::interval(interval);
        let republish_after_ms = republish_after.as_millis().try_into().unwrap_or(u64::MAX);
        ticker.tick().await; // skip initial immediate tick

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { break; }
                }
                _ = ticker.tick() => {
                    let now = current_millis();
                    tick(&jetstream, &index, &state, now, republish_after_ms).await;
                }
            }
        }
    })
}

async fn tick(
    jetstream: &async_nats::jetstream::Context,
    index: &Arc<RwLock<DispatchIndex>>,
    state: &Arc<RwLock<SweeperState>>,
    now: u64,
    republish_after_ms: u64,
) {
    let candidates = index.read().await.available_queued(now);
    for candidate in candidates {
        metrics::record_queued_without_signal_age(&candidate, now);
        let should_wait = state
            .read()
            .await
            .published
            .get(candidate.dispatch_id())
            .is_some_and(|published| {
                now.saturating_sub(published.last_published_at) < republish_after_ms
            });
        if should_wait {
            continue;
        }
        if publish(jetstream, &candidate).await {
            let mut state_w = state.write().await;
            let entry = state_w
                .published
                .entry(candidate.dispatch_id().to_string())
                .or_insert(PublishedSignal {
                    last_published_at: 0,
                    publish_count: 0,
                });
            entry.last_published_at = now;
            entry.publish_count = entry.publish_count.saturating_add(1);
            if entry.publish_count > 1 {
                metrics::inc_dispatch_signal_republish();
                tracing::warn!(
                    thread_id = %candidate.thread_id(),
                    dispatch_id = %candidate.dispatch_id(),
                    publish_count = entry.publish_count,
                    "republished queued dispatch signal"
                );
            }
        }
    }
    // Clean up tracking for dispatches no longer Queued.
    let mut state_w = state.write().await;
    let idx = index.read().await;
    state_w.published.retain(|id, _| {
        idx.get(id)
            .is_some_and(|d| d.status() == RunDispatchStatus::Queued)
    });
}

async fn publish(jetstream: &async_nats::jetstream::Context, dispatch: &RunDispatch) -> bool {
    let payload = bytes::Bytes::from(dispatch.dispatch_id().as_bytes().to_vec());
    match jetstream
        .publish(keys::dispatch_subject(dispatch.thread_id()), payload)
        .await
    {
        Ok(ack_future) => ack_future.await.is_ok(),
        Err(_) => false,
    }
}

pub(super) fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
