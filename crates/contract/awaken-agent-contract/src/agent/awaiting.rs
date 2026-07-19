//! Same-run awaiting state and the durable resume ticket.
//!
//! When a run awaits (a tool needs a decision, external input, a timer, …) it
//! commits a [`ResumeTicket`]: the structured correlation a later resume must
//! match before the run continues. The ticket is pure agent-domain data so it
//! survives a commit and is validated on resume, never a live handle.

use serde::{Deserialize, Serialize};

/// Why a run is awaiting. A client-executed tool is just one awaiting reason — the
/// design keeps these neutral rather than naming an "external tool" concept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AwaitReason {
    ToolPermission,
    UserInput,
    ExternalEvent,
    RateLimit,
    ManualPause,
    /// The run committed a `ScheduledAction` (ADR-0003 mechanism #1): a deferred
    /// action recorded in committed state, performed by the system (not decided
    /// by a human) and recovered from the committed request for consistency
    /// (ADR-0020).
    ScheduledAction,
    /// A delegated sub-agent needs more input. Its opaque execution reference
    /// is owned by the durable parent/child relationship, not duplicated here.
    Delegation,
}

impl AwaitReason {
    /// The stable snake_case token emitted on the `Awaiting` stream event. Kept
    /// beside the enum so the wire vocabulary has one authoritative source and a
    /// new reason variant forces its token here, not at each await site.
    #[must_use]
    pub fn as_stream_str(&self) -> &'static str {
        match self {
            AwaitReason::ToolPermission => "tool_permission",
            AwaitReason::UserInput => "user_input",
            AwaitReason::ExternalEvent => "external_event",
            AwaitReason::RateLimit => "rate_limit",
            AwaitReason::ManualPause => "manual_pause",
            AwaitReason::ScheduledAction => "scheduled_action",
            AwaitReason::Delegation => "delegation",
        }
    }
}

/// The committed correlation for one same-run pause. A resume is accepted only
/// when its correlation, run/thread, executable snapshot, and catalog
/// fingerprint all match, and the deadline (if any) has not passed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeTicket {
    /// Idempotency/correlation key; deduplicates retries and duplicate wakes.
    pub correlation_id: String,
    pub run_id: crate::agent::run::Id,
    pub thread_id: crate::agent::thread::Id,
    /// Executable snapshot id and catalog fingerprint, proving a resume result
    /// belongs to the same executable configuration (held as plain ids so the
    /// agent-domain contract stays independent of runtime-facing types).
    pub snapshot_id: String,
    pub catalog_fingerprint: String,
    /// Stable origin of this Run. A delegated Run keeps it across every await,
    /// process restart, and resume; a directly admitted Run stores `None`.
    #[serde(default, alias = "initiator", skip_serializing_if = "Option::is_none")]
    pub delegation_origin: Option<crate::agent::delegation::DelegationOrigin>,
    pub reason: AwaitReason,
    /// The tool call awaiting a result, when the wait is a tool decision.
    pub call_id: Option<String>,
    /// The pending tool call, kept so an `allow` decision can execute it on
    /// resume. Held as id + JSON args (not a runtime-facing `ToolCall`).
    #[serde(default)]
    pub pending_tool: Option<PendingTool>,
    /// Optional expiry (epoch millis). A resume after this is stale.
    pub deadline_ms: Option<u64>,
}

/// The tool call a wait is holding, in pure data form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingTool {
    pub tool_id: String,
    pub arguments: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_has_a_distinct_stable_stream_token() {
        let all = [
            (AwaitReason::ToolPermission, "tool_permission"),
            (AwaitReason::UserInput, "user_input"),
            (AwaitReason::ExternalEvent, "external_event"),
            (AwaitReason::RateLimit, "rate_limit"),
            (AwaitReason::ManualPause, "manual_pause"),
            (AwaitReason::ScheduledAction, "scheduled_action"),
            (AwaitReason::Delegation, "delegation"),
        ];
        for (reason, token) in &all {
            assert_eq!(reason.as_stream_str(), *token);
        }
        // Tokens are unique across the closed set.
        let mut tokens: Vec<&str> = all.iter().map(|(_, t)| *t).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), all.len(), "stream tokens must be unique");
    }

    #[test]
    fn pending_tool_and_ticket_round_trip() {
        let pt = PendingTool {
            tool_id: "t".into(),
            arguments: serde_json::json!({"a": 1}),
        };
        // A ticket without a pending_tool round-trips (serde default fills None).
        let ticket = ResumeTicket {
            correlation_id: "c".into(),
            run_id: crate::agent::run::Id("r".into()),
            thread_id: crate::agent::thread::Id("th".into()),
            snapshot_id: "s".into(),
            catalog_fingerprint: "f".into(),
            delegation_origin: None,
            reason: AwaitReason::ToolPermission,
            call_id: Some("call".into()),
            pending_tool: Some(pt),
            deadline_ms: Some(42),
        };
        let back: ResumeTicket =
            serde_json::from_str(&serde_json::to_string(&ticket).unwrap()).unwrap();
        assert_eq!(back, ticket);
    }
}
