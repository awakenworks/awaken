use async_trait::async_trait;
use tirea_contract::storage::{
    paginate_mailbox_entries, paginate_runs_in_memory, MailboxEntry, MailboxPage, MailboxQuery,
    MailboxReader, MailboxStoreError, MailboxWriter, RunPage, RunQuery, RunReader, RunRecord,
    RunStoreError, RunWriter, ThreadHead, ThreadListPage, ThreadListQuery, ThreadReader,
    ThreadStoreError, ThreadSync, ThreadWriter, VersionPrecondition,
};
use tirea_contract::{Committed, Thread, ThreadChangeSet, Version};

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

struct MemoryEntry {
    thread: Thread,
    version: Version,
    deltas: Vec<ThreadChangeSet>,
}

/// In-memory storage for testing and local development.
#[derive(Default)]
pub struct MemoryStore {
    entries: tokio::sync::RwLock<std::collections::HashMap<String, MemoryEntry>>,
    runs: tokio::sync::RwLock<std::collections::HashMap<String, RunRecord>>,
    mailbox: tokio::sync::RwLock<std::collections::HashMap<String, MailboxEntry>>,
}

impl MemoryStore {
    /// Create a new in-memory storage.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MailboxReader for MemoryStore {
    async fn load_mailbox_entry(
        &self,
        entry_id: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        Ok(self.mailbox.read().await.get(entry_id).cloned())
    }

    async fn load_mailbox_entry_by_run_id(
        &self,
        run_id: &str,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        let mailbox = self.mailbox.read().await;
        Ok(mailbox
            .values()
            .find(|entry| entry.run_id == run_id)
            .cloned())
    }

    async fn list_mailbox_entries(
        &self,
        query: &MailboxQuery,
    ) -> Result<MailboxPage, MailboxStoreError> {
        let mailbox = self.mailbox.read().await;
        let entries: Vec<MailboxEntry> = mailbox.values().cloned().collect();
        Ok(paginate_mailbox_entries(&entries, query))
    }
}

#[async_trait]
impl MailboxWriter for MemoryStore {
    async fn enqueue_mailbox_entry(&self, entry: &MailboxEntry) -> Result<(), MailboxStoreError> {
        let mut mailbox = self.mailbox.write().await;
        if mailbox.contains_key(&entry.entry_id)
            || mailbox
                .values()
                .any(|existing| existing.run_id == entry.run_id)
        {
            return Err(MailboxStoreError::AlreadyExists(entry.entry_id.clone()));
        }
        mailbox.insert(entry.entry_id.clone(), entry.clone());
        Ok(())
    }

