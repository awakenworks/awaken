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
    by_id: HashMap<String, RunDispatch>,
    by_thread: HashMap<String, Vec<String>>,
    by_status: HashMap<String, HashSet<String>>,
}

impl DispatchIndex {
    pub fn upsert(&mut self, dispatch: RunDispatch) {
        let id = dispatch.dispatch_id.clone();
        if let Some(prev) = self.by_id.get(&id) {
            if let Some(set) = self.by_status.get_mut(&status_key(prev.status)) {
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
        self.by_id.insert(id, dispatch);
    }

    pub fn remove(&mut self, dispatch_id: &str) {
        if let Some(dispatch) = self.by_id.remove(dispatch_id) {
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
        self.by_id.get(dispatch_id)
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
            .filter_map(|id| self.by_id.get(id).cloned())
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
            if let Some(d) = self.by_id.get(id) {
                threads.insert(d.thread_id.clone());
            }
        }
        threads.into_iter().collect()
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
                    if let Some(d) = self.by_id.get(id)
                        && d.completed_at.is_some_and(|c| c < cutoff)
                    {
                        out.push(id.clone());
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
            .filter_map(|id| self.by_id.get(id))
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
            index.write().await.upsert(dispatch);
        }
    }
    Ok(())
}

async fn apply_entry(index: &Arc<RwLock<DispatchIndex>>, entry: async_nats::jetstream::kv::Entry) {
    use async_nats::jetstream::kv::Operation;
    match entry.operation {
        Operation::Put => {
            if let Ok(dispatch) = codec::decode(&entry.value) {
                index.write().await.upsert(dispatch);
            }
        }
        Operation::Delete | Operation::Purge => {
            if let Some(id) = keys::dispatch_id_from_key(&entry.key) {
                index.write().await.remove(&id);
            }
        }
    }
}
