//! In-memory dispatch index, kept in sync with KV bucket via watcher.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use awaken_server_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_server_contract::contract::storage::StorageError;
use tokio::sync::RwLock;

use super::{codec, keys, kv_helpers, metrics, ops_write};

fn status_key(status: RunDispatchStatus) -> String {
    format!("{status:?}")
}

#[derive(Default)]
pub struct DispatchIndex {
    by_id: HashMap<String, IndexedDispatch>,
    by_thread: HashMap<String, Vec<String>>,
    by_status: HashMap<String, HashSet<String>>,
}

struct IndexedDispatch {
    dispatch: RunDispatch,
    revision: Option<u64>,
}

impl DispatchIndex {
    pub fn upsert(&mut self, dispatch: RunDispatch) {
        self.upsert_inner(dispatch, None, false);
    }

    pub fn upsert_with_revision(&mut self, dispatch: RunDispatch, revision: u64) {
        self.upsert_inner(dispatch, Some(revision), false);
    }

    pub fn force_upsert(&mut self, dispatch: RunDispatch) {
        self.upsert_inner(dispatch, None, true);
    }

    fn upsert_inner(&mut self, dispatch: RunDispatch, revision: Option<u64>, force: bool) {
        let id = dispatch.dispatch_id().to_string();
        if let Some(prev) = self.by_id.get(&id) {
            if !force {
                if let (Some(prev_revision), Some(incoming_revision)) = (prev.revision, revision)
                    && incoming_revision < prev_revision
                {
                    return;
                }
                if revision.is_none() && dispatch.updated_at() < prev.dispatch.updated_at() {
                    return;
                }
            }
            if let Some(set) = self.by_status.get_mut(&status_key(prev.dispatch.status())) {
                set.remove(&id);
            }
        } else {
            self.by_thread
                .entry(dispatch.thread_id().to_string())
                .or_default()
                .push(id.clone());
        }
        self.by_status
            .entry(status_key(dispatch.status()))
            .or_default()
            .insert(id.clone());
        self.by_id
            .insert(id, IndexedDispatch { dispatch, revision });
    }

    pub fn remove(&mut self, dispatch_id: &str) {
        self.remove_inner(dispatch_id, None);
    }

    pub fn remove_with_revision(&mut self, dispatch_id: &str, revision: u64) {
        self.remove_inner(dispatch_id, Some(revision));
    }

    fn remove_inner(&mut self, dispatch_id: &str, revision: Option<u64>) {
        if let Some(incoming_revision) = revision
            && let Some(indexed) = self.by_id.get(dispatch_id)
            && let Some(current_revision) = indexed.revision
            && incoming_revision < current_revision
        {
            return;
        }
        if let Some(indexed) = self.by_id.remove(dispatch_id) {
            let dispatch = indexed.dispatch;
            if let Some(set) = self.by_status.get_mut(&status_key(dispatch.status())) {
                set.remove(dispatch_id);
            }
            if let Some(ids) = self.by_thread.get_mut(dispatch.thread_id()) {
                ids.retain(|id| id != dispatch_id);
                if ids.is_empty() {
                    self.by_thread.remove(dispatch.thread_id());
                }
            }
        }
    }

    pub fn get(&self, dispatch_id: &str) -> Option<&RunDispatch> {
        self.by_id.get(dispatch_id).map(|indexed| &indexed.dispatch)
    }

    pub fn get_cloned(&self, dispatch_id: &str) -> Option<RunDispatch> {
        self.by_id
            .get(dispatch_id)
            .map(|indexed| indexed.dispatch.clone())
    }