    async fn claim_mailbox_entries(
        &self,
        limit: usize,
        consumer_id: &str,
        now: u64,
        lease_duration_ms: u64,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError> {
        let mut mailbox = self.mailbox.write().await;
        let mut claimable_ids: Vec<String> = mailbox
            .values()
            .filter(|entry| entry.is_claimable(now))
            .map(|entry| entry.entry_id.clone())
            .collect();
        claimable_ids.sort_by(|left, right| {
            let left_entry = mailbox.get(left).expect("mailbox entry should exist");
            let right_entry = mailbox.get(right).expect("mailbox entry should exist");
            left_entry
                .available_at
                .cmp(&right_entry.available_at)
                .then_with(|| left_entry.created_at.cmp(&right_entry.created_at))
                .then_with(|| left.cmp(right))
        });

        let mut claimed = Vec::new();
        for entry_id in claimable_ids.into_iter().take(limit) {
            let Some(entry) = mailbox.get_mut(&entry_id) else {
                continue;
            };
            entry.status = tirea_contract::MailboxEntryStatus::Claimed;
            entry.claim_token = Some(uuid::Uuid::now_v7().simple().to_string());
            entry.claimed_by = Some(consumer_id.to_string());
            entry.lease_until = Some(now.saturating_add(lease_duration_ms));
            entry.attempt_count = entry.attempt_count.saturating_add(1);
            entry.updated_at = now;
            claimed.push(entry.clone());
        }
        Ok(claimed)
    }

    async fn ack_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        accepted_run_id: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError> {
        let mut mailbox = self.mailbox.write().await;
        let entry = mailbox
            .get_mut(entry_id)
            .ok_or_else(|| MailboxStoreError::NotFound(entry_id.to_string()))?;
        if entry.claim_token.as_deref() != Some(claim_token) {
            return Err(MailboxStoreError::ClaimConflict(entry_id.to_string()));
        }
        entry.status = tirea_contract::MailboxEntryStatus::Accepted;
        entry.accepted_run_id = Some(accepted_run_id.to_string());
        entry.claim_token = None;
        entry.claimed_by = None;
        entry.lease_until = None;
        entry.updated_at = now;
        Ok(())
    }

    async fn nack_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError> {
        let mut mailbox = self.mailbox.write().await;
        let entry = mailbox
            .get_mut(entry_id)
            .ok_or_else(|| MailboxStoreError::NotFound(entry_id.to_string()))?;
        if entry.claim_token.as_deref() != Some(claim_token) {
            return Err(MailboxStoreError::ClaimConflict(entry_id.to_string()));
        }
        entry.status = tirea_contract::MailboxEntryStatus::Queued;
        entry.available_at = retry_at;
        entry.last_error = Some(error.to_string());
        entry.claim_token = None;
        entry.claimed_by = None;
        entry.lease_until = None;
        entry.updated_at = now;
        Ok(())
    }

    async fn dead_letter_mailbox_entry(
        &self,
        entry_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), MailboxStoreError> {
        let mut mailbox = self.mailbox.write().await;
        let entry = mailbox
            .get_mut(entry_id)
            .ok_or_else(|| MailboxStoreError::NotFound(entry_id.to_string()))?;
        if entry.claim_token.as_deref() != Some(claim_token) {
            return Err(MailboxStoreError::ClaimConflict(entry_id.to_string()));
        }
        entry.status = tirea_contract::MailboxEntryStatus::DeadLetter;
        entry.last_error = Some(error.to_string());
        entry.claim_token = None;
        entry.claimed_by = None;
        entry.lease_until = None;
        entry.updated_at = now;
        Ok(())
    }

    async fn cancel_mailbox_entry_by_run_id(
        &self,
        run_id: &str,
        now: u64,
    ) -> Result<Option<MailboxEntry>, MailboxStoreError> {
        let mut mailbox = self.mailbox.write().await;
        let Some(entry) = mailbox.values_mut().find(|entry| entry.run_id == run_id) else {
            return Ok(None);
        };
        if entry.status.is_terminal() {
            return Ok(Some(entry.clone()));
        }
        entry.status = tirea_contract::MailboxEntryStatus::Cancelled;
        entry.last_error = Some("cancelled".to_string());
        entry.claim_token = None;
        entry.claimed_by = None;
        entry.lease_until = None;
        entry.updated_at = now;
        Ok(Some(entry.clone()))
    }

    async fn cancel_pending_mailbox_for_thread(
        &self,
        thread_id: &str,
        now: u64,
        exclude_run_id: Option<&str>,
    ) -> Result<Vec<MailboxEntry>, MailboxStoreError> {
        let mut mailbox = self.mailbox.write().await;
        let mut cancelled = Vec::new();
        for entry in mailbox.values_mut() {
            if entry.thread_id != thread_id || entry.status.is_terminal() {
                continue;
            }
            if exclude_run_id.is_some_and(|run_id| entry.run_id == run_id) {
                continue;
            }
            entry.status = tirea_contract::MailboxEntryStatus::Cancelled;
            entry.last_error = Some("cancelled".to_string());
            entry.claim_token = None;
            entry.claimed_by = None;
            entry.lease_until = None;
            entry.updated_at = now;
            cancelled.push(entry.clone());
        }
        Ok(cancelled)
    }
}

#[async_trait]
impl ThreadWriter for MemoryStore {
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError> {
        let mut entries = self.entries.write().await;
        if entries.contains_key(&thread.id) {
            return Err(ThreadStoreError::AlreadyExists);
        }
        entries.insert(
            thread.id.clone(),
            MemoryEntry {
                thread: thread.clone(),
                version: 0,
                deltas: Vec::new(),
            },
        );
        Ok(Committed { version: 0 })
    }

    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError> {
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(thread_id)
            .ok_or_else(|| ThreadStoreError::NotFound(thread_id.to_string()))?;

        if let VersionPrecondition::Exact(expected) = precondition {
            if entry.version != expected {
                return Err(ThreadStoreError::VersionConflict {
                    expected,
                    actual: entry.version,
                });
            }
        }

        delta.apply_to(&mut entry.thread);
        entry.version += 1;
        entry.deltas.push(delta.clone());

        // Maintain run index from changeset metadata.
        if !delta.run_id.is_empty() {
            let now = now_unix_millis();
            let mut runs = self.runs.write().await;
            if let Some(meta) = &delta.run_meta {
                let record = runs.entry(delta.run_id.clone()).or_insert_with(|| {
                    RunRecord::new(
                        &delta.run_id,
                        thread_id,
                        &meta.agent_id,
                        meta.origin,
                        meta.status,
                        now,
                    )
                });
                record.status = meta.status;
                record.agent_id.clone_from(&meta.agent_id);
                record.origin = meta.origin;
                record.thread_id = thread_id.to_string();
                if record.parent_run_id.is_none() {
                    record.parent_run_id.clone_from(&delta.parent_run_id);
                }
                if record.parent_thread_id.is_none() {
                    record.parent_thread_id.clone_from(&meta.parent_thread_id);
                }
                record.termination_code.clone_from(&meta.termination_code);
                record
                    .termination_detail
                    .clone_from(&meta.termination_detail);
                record.updated_at = now;
            } else if let Some(record) = runs.get_mut(&delta.run_id) {
                record.updated_at = now;
            }
        }

        Ok(Committed {
            version: entry.version,
        })
    }

    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError> {
        let mut entries = self.entries.write().await;
        entries.remove(thread_id);
        Ok(())
    }

    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError> {
        let mut entries = self.entries.write().await;
        let version = entries.get(&thread.id).map_or(0, |e| e.version + 1);
        entries.insert(
            thread.id.clone(),
            MemoryEntry {
                thread: thread.clone(),
                version,
                deltas: Vec::new(),
            },
        );
        Ok(())
    }
}

#[async_trait]
impl RunReader for MemoryStore {
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        Ok(self.runs.read().await.get(run_id).cloned())
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, RunStoreError> {
        let runs = self.runs.read().await;
        let records: Vec<RunRecord> = runs.values().cloned().collect();
        Ok(paginate_runs_in_memory(&records, query))
    }

