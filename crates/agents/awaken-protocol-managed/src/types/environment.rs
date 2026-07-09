//! Wire types for the `environments` resource + its work queue
//! (`beta.environments.*` / `beta.environments.work.*`): `BetaEnvironment`, the
//! work item, and the small action responses (delete / queue stats / heartbeat).
//!
//! Pure serde shapes. Polymorphic sub-fields the SDK models as unions — an
//! environment's `config` (`BetaCloudConfig | BetaSelfHostedConfig`) and a work
//! item's `data` (`BetaSessionWorkData | BetaHealthCheckWorkData`) — stay opaque
//! `Value`s. The store, the record→wire projection, and the neutral network-policy
//! mapping live in `routes::environments`.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// `BetaEnvironment` — where a self-hosted worker runs sessions.
#[derive(Debug, Clone, Serialize)]
pub struct Environment {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub name: String,
    pub description: String,
    pub metadata: BTreeMap<String, String>,
    /// `BetaCloudConfig | BetaSelfHostedConfig`.
    pub config: Value,
}

/// `BetaEnvironmentDeleteResponse` — the `DELETE /v1/environments/:id` receipt.
#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentDeleted {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
}

/// A work item on an environment's queue. `data` is the work payload union
/// (`BetaSessionWorkData | BetaHealthCheckWorkData`); `secret` is always `null` on
/// this surface (no lease secret is minted).
#[derive(Debug, Clone, Serialize)]
pub struct Work {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub environment_id: String,
    pub data: Value,
    pub metadata: BTreeMap<String, String>,
    /// `queued` | `starting` | `active` | `stopping` | `stopped`.
    pub state: &'static str,
    pub secret: Option<Value>,
    pub acknowledged_at: Option<String>,
    pub latest_heartbeat_at: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub stop_requested_at: Option<String>,
    pub stopped_at: Option<String>,
}

/// The work queue's depth + pending count (`GET .../work/stats`).
#[derive(Debug, Clone, Serialize)]
pub struct WorkQueueStats {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub depth: usize,
    pub pending: usize,
    pub oldest_queued_at: Option<String>,
    pub workers_polling: i64,
}

/// The heartbeat receipt (`POST .../work/:wid/heartbeat`): lease extended + TTL.
#[derive(Debug, Clone, Serialize)]
pub struct WorkHeartbeat {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub last_heartbeat: &'static str,
    pub lease_extended: bool,
    pub state: &'static str,
    pub ttl_seconds: u64,
}
