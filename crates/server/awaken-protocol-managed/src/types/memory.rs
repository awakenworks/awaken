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
    pub created_by: Option<MemoryActor>,
    pub path: Option<String>,
    pub redacted_at: Option<String>,
    pub redacted_by: Option<MemoryActor>,
}
