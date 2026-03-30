//! In-memory implementation of the new lease-based `MailboxStore`.

use std::collections::HashMap;

use async_trait::async_trait;
use awaken_contract::contract::mailbox::{
    MailboxInterrupt, MailboxJob, MailboxJobStatus, MailboxStore,
};
use awaken_contract::contract::storage::StorageError;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Per-mailbox generation counter for interrupt semantics.
struct MailboxState {
    current_generation: u64,
}

/// In-memory `MailboxStore` for testing and local development.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
/// Data lives only in memory and is lost when the store is dropped.
#[derive(Default)]
pub struct InMemoryMailboxStore {
    jobs: RwLock<HashMap<String, MailboxJob>>,
    state: RwLock<HashMap<String, MailboxState>>,
}

impl InMemoryMailboxStore {
    /// Create a new empty in-memory mailbox store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MailboxStore for InMemoryMailboxStore {
    async fn enqueue(&self, job: &MailboxJob) -> Result<(), StorageError> {
        let mut jobs = self.jobs.write().await;
        let mut state = self.state.write().await;

        // Dedupe check: reject if dedupe_key matches an existing non-terminal job.
        if let Some(ref dk) = job.dedupe_key {
            let duplicate = jobs.values().any(|j| {
                j.mailbox_id == job.mailbox_id
                    && j.dedupe_key.as_deref() == Some(dk)
                    && !j.status.is_terminal()
            });
            if duplicate {
                return Err(StorageError::AlreadyExists(format!("dedupe_key={dk}")));
            }
        }

        // Auto-create MailboxState if needed, get current generation.
        let ms = state.entry(job.mailbox_id.clone()).or_insert(MailboxState {
            current_generation: 0,
        });

        let mut job = job.clone();
        job.generation = ms.current_generation;
        job.status = MailboxJobStatus::Queued;

        jobs.insert(job.job_id.clone(), job);
        Ok(())
    }

    async fn claim(
        &self,
        mailbox_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        let mut jobs = self.jobs.write().await;

        // Collect eligible job IDs, sorted by (priority ASC, created_at ASC).
        let mut eligible: Vec<&String> = jobs
            .iter()
            .filter(|(_, j)| {
                j.mailbox_id == mailbox_id
                    && j.status == MailboxJobStatus::Queued
                    && j.available_at <= now
            })
            .map(|(id, _)| id)
            .collect();

        // Sort: need to access job data for sorting.
        eligible.sort_by(|a, b| {
            let ja = &jobs[*a];
            let jb = &jobs[*b];
            ja.priority
                .cmp(&jb.priority)
                .then(ja.created_at.cmp(&jb.created_at))
        });

        eligible.truncate(limit);
        let ids: Vec<String> = eligible.into_iter().cloned().collect();

        let token = Uuid::now_v7().to_string();
        let mut claimed = Vec::with_capacity(ids.len());

        for id in ids {
            let job = jobs
                .get_mut(&id)
                .ok_or_else(|| StorageError::NotFound(id.clone()))?;
            job.status = MailboxJobStatus::Claimed;
            job.claim_token = Some(token.clone());
            job.claimed_by = Some(consumer_id.to_string());
            job.lease_until = Some(now + lease_ms);
            job.updated_at = now;
            claimed.push(job.clone());
        }

        Ok(claimed)
    }

    async fn claim_job(
        &self,
        job_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<MailboxJob>, StorageError> {
        let mut jobs = self.jobs.write().await;

        let job = match jobs.get_mut(job_id) {
            Some(j) if j.status == MailboxJobStatus::Queued => j,
            _ => return Ok(None),
        };

        let token = Uuid::now_v7().to_string();
        job.status = MailboxJobStatus::Claimed;
        job.claim_token = Some(token);
        job.claimed_by = Some(consumer_id.to_string());
        job.lease_until = Some(now + lease_ms);
        job.updated_at = now;

        Ok(Some(job.clone()))
    }

    async fn ack(&self, job_id: &str, claim_token: &str, now: u64) -> Result<(), StorageError> {
        let mut jobs = self.jobs.write().await;

        let job = jobs
            .get_mut(job_id)
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))?;