    pub fn list_by_thread(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
    ) -> Vec<RunDispatch> {
        let Some(ids) = self.by_thread.get(thread_id) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| self.by_id.get(id).map(|indexed| indexed.dispatch.clone()))
            .filter(|d| match status_filter {
                Some(filter) => filter.contains(&d.status()),
                None => true,
            })
            .collect()
    }

    pub fn best_queued_for_thread(
        &self,
        thread_id: &str,
        now: u64,
    ) -> (usize, Option<RunDispatch>) {
        let Some(ids) = self.by_thread.get(thread_id) else {
            return (0, None);
        };
        let mut eligible_count = 0usize;
        let mut best: Option<&RunDispatch> = None;
        for id in ids {
            let Some(dispatch) = self.by_id.get(id).map(|indexed| &indexed.dispatch) else {
                continue;
            };
            if dispatch.status() != RunDispatchStatus::Queued || dispatch.available_at() > now {
                continue;
            }
            eligible_count += 1;
            if best.is_none_or(|current| {
                dispatch
                    .priority()
                    .cmp(&current.priority())
                    .then(dispatch.created_at().cmp(&current.created_at()))
                    .then(dispatch.dispatch_id().cmp(current.dispatch_id()))
                    .is_lt()
            }) {
                best = Some(dispatch);
            }
        }
        (eligible_count, best.cloned())
    }

    pub fn active_claimed_for_thread(&self, thread_id: &str, now: u64) -> Option<RunDispatch> {
        let ids = self.by_thread.get(thread_id)?;
        ids.iter()
            .filter_map(|id| self.by_id.get(id).map(|indexed| &indexed.dispatch))
            .filter(|dispatch| {
                dispatch.status() == RunDispatchStatus::Claimed
                    && dispatch
                        .lease_until()
                        .is_some_and(|lease_until| lease_until >= now)
            })
            .min_by(|left, right| {
                left.lease_until()
                    .cmp(&right.lease_until())
                    .then(left.dispatch_id().cmp(right.dispatch_id()))
            })
            .cloned()
    }

    pub fn expired_claimed_dispatch_ids(&self, now: u64, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let Some(claimed_ids) = self.by_status.get(&status_key(RunDispatchStatus::Claimed)) else {
            return Vec::new();
        };
        let mut expired = claimed_ids
            .iter()
            .filter_map(|id| self.by_id.get(id).map(|indexed| &indexed.dispatch))
            .filter(|dispatch| {
                dispatch
                    .lease_until()
                    .is_some_and(|lease_until| lease_until < now)
            })
            .map(|dispatch| {
                (
                    dispatch.lease_until().unwrap_or(0),
                    dispatch.dispatch_id().to_string(),
                )
            })
            .collect::<Vec<_>>();
        expired.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        expired
            .into_iter()
            .take(limit)
            .map(|(_, dispatch_id)| dispatch_id)
            .collect()
    }

    pub fn queued_thread_ids(&self) -> Vec<String> {
        let Some(queued_ids) = self.by_status.get(&status_key(RunDispatchStatus::Queued)) else {
            return Vec::new();
        };
        let mut threads: HashSet<String> = HashSet::new();
        for id in queued_ids {
            if let Some(indexed) = self.by_id.get(id) {
                threads.insert(indexed.dispatch.thread_id().to_string());
            }
        }
        threads.into_iter().collect()
    }

    pub fn count_by_status(&self, status: RunDispatchStatus) -> usize {
        self.by_status
            .get(&status_key(status))
            .map_or(0, HashSet::len)
    }

    pub fn available_queued(&self, now: u64) -> Vec<RunDispatch> {
        let Some(queued_ids) = self.by_status.get(&status_key(RunDispatchStatus::Queued)) else {
            return Vec::new();
        };
        queued_ids
            .iter()
            .filter_map(|id| self.by_id.get(id).map(|indexed| &indexed.dispatch))
            .filter(|d| d.available_at() <= now)
            .cloned()
            .collect()
    }
}

/// Spawn a background task that keeps the index in sync with the KV bucket.
pub fn spawn_watcher(
    kv: async_nats::jetstream::kv::Store,
    kv_thread_index: async_nats::jetstream::kv::Store,
    index: Arc<RwLock<DispatchIndex>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Subscribe to new events FIRST, then do a catch-up scan. Order matters:
        // `watch_all()` uses `DeliverPolicy::New` which only reports changes made
        // after subscription, so any `put` that lands between subscription and
        // the catch-up scan will still arrive on the watcher stream. The reverse
        // order (scan then subscribe) would drop puts that landed in between.
        let mut watcher = match kv.watch_all().await {
            Ok(w) => w,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                tracing::warn!(error = %e, "nats_mailbox watch_all failed");
                return;
            }
        };

        let initial_scan = initial_scan(&kv, &kv_thread_index, &index)
            .await
            .map_err(|e| e.to_string());
        if let Err(ref e) = initial_scan {
            tracing::warn!(error = %e, "nats_mailbox watcher initial scan failed");
        }
        let _ = ready_tx.send(initial_scan);

        use futures::StreamExt;
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { break; }
                }
                entry = watcher.next() => {
                    match entry {
                        Some(Ok(entry)) => apply_entry(&kv_thread_index, &index, entry).await,
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "nats_mailbox watcher error");
                        }
                        None => break,
                    }
                }
            }
        }
    })
}

