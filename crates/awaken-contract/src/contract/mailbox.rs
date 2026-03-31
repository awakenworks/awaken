//! Mailbox job types and persistent store trait for the unified run queue.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::message::Message;
use super::storage::StorageError;

// ── MailboxJobStatus ────────────────────────────────────────────────

/// Six-state lifecycle for a mailbox job.
///
/// ```text
/// Queued ──claim──> Claimed ──ack──> Accepted (terminal)
///   |                  |
///   |               nack(retry) ──> Queued (attempt_count++, available_at = retry_at)
///   |                  |
///   |               nack(permanent) ──> DeadLetter (terminal)
///   |
///   |── cancel ──> Cancelled (terminal)
///   └── interrupt(generation bump) ──> Superseded (terminal)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxJobStatus {
    Queued,
    Claimed,
    Accepted,
    Cancelled,
    Superseded,
    DeadLetter,
}

impl MailboxJobStatus {
    /// Returns `true` for terminal states that cannot transition further.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Accepted | Self::Cancelled | Self::Superseded | Self::DeadLetter
        )
    }
}

// ── MailboxJobOrigin ────────────────────────────────────────────────

/// Origin of a mailbox job request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxJobOrigin {
    /// HTTP API, SDK.
    User,
    /// Agent-to-Agent protocol.
    A2A,
    /// Child run completion notification, handoff.
    Internal,
}

// ── MailboxJob ──────────────────────────────────────────────────────

/// A run request persisted in the mailbox queue.
///
/// Every run — streaming, background, A2A, internal notification —
/// enters the system as a MailboxJob keyed by thread_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxJob {
    // ── identity ──
    /// UUID v7, globally unique.
    pub job_id: String,
    /// Thread ID, routing anchor (`mailbox_id = thread_id`).
    pub mailbox_id: String,

    // ── request payload ──
    /// Target agent.
    pub agent_id: String,
    /// User/system messages.
    pub messages: Vec<Message>,
    /// Where this job originated.
    pub origin: MailboxJobOrigin,
    /// Audit / reply routing.
    pub sender_id: Option<String>,
    /// Parent-child run linkage.
    pub parent_run_id: Option<String>,
    /// Opaque RunRequest extras (overrides, decisions, frontend_tools, etc.)
    /// serialized as JSON. Mailbox does not inspect this; it is passed
    /// through to `spawn_execution` which reconstructs the full RunRequest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_extras: Option<Value>,

    // ── queue semantics ──
    /// 0 = highest, 255 = lowest, default 128.
    pub priority: u8,
    /// Idempotent delivery key.
    pub dedupe_key: Option<String>,
    /// Mailbox generation (set by store on enqueue).
    pub generation: u64,

    // ── lifecycle ──
    /// Current status.
    pub status: MailboxJobStatus,
    /// Unix millis; future value = delayed delivery.
    pub available_at: u64,
    /// Number of claim attempts so far.
    pub attempt_count: u32,
    /// Maximum attempts before dead-lettering (default 5).
    pub max_attempts: u32,
    /// Last error message.
    pub last_error: Option<String>,

    // ── lease ──
    /// UUID set on claim.
    pub claim_token: Option<String>,
    /// Consumer identifier (process) that claimed this job.
    pub claimed_by: Option<String>,
    /// Unix millis, extended by heartbeat.
    pub lease_until: Option<u64>,

    // ── timestamps ──
    /// Unix millis when the job was created.
    pub created_at: u64,
    /// Unix millis of the last update.
    pub updated_at: u64,
}

// ── MailboxInterrupt ────────────────────────────────────────────────

/// Result of a mailbox interrupt operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxInterrupt {
    /// New generation after bump.
    pub new_generation: u64,
    /// The job that was Claimed (running) at interrupt time, if any.
    /// Caller should cancel the corresponding runtime run.
    pub active_job: Option<MailboxJob>,
    /// Number of Queued jobs superseded.
    pub superseded_count: usize,
}

// ── MailboxStore trait ──────────────────────────────────────────────

/// Persistent mailbox queue with lease-based distributed claim.
///
/// Implementations must guarantee:
/// - enqueue is durable before returning
/// - claim is atomic (exactly one consumer wins)
/// - interrupt atomically bumps generation + supersedes stale jobs
/// - ack/nack/dead_letter validate claim_token (reject stale claims)
#[async_trait]
pub trait MailboxStore: Send + Sync {
    // ── write path ──

    /// Persist a job. Sets generation from current mailbox state
    /// (auto-creates state if first job for this mailbox_id).
    /// Rejects if dedupe_key matches an existing non-terminal job.
    async fn enqueue(&self, job: &MailboxJob) -> Result<(), StorageError>;

