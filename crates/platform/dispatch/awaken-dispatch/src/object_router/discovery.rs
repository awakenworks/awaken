// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Bounded discovery retains progress independently of authoritative claims.
use super::{DispatchError, ObjectDispatchStore};
use awaken_time::WallClockExt as _;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

const MAX_TRACKED_SEARCHES: usize = 4_096;
pub(super) const SESSIONS_PER_SEARCH: usize = 64;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) enum SearchKind {
    Local,
    Compatible,
    Placed,
    Requested,
    RetryExhausted,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct Search {
    owner: String,
    kind: SearchKind,
    shard: Option<u8>,
}

#[derive(Default)]
struct Position {
    due_at: u64,
    after: Option<String>,
}

#[derive(Default)]
pub(super) struct Discovery(awaken_sync::LeafLock<HashMap<Search, Arc<Mutex<Position>>>>);

impl Discovery {
    fn position(&self, search: Search) -> Arc<Mutex<Position>> {
        let mut positions = self.0.lock();
        if positions.len() >= MAX_TRACKED_SEARCHES && !positions.contains_key(&search) {
            positions.retain(|_, position| Arc::strong_count(position) > 1);
        }
        if positions.len() >= MAX_TRACKED_SEARCHES && !positions.contains_key(&search) {
            return Arc::default();
        }
        positions.entry(search).or_default().clone()
    }
}

impl ObjectDispatchStore {
    pub(super) async fn discovery_in(
        &self,
        owner: &str,
        kind: SearchKind,
        shard: Option<u8>,
    ) -> Result<crate::memory::MemoryDispatchStore, DispatchError> {
        self.ensure_claim_index().await?;
        let position = self.discovery.position(Search {
            owner: owner.into(),
            kind,
            shard,
        });
        let mut position = position.try_lock().map_err(|_| {
            DispatchError::unavailable("Dispatch discovery already running for this Worker")
        })?;
        if position.after.is_none() {
            position.due_at = self.clock.now_ms();
        }
        let found = self
            .objects
            .scan_runnable_sessions(
                position.due_at,
                shard,
                position.after.as_deref(),
                SESSIONS_PER_SEARCH,
            )
            .await
            .map_err(DispatchError::unavailable)?;
        let snapshot = crate::memory::MemoryDispatchStore::discovery_snapshot(
            &found.sessions,
            self.clock.clone(),
        )?;
        position.after = found.resume_after;
        Ok(snapshot)
    }
}
