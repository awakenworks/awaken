//! The webhook event wire shape, byte-compatible with Claude Managed Agents:
//! a thin envelope carrying the event `type` + object `id`, and — crucially for
//! the org/workspace alignment (ADR-0048) — both `organization_id` and
//! `workspace_id` stamped in `data`, projected from the session's persisted owner
//! (S3). The receiver GETs the full object by id; the event carries only the key.
//!
//! ```json
//! { "type": "event", "id": "event_01ABC", "created_at": "…",
//!   "data": { "type": "session.status_idled", "id": "sesn_01XYZ",
//!             "organization_id": "org_…", "workspace_id": "wrkspc_…" } }
//! ```

use serde::{Deserialize, Serialize};

/// The `data` block: which resource changed, and the tenancy it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookEventData {
    /// The event kind, e.g. `session.status_idled` (the `OutboundKind` wire name).
    #[serde(rename = "type")]
    pub event_type: String,
    /// The id of the object the event is about (session id, agent id, …).
    pub id: String,
    /// The owning organization — present only in a cloud deployment with a real
    /// Org (ADR-0048 D4); omitted self-hosted, where the chain roots at workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    /// The owning workspace — always present; the session's persisted owner (S3).
    pub workspace_id: String,
}

/// The event envelope delivered to a subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// Always `"event"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The unique event id (`event_…`) — also the `webhook-id` delivery header, so
    /// a receiver dedupes at-least-once retries by it.
    pub id: String,
    /// RFC-3339 creation time.
    pub created_at: String,
    pub data: WebhookEventData,
}

impl WebhookEvent {
    /// Build an event for `event_type` about `object_id`, owned by `workspace_id`
    /// (and optionally `organization_id`).
    pub fn new(
        id: impl Into<String>,
        created_at: impl Into<String>,
        event_type: impl Into<String>,
        object_id: impl Into<String>,
        workspace_id: impl Into<String>,
        organization_id: Option<String>,
    ) -> Self {
        Self {
            kind: "event".to_string(),
            id: id.into(),
            created_at: created_at.into(),
            data: WebhookEventData {
                event_type: event_type.into(),
                id: object_id.into(),
                organization_id,
                workspace_id: workspace_id.into(),
            },
        }
    }

    /// The canonical JSON body that is both delivered and signed.
    pub fn to_body(&self) -> String {
        serde_json::to_string(self).expect("webhook event serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_event_omits_org_and_keeps_workspace() {
        let ev = WebhookEvent::new(
            "event_1",
            "2026-07-09T00:00:00Z",
            "session.status_idled",
            "sesn_1",
            "wrkspc_acme",
            None,
        );
        let v: serde_json::Value = serde_json::from_str(&ev.to_body()).unwrap();
        assert_eq!(v["type"], "event");
        assert_eq!(v["data"]["type"], "session.status_idled");
        assert_eq!(v["data"]["workspace_id"], "wrkspc_acme");
        assert!(
            v["data"].get("organization_id").is_none(),
            "self-hosted omits org"
        );
    }

    #[test]
    fn cloud_event_stamps_org_and_workspace() {
        let ev = WebhookEvent::new(
            "event_2",
            "2026-07-09T00:00:00Z",
            "agent.created",
            "agent_1",
            "wrkspc_acme",
            Some("org_root".to_string()),
        );
        let v: serde_json::Value = serde_json::from_str(&ev.to_body()).unwrap();
        assert_eq!(v["data"]["organization_id"], "org_root");
        assert_eq!(v["data"]["workspace_id"], "wrkspc_acme");
    }
}
