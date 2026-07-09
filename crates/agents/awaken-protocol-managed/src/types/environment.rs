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

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `EnvironmentCreateParams` — the `POST /v1/environments` body. `config` is the
/// `BetaCloudConfig | BetaSelfHostedConfig` union (opaque `Value`); absent defaults
/// to `{ type: "self_hosted" }`.
#[derive(Debug, Clone, Deserialize)]
pub struct EnvironmentCreateParams {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub config: Option<Value>,
}

/// `EnvironmentUpdateParams` — a partial update. `name` / `description` / `config`
/// replace when present; `metadata` is a patch where an entry's `null` value
/// removes the key.
#[derive(Debug, Clone, Deserialize)]
pub struct EnvironmentUpdateParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub config: Option<Value>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, Option<String>>>,
}

/// `BetaSelfHostedWorkUpdateRequest` — the `POST .../work/:wid` body: a metadata
/// merge (each present key upserts).
#[derive(Debug, Clone, Deserialize)]
pub struct WorkUpdateParams {
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
}

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
pub struct DeletedEnvironment {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
}

/// `BetaSelfHostedWork` — a work item on an environment's queue. `data` is the
/// work payload union (`BetaSessionWorkData | BetaHealthCheckWorkData`); `secret`
/// is always `null` on this surface (no lease secret is minted).
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

/// `BetaSelfHostedWorkQueueStats` — the queue's depth + pending count
/// (`GET .../work/stats`).
#[derive(Debug, Clone, Serialize)]
pub struct WorkQueueStats {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub depth: usize,
    pub pending: usize,
    pub oldest_queued_at: Option<String>,
    pub workers_polling: i64,
}

/// `BetaSelfHostedWorkHeartbeatResponse` — the heartbeat receipt
/// (`POST .../work/:wid/heartbeat`): lease extended + TTL.
#[derive(Debug, Clone, Serialize)]
pub struct WorkHeartbeat {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub last_heartbeat: &'static str,
    pub lease_extended: bool,
    pub state: &'static str,
    pub ttl_seconds: u64,
}
