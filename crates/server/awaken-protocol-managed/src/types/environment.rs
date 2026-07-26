//! Wire types for the `environments` resource + its work queue
//! (`beta.environments.*` / `beta.environments.work.*`): `BetaEnvironment`, the
//! work item, and the small action responses (delete / queue stats / heartbeat).
//!
//! Pure serde shapes. The Environment config is the exact Anthropic tagged union;
//! Awaken execution policy is deliberately not accepted in this wire object.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentConfigParams {
    Cloud {
        #[serde(default)]
        networking: Option<CloudNetworkingParams>,
        #[serde(default)]
        packages: Option<PackagesParams>,
    },
    SelfHosted {},
}

impl Default for EnvironmentConfigParams {
    fn default() -> Self {
        Self::SelfHosted {}
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudNetworkingParams {
    Unrestricted,
    Limited {
        #[serde(default)]
        allowed_hosts: Option<Vec<String>>,
        #[serde(default)]
        allow_mcp_servers: Option<bool>,
        #[serde(default)]
        allow_package_managers: Option<bool>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackagesParams {
    #[serde(default)]
    pub apt: Option<Vec<String>>,
    #[serde(default)]
    pub cargo: Option<Vec<String>>,
    #[serde(default)]
    pub gem: Option<Vec<String>>,
    #[serde(default)]
    pub go: Option<Vec<String>>,
    #[serde(default)]
    pub npm: Option<Vec<String>>,
    #[serde(default)]
    pub pip: Option<Vec<String>>,
    #[serde(rename = "type", default)]
    pub kind: Option<PackagesKind>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackagesKind {
    Packages,
}

/// `EnvironmentCreateParams` — the `POST /v1/environments` body. `config` is the
/// `BetaCloudConfig | BetaSelfHostedConfig` union (opaque `Value`); absent defaults
/// to `{ type: "self_hosted" }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentCreateParams {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub scope: Option<EnvironmentScope>,
    #[serde(default)]
    pub config: Option<EnvironmentConfigParams>,
}

/// `EnvironmentUpdateParams` — a partial update. `name` / `description` / `config`
/// replace when present; `metadata` is a patch where an entry's `null` value
/// removes the key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentUpdateParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub config: Option<EnvironmentConfigParams>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    pub scope: Option<EnvironmentScope>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentScope {
    Organization,
    Account,
}

impl EnvironmentScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Account => "account",
        }
    }
}

/// `BetaSelfHostedWorkUpdateRequest` — the `POST .../work/:wid` body: a metadata
/// merge (each present key upserts).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
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

/// A work item's payload — `BetaHealthCheckWorkData | BetaSessionWorkData`. A fresh
/// environment is seeded with a `healthcheck`; a session assigned to a self-hosted
/// environment is enqueued as `session` work (its `id` is the session id).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum WorkData {
    #[serde(rename = "healthcheck")]
    HealthCheck { id: String },
    #[serde(rename = "session")]
    Session { id: String },
}

/// `BetaSelfHostedWork` — a work item on an environment's queue. `secret` (a lease
/// token) is always `null` here (no lease secret is minted).
#[derive(Debug, Clone, Serialize)]
pub struct Work {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub environment_id: String,
    pub data: WorkData,
    pub metadata: BTreeMap<String, String>,
    /// `queued` | `starting` | `active` | `stopping` | `stopped`.
    pub state: &'static str,
    pub secret: Option<String>,
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
    pub last_heartbeat: String,
    pub lease_extended: bool,
    pub state: &'static str,
    pub ttl_seconds: u64,
}
