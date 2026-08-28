//! Strong DTOs added by the Managed Memory version-attribution contract.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum MemoryVersionObjectType {
    #[serde(rename = "memory_version")]
    MemoryVersion,
}

/// Authenticated request attribution stamped by the owning IAM edge. This is
/// request context, not a second identity model; its value is the Resources
/// domain's canonical actor type.
#[derive(Debug, Clone)]
pub struct AuthenticatedMemoryActor(pub awaken_resource_contract::MemoryActor);

/// `BetaManagedAgentsActor`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryActor {
    ApiActor { api_key_id: String },
    SessionActor { session_id: String },
    UserActor { user_id: String },
    ServiceAccountActor { service_account_id: String },
}

/// `BetaManagedAgentsMemoryVersionOperation`.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MemoryVersionOperation {
    Created,
    Modified,
    Deleted,
}

/// `BetaManagedAgentsMemoryVersion`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MemoryVersion {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: MemoryVersionObjectType,
    pub created_at: String,
    pub memory_id: String,
    pub memory_store_id: String,
    pub operation: MemoryVersionOperation,
    pub content: Option<String>,
    pub content_sha256: Option<String>,
    pub content_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<MemoryActor>,
    pub path: Option<String>,
    pub redacted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted_by: Option<MemoryActor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_non_nullable_memory_actors_are_absent_instead_of_null() {
        // Official partition: `created_by?`/`redacted_by?` are optional actor
        // objects, unlike nullable content/path/redaction fields. Missing
        // attribution therefore omits both keys; serializing JSON null would
        // be accepted by the permissive SDK runtime but violate its declared
        // response type and the generated real-process shape gate.
        let wire = serde_json::to_value(MemoryVersion {
            id: "memver_1".into(),
            kind: MemoryVersionObjectType::MemoryVersion,
            created_at: "2026-08-28T00:00:00Z".into(),
            memory_id: "mem_1".into(),
            memory_store_id: "memstore_1".into(),
            operation: MemoryVersionOperation::Created,
            content: None,
            content_sha256: None,
            content_size_bytes: None,
            created_by: None,
            path: None,
            redacted_at: None,
            redacted_by: None,
        })
        .expect("MemoryVersion serializes");
        assert!(!wire.as_object().expect("object").contains_key("created_by"));
        assert!(
            !wire
                .as_object()
                .expect("object")
                .contains_key("redacted_by")
        );
        for nullable in [
            "content",
            "content_sha256",
            "content_size_bytes",
            "path",
            "redacted_at",
        ] {
            assert!(
                wire[nullable].is_null(),
                "{nullable} remains optional nullable"
            );
        }
    }
}