async fn initial_scan(
    kv: &async_nats::jetstream::kv::Store,
    kv_thread_index: &async_nats::jetstream::kv::Store,
    index: &Arc<RwLock<DispatchIndex>>,
) -> Result<(), StorageError> {
    use futures::StreamExt;
    let started = std::time::Instant::now();
    let mut keys = kv
        .keys()
        .await
        .map_err(|e| StorageError::Io(format!("kv dispatch keys: {e}")))?;
    let mut key_count = 0usize;
    let mut seen_threads: HashSet<String> = HashSet::new();
    let mut active_by_thread: HashMap<String, Vec<String>> = HashMap::new();
    while let Some(key_result) = keys.next().await {
        let key = match key_result {
            Ok(k) => k,
            Err(_) => continue,
        };
        key_count += 1;
        if let Ok(Some(entry)) = kv.entry(&key).await
            && !kv_helpers::is_tombstone(&entry)
            && let Ok(dispatch) = codec::decode(&entry.value)
        {
            seen_threads.insert(dispatch.thread_id().to_string());
            if !is_terminal(dispatch.status()) {
                active_by_thread
                    .entry(dispatch.thread_id().to_string())
                    .or_default()
                    .push(dispatch.dispatch_id().to_string());
            }
            index
                .write()
                .await
                .upsert_with_revision(dispatch, entry.revision);
        }
    }

    for thread_id in seen_threads {
        let mut active_ids = active_by_thread.remove(&thread_id).unwrap_or_default();
        active_ids.sort();
        active_ids.dedup();
        ops_write::replace_thread_index_bucket(kv_thread_index, &thread_id, &active_ids).await?;
    }

    metrics::record_watcher_initial_scan(key_count, started.elapsed());
    Ok(())
}

async fn apply_entry(
    kv_thread_index: &async_nats::jetstream::kv::Store,
    index: &Arc<RwLock<DispatchIndex>>,
    entry: async_nats::jetstream::kv::Entry,
) {
    if kv_helpers::is_tombstone(&entry) {
        if let Some(id) = keys::dispatch_id_from_key(&entry.key) {
            index
                .write()
                .await
                .remove_with_revision(&id, entry.revision);
        }
        return;
    }

    if let Ok(dispatch) = codec::decode(&entry.value) {
        if let Err(error) = reconcile_thread_index(kv_thread_index, &dispatch).await {
            tracing::warn!(
                thread_id = %dispatch.thread_id(),
                dispatch_id = %dispatch.dispatch_id(),
                error = %error,
                "failed to maintain nats_mailbox thread index from watcher"
            );
        }
        index
            .write()
            .await
            .upsert_with_revision(dispatch, entry.revision);
    }
}

async fn reconcile_thread_index(
    kv_thread_index: &async_nats::jetstream::kv::Store,
    dispatch: &RunDispatch,
) -> Result<(), StorageError> {
    if is_terminal(dispatch.status()) {
        ops_write::remove_thread_index_from_bucket(
            kv_thread_index,
            dispatch.thread_id(),
            dispatch.dispatch_id(),
        )
        .await
    } else {
        ops_write::append_thread_index_to_bucket(
            kv_thread_index,
            dispatch.thread_id(),
            dispatch.dispatch_id(),
        )
        .await
    }
}

fn is_terminal(status: RunDispatchStatus) -> bool {
    matches!(
        status,
        RunDispatchStatus::Acked
            | RunDispatchStatus::Cancelled
            | RunDispatchStatus::Superseded
            | RunDispatchStatus::DeadLetter
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(id: &str, status: RunDispatchStatus, updated_at: u64) -> RunDispatch {
        let parts = awaken_server_contract::contract::mailbox::RunDispatchParts {
            dispatch_id: id.to_string(),
            thread_id: "thread".to_string(),
            run_id: format!("{id}-run"),
            priority: 128,
            dedupe_key: None,
            dispatch_epoch: 0,
            status,
            available_at: 0,
            attempt_count: 0,
            max_attempts: 3,
            last_error: None,
            claim_token: (status == RunDispatchStatus::Claimed).then(|| "token".to_string()),
            claimed_by: (status == RunDispatchStatus::Claimed).then(|| "worker".to_string()),
            lease_until: (status == RunDispatchStatus::Claimed).then_some(10_000),
            dispatch_instance_id: None,
            run_status: None,
            termination: None,
            run_response: None,
            run_error: None,
            completed_at: status.is_terminal().then_some(updated_at),
            created_at: 0,
            updated_at,
        };
        RunDispatch::from_persisted_parts(parts).expect("test dispatch must be valid")
    }

    #[test]
    fn revision_guard_rejects_stale_watcher_events() {
        let mut index = DispatchIndex::default();
        index.upsert_with_revision(dispatch("d1", RunDispatchStatus::Claimed, 20), 20);
        index.upsert_with_revision(dispatch("d1", RunDispatchStatus::Queued, 10), 10);

        assert_eq!(
            index.get("d1").map(|dispatch| dispatch.status()),
            Some(RunDispatchStatus::Claimed)
        );
        assert_eq!(index.count_by_status(RunDispatchStatus::Claimed), 1);
        assert_eq!(index.count_by_status(RunDispatchStatus::Queued), 0);
    }

    #[test]
    fn revision_guard_rejects_stale_delete_events() {
        let mut index = DispatchIndex::default();
        index.upsert_with_revision(dispatch("d1", RunDispatchStatus::Queued, 20), 20);
        index.remove_with_revision("d1", 10);

        assert!(index.get("d1").is_some());
        assert_eq!(index.count_by_status(RunDispatchStatus::Queued), 1);
    }
}
