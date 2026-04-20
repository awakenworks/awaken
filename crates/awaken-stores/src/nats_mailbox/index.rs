//! In-memory dispatch index, kept in sync with KV bucket via watcher.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use tokio::sync::RwLock;

use super::{codec, keys};

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
        let id = dispatch.dispatch_id.clone();
        if let Some(prev) = self.by_id.get(&id) {
            if !force {
                if let (Some(prev_revision), Some(incoming_revision)) = (prev.revision, revision)
                    && incoming_revision < prev_revision
                {
                    return;
                }
                if revision.is_none() && dispatch.updated_at < prev.dispatch.updated_at {
                    return;
                }
            }
            if let Some(set) = self.by_status.get_mut(&status_key(prev.dispatch.status)) {
                set.remove(&id);
            }
        } else {
            self.by_thread
                .entry(dispatch.thread_id.clone())
                .or_default()
                .push(id.clone());
        }
        self.by_status
            .entry(status_key(dispatch.status))
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
            if let Some(set) = self.by_status.get_mut(&status_key(dispatch.status)) {
                set.remove(dispatch_id);
            }
            if let Some(ids) = self.by_thread.get_mut(&dispatch.thread_id) {
                ids.retain(|id| id != dispatch_id);
                if ids.is_empty() {
                    self.by_thread.remove(&dispatch.thread_id);
                }
            }
        }
    }

    pub fn get(&self, dispatch_id: &str) -> Option<&RunDispatch> {
        self.by_id.get(dispatch_id).map(|indexed| &indexed.dispatch)
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
                Some(filter) => filter.contains(&d.status),
                None => true,
            })
            .collect()
    }

    pub fn queued_thread_ids(&self) -> Vec<String> {
        let Some(queued_ids) = self.by_status.get(&status_key(RunDispatchStatus::Queued)) else {
            return Vec::new();
        };
        let mut threads: HashSet<String> = HashSet::new();
        for id in queued_ids {
            if let Some(indexed) = self.by_id.get(id) {
                threads.insert(indexed.dispatch.thread_id.clone());
            }
        }
        threads.into_iter().collect()
    }

    pub fn count_by_status(&self, status: RunDispatchStatus) -> usize {
        self.by_status
            .get(&status_key(status))
            .map_or(0, HashSet::len)
    }

    pub fn terminal_older_than(&self, cutoff: u64) -> Vec<String> {
        let terminal_statuses = [
            RunDispatchStatus::Acked,
            RunDispatchStatus::Cancelled,
            RunDispatchStatus::Superseded,
            RunDispatchStatus::DeadLetter,
        ];
        let mut out = Vec::new();
        for status in terminal_statuses {
            if let Some(ids) = self.by_status.get(&status_key(status)) {
                for id in ids {
                    if let Some(indexed) = self.by_id.get(id) {
                        let dispatch = &indexed.dispatch;
                        if dispatch.completed_at.is_some_and(|c| c < cutoff) {
                            out.push(id.clone());
                        }
                    }
                }
            }
        }
        out
    }

    pub fn available_queued(&self, now: u64) -> Vec<RunDispatch> {
        let Some(queued_ids) = self.by_status.get(&status_key(RunDispatchStatus::Queued)) else {
            return Vec::new();
        };
        queued_ids
            .iter()
            .filter_map(|id| self.by_id.get(id).map(|indexed| &indexed.dispatch))
            .filter(|d| d.available_at <= now)
            .cloned()
            .collect()
    }
}

/// Spawn a background task that keeps the index in sync with the KV bucket.
pub fn spawn_watcher(
    kv: async_nats::jetstream::kv::Store,
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

        let initial_scan = initial_scan(&kv, &index).await.map_err(|e| e.to_string());
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
                        Some(Ok(entry)) => apply_entry(&index, entry).await,
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
    index: &Arc<RwLock<DispatchIndex>>,
) -> Result<(), async_nats::Error> {
    use futures::StreamExt;
    let mut keys = kv.keys().await?;
    while let Some(key_result) = keys.next().await {
        let key = match key_result {
            Ok(k) => k,
            Err(_) => continue,
        };
        if let Ok(Some(entry)) = kv.entry(&key).await
            && let Ok(dispatch) = codec::decode(&entry.value)
        {
            index
                .write()
                .await
                .upsert_with_revision(dispatch, entry.revision);
        }
    }
    Ok(())
}

async fn apply_entry(index: &Arc<RwLock<DispatchIndex>>, entry: async_nats::jetstream::kv::Entry) {
    use async_nats::jetstream::kv::Operation;
    match entry.operation {
        Operation::Put => {
            if let Ok(dispatch) = codec::decode(&entry.value) {
                index
                    .write()
                    .await
                    .upsert_with_revision(dispatch, entry.revision);
            }
        }
        Operation::Delete | Operation::Purge => {
            if let Some(id) = keys::dispatch_id_from_key(&entry.key) {
                index
                    .write()
                    .await
                    .remove_with_revision(&id, entry.revision);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(id: &str, status: RunDispatchStatus, updated_at: u64) -> RunDispatch {
        RunDispatch {
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
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            dispatch_instance_id: None,
            run_status: None,
            termination: None,
            run_response: None,
            run_error: None,
            completed_at: None,
            created_at: 0,
            updated_at,
        }
    }

    #[test]
    fn revision_guard_rejects_stale_watcher_events() {
        let mut index = DispatchIndex::default();
        index.upsert_with_revision(dispatch("d1", RunDispatchStatus::Claimed, 20), 20);
        index.upsert_with_revision(dispatch("d1", RunDispatchStatus::Queued, 10), 10);

        assert_eq!(
            index.get("d1").map(|dispatch| dispatch.status),
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
