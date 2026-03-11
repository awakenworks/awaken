use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::runtime::RunStatus;

/// Origin of a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    /// End-user initiated run.
    #[default]
    User,
    /// Internal sub-agent delegated run.
    Subagent,
    /// AG-UI protocol initiated run.
    AgUi,
    /// AI SDK protocol initiated run.
    AiSdk,
    /// A2A protocol initiated run.
    A2a,
    /// Other internal origin.
    Internal,
}

/// Durable projection record for one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub thread_id: String,
    /// The agent definition that owns this run.
    #[serde(default)]
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(default)]
    pub origin: RunOrigin,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_detail: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mailbox_entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl RunRecord {
    /// Create a new run record with `created_at = updated_at = now_ms`.
    pub fn new(
        run_id: impl Into<String>,
        thread_id: impl Into<String>,
        agent_id: impl Into<String>,
        origin: RunOrigin,
        status: RunStatus,
        now_ms: u64,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            thread_id: thread_id.into(),
            agent_id: agent_id.into(),
            parent_run_id: None,
            parent_thread_id: None,
            origin,
            status,
            termination_code: None,
            termination_detail: None,
            created_at: now_ms,
            updated_at: now_ms,
            source_mailbox_entry_id: None,
            metadata: None,
        }
    }
}

/// Pagination/filter query for run listing.
#[derive(Debug, Clone)]
pub struct RunQuery {
    pub offset: usize,
    pub limit: usize,
    pub thread_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub status: Option<RunStatus>,
    pub termination_code: Option<String>,
    pub origin: Option<RunOrigin>,
    /// Inclusive lower bound for `created_at` (unix millis).
    pub created_at_from: Option<u64>,
    /// Inclusive upper bound for `created_at` (unix millis).
    pub created_at_to: Option<u64>,
    /// Inclusive lower bound for `updated_at` (unix millis).
    pub updated_at_from: Option<u64>,
    /// Inclusive upper bound for `updated_at` (unix millis).
    pub updated_at_to: Option<u64>,
}

impl Default for RunQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            thread_id: None,
            parent_run_id: None,
            status: None,
            termination_code: None,
            origin: None,
            created_at_from: None,
            created_at_to: None,
            updated_at_from: None,
            updated_at_to: None,
        }
    }
}

/// Paginated run list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPage {
    pub items: Vec<RunRecord>,
    pub total: usize,
    pub has_more: bool,
}

/// In-memory run pagination helper.
pub fn paginate_runs_in_memory(records: &[RunRecord], query: &RunQuery) -> RunPage {
    let limit = query.limit.clamp(1, 200);
    let mut filtered: Vec<RunRecord> = records
        .iter()
        .filter(|record| {
            if let Some(thread_id) = query.thread_id.as_deref() {
                record.thread_id == thread_id
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(parent_run_id) = query.parent_run_id.as_deref() {
                record.parent_run_id.as_deref() == Some(parent_run_id)
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(status) = query.status {
                record.status == status
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(termination_code) = query.termination_code.as_deref() {
                record.termination_code.as_deref() == Some(termination_code)
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(origin) = query.origin {
                record.origin == origin
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(from) = query.created_at_from {
                record.created_at >= from
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(to) = query.created_at_to {
                record.created_at <= to
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(from) = query.updated_at_from {
                record.updated_at >= from
            } else {
                true
            }
        })
        .filter(|record| {
            if let Some(to) = query.updated_at_to {
                record.updated_at <= to
            } else {
                true
            }
        })
        .cloned()
        .collect();

    filtered.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.run_id.cmp(&b.run_id))
    });

    let total = filtered.len();
    let offset = query.offset.min(total);
    let end = (offset + limit + 1).min(total);
    let slice = &filtered[offset..end];
    let has_more = slice.len() > limit;
    let items = slice.iter().take(limit).cloned().collect();

    RunPage {
        items,
        total,
        has_more,
    }
}

/// Run storage-level errors.
#[derive(Debug, Error)]
pub enum RunStoreError {
    #[error("run store backend error: {0}")]
    Backend(String),
}

impl From<std::io::Error> for RunStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Backend(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paginate_runs_filters_and_pages() {
        let mut records = Vec::new();
        for i in 0..6 {
            records.push(RunRecord::new(
                format!("run-{i}"),
                if i < 4 { "thread-a" } else { "thread-b" },
                "test-agent",
                RunOrigin::User,
                if i % 2 == 0 {
                    RunStatus::Running
                } else {
                    RunStatus::Done
                },
                i as u64,
            ));
        }

        let page = paginate_runs_in_memory(
            &records,
            &RunQuery {
                limit: 2,
                thread_id: Some("thread-a".to_string()),
                status: Some(RunStatus::Running),
                ..Default::default()
            },
        );
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
        assert!(!page.has_more);

        records[2].termination_code = Some("cancel_requested".to_string());
        records[4].termination_code = Some("cancel_requested".to_string());

        let page = paginate_runs_in_memory(
            &records,
            &RunQuery {
                termination_code: Some("cancel_requested".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);

        let page = paginate_runs_in_memory(
            &records,
            &RunQuery {
                created_at_from: Some(2),
                created_at_to: Some(4),
                updated_at_from: Some(2),
                updated_at_to: Some(4),
                ..Default::default()
            },
        );
        assert_eq!(page.total, 3);
        assert_eq!(page.items.len(), 3);
    }
}
