use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Action that triggered an audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    Create,
    Update,
    Delete,
    Restart,
    Publish,
}

/// A self-contained audit record for a single admin action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// ULID string — monotonically increasing, serves as the storage key.
    pub id: String,
    /// RFC 3339 timestamp.
    pub ts: String,
    /// SHA-256 prefix of the bearer token, or `"anonymous"`.
    pub actor: String,
    /// Action that was performed.
    pub action: AuditAction,
    /// Resource path in the form `<namespace>/<id>`.
    pub resource: String,
    /// Payload before the change (null for create / restart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    /// Payload after the change (null for delete / restart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
    /// Client IP address derived from request headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// Value of the `X-Request-Id` header if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn audit_event_serde_round_trip() {
        let event = AuditEvent {
            id: "01ABCDEFGH".to_string(),
            ts: "2026-05-01T00:00:00Z".to_string(),
            actor: "deadbeef01234567".to_string(),
            action: AuditAction::Create,
            resource: "agents/my-agent".to_string(),
            before: None,
            after: Some(json!({"id": "my-agent"})),
            ip: Some("127.0.0.1".to_string()),
            request_id: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn optional_fields_omitted_when_none() {
        let event = AuditEvent {
            id: "01ABCDEFGH".to_string(),
            ts: "2026-05-01T00:00:00Z".to_string(),
            actor: "anonymous".to_string(),
            action: AuditAction::Delete,
            resource: "agents/old-agent".to_string(),
            before: None,
            after: None,
            ip: None,
            request_id: None,
        };

        let value = serde_json::to_value(&event).unwrap();
        assert!(value.get("before").is_none(), "before should be omitted");
        assert!(value.get("after").is_none(), "after should be omitted");
        assert!(value.get("ip").is_none(), "ip should be omitted");
        assert!(
            value.get("request_id").is_none(),
            "request_id should be omitted"
        );
    }

    #[test]
    fn action_snake_case_serialization() {
        assert_eq!(
            serde_json::to_value(AuditAction::Create).unwrap(),
            json!("create")
        );
        assert_eq!(
            serde_json::to_value(AuditAction::Update).unwrap(),
            json!("update")
        );
        assert_eq!(
            serde_json::to_value(AuditAction::Delete).unwrap(),
            json!("delete")
        );
        assert_eq!(
            serde_json::to_value(AuditAction::Restart).unwrap(),
            json!("restart")
        );
        assert_eq!(
            serde_json::to_value(AuditAction::Publish).unwrap(),
            json!("publish")
        );
    }

    #[test]
    fn action_round_trip_from_str() {
        for (s, expected) in [
            ("create", AuditAction::Create),
            ("update", AuditAction::Update),
            ("delete", AuditAction::Delete),
            ("restart", AuditAction::Restart),
            ("publish", AuditAction::Publish),
        ] {
            let parsed: AuditAction = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(parsed, expected);
        }
    }
}