    /// Atomically claim up to `limit` Queued jobs for a mailbox
    /// where `available_at <= now`. Sets status=Claimed, claim_token,
    /// claimed_by, lease_until = now + lease_ms.
    /// Returns claimed jobs ordered by (priority ASC, created_at ASC).
    async fn claim(
        &self,
        mailbox_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError>;

    /// Claim a specific job by job_id. Same semantics as `claim()`
    /// but targets a single known job (used for inline streaming).
    async fn claim_job(
        &self,
        job_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<MailboxJob>, StorageError>;

    /// Mark job as successfully processed. Validates claim_token.
    async fn ack(&self, job_id: &str, claim_token: &str, now: u64) -> Result<(), StorageError>;

    /// Return job to queue for retry. Sets available_at = retry_at,
    /// increments attempt_count, records error.
    /// If attempt_count >= max_attempts, transitions to DeadLetter instead.
    async fn nack(
        &self,
        job_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Permanently fail a job. Terminal state.
    async fn dead_letter(
        &self,
        job_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Cancel a specific job. Works on Queued jobs only.
    /// For Claimed jobs, caller must also cancel the runtime run.
    async fn cancel(&self, job_id: &str, now: u64) -> Result<Option<MailboxJob>, StorageError>;

    /// Extend an active lease. Returns false if job not Claimed
    /// or claim_token mismatch (lease already expired and reclaimed).
    async fn extend_lease(
        &self,
        job_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError>;

    // ── interrupt ──

    /// Atomically: bump generation, supersede all Queued jobs
    /// with generation < new_generation, return the Claimed job
    /// (if any) so caller can cancel its runtime run.
    async fn interrupt(&self, mailbox_id: &str, now: u64)
    -> Result<MailboxInterrupt, StorageError>;

    // ── read path ──

    /// Load a single job by ID.
    async fn load_job(&self, job_id: &str) -> Result<Option<MailboxJob>, StorageError>;

    /// List jobs for a mailbox, filtered by status.
    async fn list_jobs(
        &self,
        mailbox_id: &str,
        status_filter: Option<&[MailboxJobStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MailboxJob>, StorageError>;

    // ── maintenance ──

    /// Reclaim jobs whose lease_until < now (orphaned by crashed consumers).
    /// Resets to Queued with incremented attempt_count.
    /// Returns reclaimed jobs for immediate dispatch.
    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<MailboxJob>, StorageError>;

    /// Purge terminal jobs (Accepted, Cancelled, Superseded, DeadLetter)
    /// older than `older_than` timestamp. Returns count purged.
    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError>;

    /// List distinct mailbox_ids that have at least one Queued job.
    /// Used by recover() at startup.
    async fn queued_mailbox_ids(&self) -> Result<Vec<String>, StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_terminal_returns_true_for_terminal_states() {
        assert!(MailboxJobStatus::Accepted.is_terminal());
        assert!(MailboxJobStatus::Cancelled.is_terminal());
        assert!(MailboxJobStatus::Superseded.is_terminal());
        assert!(MailboxJobStatus::DeadLetter.is_terminal());
    }

    #[test]
    fn is_terminal_returns_false_for_non_terminal_states() {
        assert!(!MailboxJobStatus::Queued.is_terminal());
        assert!(!MailboxJobStatus::Claimed.is_terminal());
    }

    fn make_mailbox_job() -> MailboxJob {
        MailboxJob {
            job_id: "job-001".to_string(),
            mailbox_id: "thread-abc".to_string(),
            agent_id: "agent-1".to_string(),
            messages: vec![Message::user("hello")],
            origin: MailboxJobOrigin::User,
            sender_id: Some("user-42".to_string()),
            parent_run_id: None,
            overrides: Some(serde_json::json!({"temperature": 0.7})),
            priority: 128,
            dedupe_key: Some("req-xyz".to_string()),
            generation: 1,
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

    #[test]
    fn mailbox_job_serde_roundtrip() {
        let job = make_mailbox_job();
        let json = serde_json::to_string(&job).unwrap();
        let parsed: MailboxJob = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.job_id, "job-001");
        assert_eq!(parsed.mailbox_id, "thread-abc");
        assert_eq!(parsed.agent_id, "agent-1");
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.origin, MailboxJobOrigin::User);
        assert_eq!(parsed.sender_id.as_deref(), Some("user-42"));
        assert!(parsed.parent_run_id.is_none());
        assert!(parsed.overrides.is_some());
        assert_eq!(parsed.priority, 128);
        assert_eq!(parsed.dedupe_key.as_deref(), Some("req-xyz"));
        assert_eq!(parsed.generation, 1);
        assert_eq!(parsed.status, MailboxJobStatus::Queued);
        assert_eq!(parsed.available_at, 1000);
        assert_eq!(parsed.attempt_count, 0);
        assert_eq!(parsed.max_attempts, 5);
        assert!(parsed.last_error.is_none());
        assert!(parsed.claim_token.is_none());
        assert!(parsed.claimed_by.is_none());
        assert!(parsed.lease_until.is_none());
        assert_eq!(parsed.created_at, 1000);
        assert_eq!(parsed.updated_at, 1000);
    }

    #[test]
    fn mailbox_job_status_serde_roundtrip() {
        for status in [
            MailboxJobStatus::Queued,
            MailboxJobStatus::Claimed,
            MailboxJobStatus::Accepted,
            MailboxJobStatus::Cancelled,
            MailboxJobStatus::Superseded,
            MailboxJobStatus::DeadLetter,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: MailboxJobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn mailbox_job_origin_serde_roundtrip() {
        for origin in [
            MailboxJobOrigin::User,
            MailboxJobOrigin::A2A,
            MailboxJobOrigin::Internal,
        ] {
            let json = serde_json::to_string(&origin).unwrap();
            let parsed: MailboxJobOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, origin);
        }
    }

    #[test]
    fn mailbox_interrupt_serde_roundtrip() {
        let interrupt = MailboxInterrupt {
            new_generation: 5,
            active_job: Some(make_mailbox_job()),
            superseded_count: 3,
        };
        let json = serde_json::to_string(&interrupt).unwrap();
        let parsed: MailboxInterrupt = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.new_generation, 5);
        assert!(parsed.active_job.is_some());
        assert_eq!(parsed.superseded_count, 3);
    }
}
