use crate::io::RunRequest;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Durable status for a queued background run request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxEntryStatus {
    Queued,
    Claimed,
    Accepted,
    Superseded,
    Cancelled,
    DeadLetter,
}

impl MailboxEntryStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Accepted | Self::Superseded | Self::Cancelled | Self::DeadLetter
        )
    }
}

/// A durable thread-scoped queued input waiting to become a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEntry {
    pub entry_id: String,
    pub thread_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub generation: u64,
    pub status: MailboxEntryStatus,
    pub request: RunRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// Envelope-level message type tag for routing and priority dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Identity of the sender (agent_id or run_id) for audit and reply routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    /// Dispatch priority (higher = dispatched first). Default 0.
    #[serde(default)]
    pub priority: u8,
    pub available_at: u64,
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_run_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl MailboxEntry {
    pub fn is_claimable(&self, now: u64) -> bool {
        match self.status {
            MailboxEntryStatus::Queued => self.available_at <= now,
            MailboxEntryStatus::Claimed => self.lease_until.is_some_and(|lease| lease <= now),
            MailboxEntryStatus::Accepted
            | MailboxEntryStatus::Superseded
            | MailboxEntryStatus::Cancelled
            | MailboxEntryStatus::DeadLetter => false,
        }
    }
}

/// Durable thread-scoped mailbox control state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxThreadState {
    pub thread_id: String,
    pub current_generation: u64,
    pub updated_at: u64,
}

/// Result of bumping thread mailbox generation and superseding older entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxThreadInterrupt {
    pub thread_state: MailboxThreadState,
    pub superseded_entries: Vec<MailboxEntry>,
}

/// Query options for listing queued inputs.
#[derive(Debug, Clone, Default)]
pub struct MailboxQuery {
    pub thread_id: Option<String>,
    pub run_id: Option<String>,
    pub status: Option<MailboxEntryStatus>,
    pub offset: usize,
    pub limit: usize,
}

/// Paginated mailbox view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxPage {
    pub items: Vec<MailboxEntry>,
    pub total: usize,
    pub has_more: bool,
}

pub fn paginate_mailbox_entries(entries: &[MailboxEntry], query: &MailboxQuery) -> MailboxPage {
    let mut filtered: Vec<MailboxEntry> = entries
        .iter()
        .filter(|entry| match query.thread_id.as_deref() {
            Some(thread_id) => entry.thread_id == thread_id,
            None => true,
        })
        .filter(|entry| match query.run_id.as_deref() {
            Some(run_id) => entry.run_id == run_id,
            None => true,
        })
        .filter(|entry| match query.status {
            Some(status) => entry.status == status,
            None => true,
        })
        .cloned()
        .collect();

    filtered.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });

    let total = filtered.len();
    let limit = query.limit.clamp(1, 200);
    let offset = query.offset.min(total);
    let end = (offset + limit + 1).min(total);
    let slice = &filtered[offset..end];
    let has_more = slice.len() > limit;
    let items = slice.iter().take(limit).cloned().collect();

    MailboxPage {
        items,
        total,
        has_more,
    }
}

/// Mailbox persistence errors.
#[derive(Debug, Error)]
pub enum MailboxStoreError {
    #[error("mailbox entry not found: {0}")]
    NotFound(String),

    #[error("mailbox entry already exists: {0}")]
    AlreadyExists(String),

    #[error("mailbox claim token mismatch for entry: {0}")]
    ClaimConflict(String),

    #[error("mailbox generation mismatch for thread '{thread_id}': expected {expected}, got {actual}")]
    GenerationMismatch {
        thread_id: String,
        expected: u64,
        actual: u64,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("backend error: {0}")]
    Backend(String),
}
