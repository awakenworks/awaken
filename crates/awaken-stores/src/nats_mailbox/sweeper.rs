//! Background sweeper: re-publishes available Queued dispatches.

use std::sync::Arc;
use std::time::Duration;

use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use tokio::sync::RwLock;

use super::{index::DispatchIndex, keys};

#[derive(Default)]
struct SweeperState {
    already_published: std::collections::HashSet<String>,
}

pub fn spawn_sweeper(
    jetstream: async_nats::jetstream::Context,
    index: Arc<RwLock<DispatchIndex>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let state = Arc::new(RwLock::new(SweeperState::default()));
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // skip initial immediate tick

        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { break; }
                }
                _ = ticker.tick() => {
                    let now = current_millis();
                    tick(&jetstream, &index, &state, now).await;
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
) {
    let candidates = index.read().await.available_queued(now);
    for candidate in candidates {
        let already = state
            .read()
            .await
            .already_published
            .contains(&candidate.dispatch_id);
        if already {
            continue;
        }
        if publish(jetstream, &candidate).await {
            state
                .write()
                .await
                .already_published
                .insert(candidate.dispatch_id);
        }
    }
    // Clean up tracking for dispatches no longer Queued.
    let mut state_w = state.write().await;
    let idx = index.read().await;
    state_w.already_published.retain(|id| {
        idx.get(id)
            .is_some_and(|d| d.status == RunDispatchStatus::Queued)
    });
}

async fn publish(jetstream: &async_nats::jetstream::Context, dispatch: &RunDispatch) -> bool {
    let payload = bytes::Bytes::from(dispatch.dispatch_id.clone().into_bytes());
    match jetstream
        .publish(keys::dispatch_subject(&dispatch.thread_id), payload)
        .await
    {
        Ok(ack_future) => ack_future.await.is_ok(),
        Err(_) => false,
    }
}

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