    async fn load_current_run(&self, thread_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.thread_id == thread_id && !r.status.is_terminal())
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.updated_at.cmp(&b.updated_at))
                    .then_with(|| a.run_id.cmp(&b.run_id))
            })
            .cloned())
    }
}

#[async_trait]
impl RunWriter for MemoryStore {
    async fn upsert_run(&self, record: &RunRecord) -> Result<(), RunStoreError> {
        self.runs
            .write()
            .await
            .insert(record.run_id.clone(), record.clone());
        Ok(())
    }

    async fn delete_run(&self, run_id: &str) -> Result<(), RunStoreError> {
        self.runs.write().await.remove(run_id);
        Ok(())
    }
}

#[async_trait]
impl ThreadReader for MemoryStore {
    async fn load(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError> {
        let entries = self.entries.read().await;
        Ok(entries.get(thread_id).map(|e| ThreadHead {
            thread: e.thread.clone(),
            version: e.version,
        }))
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, ThreadStoreError> {
        Ok(self.runs.read().await.get(run_id).cloned())
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, ThreadStoreError> {
        let runs = self.runs.read().await;
        let records: Vec<RunRecord> = runs.values().cloned().collect();
        Ok(paginate_runs_in_memory(&records, query))
    }

    async fn active_run_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<RunRecord>, ThreadStoreError> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.thread_id == thread_id && !r.status.is_terminal())
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.updated_at.cmp(&b.updated_at))
                    .then_with(|| a.run_id.cmp(&b.run_id))
            })
            .cloned())
    }

    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        let entries = self.entries.read().await;
        let mut ids: Vec<String> = entries
            .iter()
            .filter(|(_, e)| {
                if let Some(ref rid) = query.resource_id {
                    e.thread.resource_id.as_deref() == Some(rid.as_str())
                } else {
                    true
                }
            })
            .filter(|(_, e)| {
                if let Some(ref pid) = query.parent_thread_id {
                    e.thread.parent_thread_id.as_deref() == Some(pid.as_str())
                } else {
                    true
                }
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        let total = ids.len();
        let limit = query.limit.clamp(1, 200);
        let offset = query.offset.min(total);
        let end = (offset + limit + 1).min(total);
        let slice = &ids[offset..end];
        let has_more = slice.len() > limit;
        let items: Vec<String> = slice.iter().take(limit).cloned().collect();
        Ok(ThreadListPage {
            items,
            total,
            has_more,
        })
    }
}

#[async_trait]
impl ThreadSync for MemoryStore {
    async fn load_deltas(
        &self,
        thread_id: &str,
        after_version: Version,
    ) -> Result<Vec<ThreadChangeSet>, ThreadStoreError> {
        let entries = self.entries.read().await;
        let entry = entries
            .get(thread_id)
            .ok_or_else(|| ThreadStoreError::NotFound(thread_id.to_string()))?;
        // Deltas are 1-indexed: delta[0] produced version 1, delta[1] produced version 2, etc.
        let skip = after_version as usize;
        Ok(entry.deltas[skip..].to_vec())
    }
}
