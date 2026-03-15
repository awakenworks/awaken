use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::RunOrigin;

/// Durable status for a queued mailbox entry.
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

/// Coarse-grained ingress origin for mailbox entries.
///
/// This is intentionally separate from thread message visibility:
/// - origin = who submitted the queued work item
/// - visibility = which messages are exposed to external API consumers
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxEntryOrigin {
    /// Submitted from an external protocol or end-user API surface.
    #[default]
    External,
    /// Submitted from internal orchestration/runtime code paths.
    Internal,
}

impl MailboxEntryOrigin {
    /// Classify a run origin into coarse mailbox origin buckets.
    pub fn from_run_origin(origin: RunOrigin) -> Self {
        match origin {
            RunOrigin::Subagent | RunOrigin::Internal => Self::Internal,
            RunOrigin::User | RunOrigin::AgUi | RunOrigin::AiSdk | RunOrigin::A2a => Self::External,
        }
    }
}

/// A durable queued message in a mailbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEntry {
    pub entry_id: String,
    /// Target mailbox address.
    pub mailbox_id: String,
    /// Coarse ingress origin classification for routing and visibility defaults.
    #[serde(default)]
    pub origin: MailboxEntryOrigin,
    /// Identity of the sender for audit and reply routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    /// Opaque message payload — receiver interprets.
    pub payload: Value,
    /// Dispatch priority (higher = dispatched first). Default 0.
    #[serde(default)]
    pub priority: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    pub generation: u64,
    pub status: MailboxEntryStatus,
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

/// Durable mailbox-scoped control state (generation tracking).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxState {
    pub mailbox_id: String,
    pub current_generation: u64,
    pub updated_at: u64,
}

/// Result of bumping mailbox generation and superseding older entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxInterrupt {
    pub mailbox_state: MailboxState,
    pub superseded_entries: Vec<MailboxEntry>,
}

/// Query options for listing mailbox entries.
#[derive(Debug, Clone, Default)]
pub struct MailboxQuery {
    pub mailbox_id: Option<String>,
    pub origin: Option<MailboxEntryOrigin>,
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
        .filter(|entry| match query.mailbox_id.as_deref() {
            Some(mailbox_id) => entry.mailbox_id == mailbox_id,
            None => true,
        })
        .filter(|entry| match query.origin {
            Some(origin) => entry.origin == origin,
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

/// Check whether any entry in the given collection holds an active (non-expired)
/// claim for `mailbox_id`, optionally excluding `exclude_entry_id`.
pub fn has_active_claim_for_mailbox<'a>(
    entries: impl IntoIterator<Item = &'a MailboxEntry>,
    mailbox_id: &str,
    now: u64,
    exclude_entry_id: Option<&str>,
) -> bool {
    entries.into_iter().any(|e| {
        e.mailbox_id == mailbox_id
            && e.status == MailboxEntryStatus::Claimed
            && e.lease_until.is_some_and(|l| l > now)
            && exclude_entry_id.map_or(true, |eid| e.entry_id != eid)
    })
}

/// Outcome of a receiver processing a mailbox entry.
#[derive(Debug, Clone)]
pub enum ReceiveOutcome {
    /// Message processed successfully.
    Accepted,
    /// Transient failure — retry later.
    Retry(String),
    /// Permanent failure — dead-letter.
    Reject(String),
}

/// Pluggable consumer for mailbox entries.
#[async_trait]
pub trait MailboxReceiver: Send + Sync {
    async fn receive(&self, entry: &MailboxEntry) -> ReceiveOutcome;
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

    #[error(
        "mailbox generation mismatch for mailbox '{mailbox_id}': expected {expected}, got {actual}"
    )]
    GenerationMismatch {
        mailbox_id: String,
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