        if job.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }

        job.status = MailboxJobStatus::Accepted;
        job.updated_at = now;
        Ok(())
    }

    async fn nack(
        &self,
        job_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let mut jobs = self.jobs.write().await;

        let job = jobs
            .get_mut(job_id)
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))?;

        if job.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }

        job.attempt_count += 1;
        job.last_error = Some(error.to_string());
        job.updated_at = now;

        if job.attempt_count >= job.max_attempts {
            job.status = MailboxJobStatus::DeadLetter;
        } else {
            job.status = MailboxJobStatus::Queued;
            job.available_at = retry_at;
            job.claim_token = None;
            job.claimed_by = None;
            job.lease_until = None;
        }

        Ok(())
    }

    async fn dead_letter(
        &self,
        job_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        let mut jobs = self.jobs.write().await;

        let job = jobs
            .get_mut(job_id)
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))?;

        if job.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }

        job.status = MailboxJobStatus::DeadLetter;
        job.last_error = Some(error.to_string());
        job.updated_at = now;
        Ok(())
    }

    async fn cancel(&self, job_id: &str, now: u64) -> Result<Option<MailboxJob>, StorageError> {
        let mut jobs = self.jobs.write().await;

        let job = match jobs.get_mut(job_id) {
            Some(j) if j.status == MailboxJobStatus::Queued => j,
            _ => return Ok(None),
        };

        job.status = MailboxJobStatus::Cancelled;
        job.updated_at = now;
        Ok(Some(job.clone()))
    }

    async fn extend_lease(
        &self,
        job_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        let mut jobs = self.jobs.write().await;

        let job = match jobs.get_mut(job_id) {
            Some(j)
                if j.status == MailboxJobStatus::Claimed
                    && j.claim_token.as_deref() == Some(claim_token) =>
            {
                j
            }
            _ => return Ok(false),
        };

        job.lease_until = Some(now + extension_ms);
        job.updated_at = now;
        Ok(true)
    }

    async fn interrupt(
        &self,
        mailbox_id: &str,
        now: u64,
    ) -> Result<MailboxInterrupt, StorageError> {
        let mut jobs = self.jobs.write().await;
        let mut state = self.state.write().await;

        let ms = state.entry(mailbox_id.to_string()).or_insert(MailboxState {
            current_generation: 0,
        });

        let old_gen = ms.current_generation;
        ms.current_generation += 1;
        let new_generation = ms.current_generation;

        let mut superseded_count = 0;
        let mut active_job = None;

        for job in jobs.values_mut() {
            if job.mailbox_id != mailbox_id {
                continue;
            }
            match job.status {
                MailboxJobStatus::Queued if job.generation <= old_gen => {
                    job.status = MailboxJobStatus::Superseded;
                    job.updated_at = now;
                    superseded_count += 1;
                }
                MailboxJobStatus::Claimed => {
                    active_job = Some(job.clone());
                }
                _ => {}
            }
        }

        Ok(MailboxInterrupt {
            new_generation,
            active_job,
            superseded_count,
        })
    }

    async fn load_job(&self, job_id: &str) -> Result<Option<MailboxJob>, StorageError> {
        let jobs = self.jobs.read().await;
        Ok(jobs.get(job_id).cloned())
    }

    async fn list_jobs(
        &self,
        mailbox_id: &str,
        status_filter: Option<&[MailboxJobStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        let jobs = self.jobs.read().await;

        let mut matched: Vec<&MailboxJob> = jobs
            .values()
            .filter(|j| {
                j.mailbox_id == mailbox_id
                    && status_filter
                        .map(|sf| sf.contains(&j.status))
                        .unwrap_or(true)
            })
            .collect();

        matched.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then(a.created_at.cmp(&b.created_at))
        });

        Ok(matched
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError> {
        let mut jobs = self.jobs.write().await;

        let expired_ids: Vec<String> = jobs
            .values()
            .filter(|j| {
                j.status == MailboxJobStatus::Claimed && j.lease_until.is_some_and(|lu| lu < now)
            })
            .take(limit)
            .map(|j| j.job_id.clone())
            .collect();

        let mut reclaimed = Vec::with_capacity(expired_ids.len());

        for id in expired_ids {
            let job = jobs
                .get_mut(&id)
                .ok_or_else(|| StorageError::NotFound(id.clone()))?;
            job.attempt_count += 1;
            job.updated_at = now;

            if job.attempt_count >= job.max_attempts {
                job.status = MailboxJobStatus::DeadLetter;
            } else {
                job.status = MailboxJobStatus::Queued;
                job.claim_token = None;
                job.claimed_by = None;
                job.lease_until = None;
            }
            reclaimed.push(job.clone());
        }

        Ok(reclaimed)
    }

    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError> {
        let mut jobs = self.jobs.write().await;
        let before = jobs.len();
        jobs.retain(|_, j| !(j.status.is_terminal() && j.updated_at < older_than));
        Ok(before - jobs.len())
    }

    async fn queued_mailbox_ids(&self) -> Result<Vec<String>, StorageError> {
        let jobs = self.jobs.read().await;
        let mut ids: Vec<String> = jobs
            .values()
            .filter(|j| j.status == MailboxJobStatus::Queued)
            .map(|j| j.mailbox_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        ids.sort();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::mailbox::MailboxJobOrigin;

    fn make_job(mailbox_id: &str, agent_id: &str) -> MailboxJob {
        MailboxJob {
            job_id: Uuid::now_v7().to_string(),
            mailbox_id: mailbox_id.to_string(),
            agent_id: agent_id.to_string(),
            messages: vec![],
            origin: MailboxJobOrigin::User,
            sender_id: None,
            parent_run_id: None,
            overrides: None,
            priority: 128,
            dedupe_key: None,
            generation: 0,
            status: MailboxJobStatus::Queued,
            available_at: 1000,
            attempt_count: 0,
            max_attempts: 5,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            created_at: 1000,
            updated_at: 1000,
        }
    }

    #[tokio::test]
    async fn enqueue_and_list() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        store.enqueue(&job).await.unwrap();

        let listed = store.list_jobs("m-1", None, 100, 0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, MailboxJobStatus::Queued);
    }

    #[tokio::test]
    async fn claim_returns_queued_job() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].job_id, job_id);
        assert_eq!(claimed[0].status, MailboxJobStatus::Claimed);
        assert!(claimed[0].claim_token.is_some());
    }

    #[tokio::test]
    async fn claim_respects_available_at() {
        let store = InMemoryMailboxStore::new();
        let mut job = make_job("m-1", "agent-1");
        job.available_at = 5000; // future
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 10)
            .await
            .unwrap();
        assert!(claimed.is_empty());

        // Now advance time past available_at.
        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 5000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
    }

    #[tokio::test]
    async fn claim_limit() {
        let store = InMemoryMailboxStore::new();
        for _ in 0..3 {
            store.enqueue(&make_job("m-1", "agent-1")).await.unwrap();
        }

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
    }

    #[tokio::test]
    async fn claim_priority_ordering() {
        let store = InMemoryMailboxStore::new();

        let mut low = make_job("m-1", "agent-1");
        low.priority = 200;
        low.created_at = 900;
        store.enqueue(&low).await.unwrap();

        let mut high = make_job("m-1", "agent-1");
        high.priority = 10;
        high.created_at = 1000;
        store.enqueue(&high).await.unwrap();

        let mut mid = make_job("m-1", "agent-1");
        mid.priority = 128;
        mid.created_at = 950;
        store.enqueue(&mid).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 3);
        assert_eq!(claimed[0].priority, 10);
        assert_eq!(claimed[1].priority, 128);
        assert_eq!(claimed[2].priority, 200);
    }

    #[tokio::test]
    async fn ack_transitions_to_accepted() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store.ack(&job_id, &token, 2000).await.unwrap();

        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::Accepted);
    }

    #[tokio::test]
    async fn ack_rejects_wrong_claim_token() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();

        let result = store.ack(&job_id, "wrong-token", 2000).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn nack_returns_to_queued() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store
            .nack(&job_id, &token, 3000, "transient error", 2000)
            .await
            .unwrap();

        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::Queued);
        assert_eq!(loaded.attempt_count, 1);
        assert_eq!(loaded.available_at, 3000);
        assert!(loaded.claim_token.is_none());
    }

    #[tokio::test]
    async fn nack_dead_letters_after_max_attempts() {
        let store = InMemoryMailboxStore::new();
        let mut job = make_job("m-1", "agent-1");
        job.max_attempts = 1;
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store
            .nack(&job_id, &token, 3000, "final error", 2000)
            .await
            .unwrap();

        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::DeadLetter);
    }

    #[tokio::test]
    async fn dead_letter_is_terminal() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        store
            .dead_letter(&job_id, &token, "permanent failure", 2000)
            .await
            .unwrap();

        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::DeadLetter);
        assert!(loaded.status.is_terminal());
    }

    #[tokio::test]
    async fn cancel_queued_job() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let cancelled = store.cancel(&job_id, 2000).await.unwrap();
        assert!(cancelled.is_some());
        assert_eq!(cancelled.unwrap().status, MailboxJobStatus::Cancelled);

        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::Cancelled);
    }

    #[tokio::test]
    async fn extend_lease_success() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();

        let ok = store
            .extend_lease(&job_id, &token, 60_000, 15_000)
            .await
            .unwrap();
        assert!(ok);

        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        assert_eq!(loaded.lease_until, Some(75_000));
    }

    #[tokio::test]
    async fn extend_lease_wrong_token() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();

        let ok = store
            .extend_lease(&job_id, "wrong-token", 60_000, 15_000)
            .await
            .unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn interrupt_supersedes_queued() {
        let store = InMemoryMailboxStore::new();
        store.enqueue(&make_job("m-1", "agent-1")).await.unwrap();
        store.enqueue(&make_job("m-1", "agent-1")).await.unwrap();

        let result = store.interrupt("m-1", 2000).await.unwrap();
        assert_eq!(result.new_generation, 1);
        assert_eq!(result.superseded_count, 2);
        assert!(result.active_job.is_none());

        let listed = store
            .list_jobs("m-1", Some(&[MailboxJobStatus::Superseded]), 100, 0)
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[tokio::test]
    async fn interrupt_returns_active_claimed() {
        let store = InMemoryMailboxStore::new();
        let job1 = make_job("m-1", "agent-1");
        store.enqueue(&job1).await.unwrap();

        // Claim the first job.
        store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();

        // Enqueue another.
        store.enqueue(&make_job("m-1", "agent-1")).await.unwrap();

        let result = store.interrupt("m-1", 2000).await.unwrap();
        assert!(result.active_job.is_some());
        assert_eq!(result.active_job.unwrap().status, MailboxJobStatus::Claimed);
        // The second (Queued) job should be superseded.
        assert_eq!(result.superseded_count, 1);
    }

    #[tokio::test]
    async fn dedupe_key_rejects_duplicate() {
        let store = InMemoryMailboxStore::new();
        let mut job1 = make_job("m-1", "agent-1");
        job1.dedupe_key = Some("unique-key".to_string());
        store.enqueue(&job1).await.unwrap();

        let mut job2 = make_job("m-1", "agent-1");
        job2.dedupe_key = Some("unique-key".to_string());
        let result = store.enqueue(&job2).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reclaim_expired_leases() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        // Claim with a short lease.
        store
            .claim("m-1", "consumer-1", 100, 1000, 1)
            .await
            .unwrap();

        // Advance time past lease expiry (lease_until = 1100).
        let reclaimed = store.reclaim_expired_leases(2000, 10).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].job_id, job_id);
        assert_eq!(reclaimed[0].status, MailboxJobStatus::Queued);
        assert_eq!(reclaimed[0].attempt_count, 1);
    }

    #[tokio::test]
    async fn purge_terminal() {
        let store = InMemoryMailboxStore::new();

        // Create a job, claim, and ack it (terminal).
        let job = make_job("m-1", "agent-1");
        store.enqueue(&job).await.unwrap();
        let claimed = store
            .claim("m-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        let token = claimed[0].claim_token.as_ref().unwrap().clone();
        store.ack(&claimed[0].job_id, &token, 1500).await.unwrap();

        // Create another non-terminal job.
        store.enqueue(&make_job("m-1", "agent-1")).await.unwrap();

        // Purge terminal jobs older than 2000.
        let purged = store.purge_terminal(2000).await.unwrap();
        assert_eq!(purged, 1);

        // The non-terminal job should remain.
        let listed = store.list_jobs("m-1", None, 100, 0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, MailboxJobStatus::Queued);
    }

    #[tokio::test]
    async fn queued_mailbox_ids() {
        let store = InMemoryMailboxStore::new();
        store.enqueue(&make_job("m-1", "agent-1")).await.unwrap();
        store.enqueue(&make_job("m-2", "agent-1")).await.unwrap();
        store.enqueue(&make_job("m-3", "agent-1")).await.unwrap();

        let ids = store.queued_mailbox_ids().await.unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"m-1".to_string()));
        assert!(ids.contains(&"m-2".to_string()));
        assert!(ids.contains(&"m-3".to_string()));
    }

    #[tokio::test]
    async fn claim_job_by_id() {
        let store = InMemoryMailboxStore::new();
        let job = make_job("m-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        let claimed = store
            .claim_job(&job_id, "consumer-1", 30_000, 1000)
            .await
            .unwrap();
        assert!(claimed.is_some());
        let claimed = claimed.unwrap();
        assert_eq!(claimed.job_id, job_id);
        assert_eq!(claimed.status, MailboxJobStatus::Claimed);
        assert!(claimed.claim_token.is_some());
    }

    // ── Concurrency & parallelism tests ─────────────────────────────

    #[tokio::test]
    async fn fifo_ordering_within_same_priority() {
        let store = InMemoryMailboxStore::new();

        // Enqueue 5 jobs with identical priority but incrementing created_at.
        let mut job_ids = Vec::new();
        for i in 0u64..5 {
            let mut job = make_job("thread-1", "agent-1");
            job.priority = 0;
            job.created_at = 1000 + i;
            job.available_at = 1000;
            job_ids.push(job.job_id.clone());
            store.enqueue(&job).await.unwrap();
        }

        // Claim them one-by-one and verify FIFO order.
        let mut claimed_order = Vec::new();
        for _ in 0..5 {
            let claimed = store
                .claim("thread-1", "consumer-1", 30_000, 1000, 1)
                .await
                .unwrap();
            assert_eq!(claimed.len(), 1, "expected exactly 1 job per claim");
            let job = &claimed[0];
            claimed_order.push(job.job_id.clone());
            // Ack so it becomes terminal and won't be claimed again.
            store
                .ack(&job.job_id, job.claim_token.as_ref().unwrap(), 2000)
                .await
                .unwrap();
        }

        assert_eq!(claimed_order, job_ids, "jobs must be claimed in FIFO order");
    }

    #[tokio::test]
    async fn concurrent_enqueue_no_lost_jobs() {
        let store = std::sync::Arc::new(InMemoryMailboxStore::new());
        let mut handles = Vec::new();

        for i in 0..10 {
            let store = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let mut job = make_job("thread-1", "agent-1");
                job.dedupe_key = Some(format!("dedupe-{i}"));
                store.enqueue(&job).await.unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let listed = store.list_jobs("thread-1", None, 100, 0).await.unwrap();
        assert_eq!(
            listed.len(),
            10,
            "all 10 concurrently enqueued jobs must be present"
        );
    }

    #[tokio::test]
    async fn concurrent_claim_only_one_wins() {
        let store = std::sync::Arc::new(InMemoryMailboxStore::new());

        // Enqueue exactly 1 job.
        let job = make_job("thread-1", "agent-1");
        let job_id = job.job_id.clone();
        store.enqueue(&job).await.unwrap();

        // Use a barrier so all tasks start claiming at roughly the same time.
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(10));
        let mut handles = Vec::new();

        for i in 0..10 {
            let store = std::sync::Arc::clone(&store);
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .claim("thread-1", &format!("consumer-{i}"), 30_000, 1000, 1)
                    .await
                    .unwrap()
            }));
        }

        let mut winners = 0;
        let mut losers = 0;
        for h in handles {
            let claimed = h.await.unwrap();
            if claimed.is_empty() {
                losers += 1;
            } else {
                winners += 1;
                assert_eq!(claimed.len(), 1);
                assert_eq!(claimed[0].job_id, job_id);
            }
        }

        assert_eq!(winners, 1, "exactly one consumer must win the claim");
        assert_eq!(losers, 9, "the other 9 must get empty results");

        // Verify the job has a single claim_token.
        let loaded = store.load_job(&job_id).await.unwrap().unwrap();
        assert_eq!(loaded.status, MailboxJobStatus::Claimed);
        assert!(loaded.claim_token.is_some());
    }

    #[tokio::test]
    async fn claim_respects_per_mailbox_isolation() {
        let store = InMemoryMailboxStore::new();

        let job1 = make_job("thread-1", "agent-1");
        let job1_id = job1.job_id.clone();
        store.enqueue(&job1).await.unwrap();

        let job2 = make_job("thread-2", "agent-1");
        let job2_id = job2.job_id.clone();
        store.enqueue(&job2).await.unwrap();

        // Claim from thread-1.
        let claimed1 = store
            .claim("thread-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed1.len(), 1);
        assert_eq!(claimed1[0].job_id, job1_id);

        // Claim from thread-2 should succeed independently.
        let claimed2 = store
            .claim("thread-2", "consumer-2", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed2.len(), 1);
        assert_eq!(claimed2[0].job_id, job2_id);

        // Both are independently Claimed.
        let loaded1 = store.load_job(&job1_id).await.unwrap().unwrap();
        let loaded2 = store.load_job(&job2_id).await.unwrap().unwrap();
        assert_eq!(loaded1.status, MailboxJobStatus::Claimed);
        assert_eq!(loaded2.status, MailboxJobStatus::Claimed);
        assert_ne!(
            loaded1.claim_token, loaded2.claim_token,
            "each mailbox should get its own claim token"
        );
    }

    #[tokio::test]
    async fn claim_returns_only_one_per_call_with_limit_1() {
        let store = InMemoryMailboxStore::new();

        for _ in 0..3 {
            store
                .enqueue(&make_job("thread-1", "agent-1"))
                .await
                .unwrap();
        }

        let claimed = store
            .claim("thread-1", "consumer-1", 30_000, 1000, 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "limit=1 must return exactly 1 job");

        // Verify remaining 2 are still Queued.
        let queued = store
            .list_jobs("thread-1", Some(&[MailboxJobStatus::Queued]), 100, 0)
            .await
            .unwrap();
        assert_eq!(queued.len(), 2, "remaining 2 jobs must still be Queued");
    }
}
