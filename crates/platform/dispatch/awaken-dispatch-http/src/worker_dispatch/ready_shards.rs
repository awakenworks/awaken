// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: This process-local cache shares candidate observations only; exact claims retain authority.
use std::time::Duration;

use awaken_dispatch_contract::{DispatchControlClaims, DispatchError};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::{Mutex, Notify, Semaphore, SemaphorePermit};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub(super) const REFRESH: Duration = Duration::from_millis(250);

pub(super) struct ReadyShards {
    cached: Mutex<Option<Observation>>,
    pub(super) changed: Notify,
    listener: OnceLock<JoinHandle<()>>,
    claims: [Semaphore; 256],
}

struct Observation {
    refresh_after: Instant,
    shards: Option<Vec<u8>>,
    next: usize,
}

impl ReadyShards {
    pub(super) fn new(wake: Option<Arc<dyn awaken_dispatch_contract::WakeSignal>>) -> Arc<Self> {
        let cache = Arc::new(Self {
            cached: Mutex::new(None),
            changed: Notify::new(),
            listener: OnceLock::new(),
            claims: std::array::from_fn(|_| Semaphore::new(1)),
        });
        if let Some(wake) = wake {
            let weak = Arc::downgrade(&cache);
            let listener = tokio::spawn(async move {
                loop {
                    let shard = wake.wait_shard().await;
                    let Some(cache) = weak.upgrade() else { break };
                    cache.notify(shard).await;
                }
            });
            assert!(cache.listener.set(listener).is_ok());
        }
        cache
    }

    pub(super) fn try_claim(&self, shard: u8) -> Option<SemaphorePermit<'_>> {
        self.claims[usize::from(shard)].try_acquire().ok()
    }

    /// A single pending read per Coordinator, and no retained SQL connection
    /// while the workers wait. Rotate hints so unrelated shards can progress.
    pub(super) async fn observe(
        &self,
        dispatch: &dyn DispatchControlClaims,
    ) -> Result<Option<Vec<u8>>, DispatchError> {
        let mut cached = self.cached.lock().await;
        if cached
            .as_ref()
            .is_none_or(|value| value.refresh_after <= awaken_time::mono::now())
        {
            let shards = dispatch.claimable_shards().await?;
            let next = cached.as_ref().map_or(0, |value| value.next);
            *cached = Some(Observation {
                refresh_after: awaken_time::mono::now() + REFRESH,
                shards,
                next,
            });
        }
        let observation = cached
            .as_mut()
            .expect("successful refresh installs an observation");
        let mut shards = observation.shards.clone();
        if let Some(shards) = &mut shards
            && !shards.is_empty()
        {
            let offset = observation.next % shards.len();
            shards.rotate_left(offset);
            observation.next = observation.next.wrapping_add(1);
        }
        Ok(shards)
    }

    pub(super) async fn notify(&self, shard: Option<u8>) {
        let mut cached = self.cached.lock().await;
        if let Some(observation) = cached.as_mut() {
            if let Some(shard) = shard {
                if let Some(shards) = &mut observation.shards
                    && !shards.contains(&shard)
                {
                    shards.push(shard);
                }
            } else {
                observation.refresh_after = awaken_time::mono::now();
            }
        }
        self.changed.notify_waiters();
    }
}

impl Drop for ReadyShards {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.get() {
            listener.abort();
        }
    }
}
