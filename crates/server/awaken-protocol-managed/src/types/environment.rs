//! Wire types for the `environments` resource + its work queue
//! (`beta.environments.*` / `beta.environments.work.*`): `BetaEnvironment`, the
//! work item, and the small action responses (delete / queue stats / heartbeat).
//!
//! Pure serde shapes. The Environment config is the exact Anthropic tagged union;
//! Awaken execution policy is deliberately not accepted in this wire object.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct AllowedHost(String);

impl AllowedHost {
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for AllowedHost {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        let host = raw.trim().to_ascii_lowercase();
        let dns = host.strip_prefix("*.").unwrap_or(&host);
        let valid = !dns.is_empty()
            && dns.len() <= 253
            && !dns.contains("..")
            && dns.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid {
            return Err(serde::de::Error::custom(
                "allowed_hosts entries must be hostnames or `*.example.com` patterns without scheme, port, or path",
            ));
        }
        Ok(Self(host))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct PackageSpec(String);

impl PackageSpec {
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for PackageSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty()
            || value.trim() != value
            || value.starts_with('-')
            || value.chars().any(char::is_control)
        {
            return Err(serde::de::Error::custom(
                "package requirements must be non-empty values and cannot be command options",
            ));
        }
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentConfigParams {
    Cloud {
        #[serde(default)]
        networking: Option<CloudNetworkingParams>,
        #[serde(default)]
        packages: Option<Box<PackagesParams>>,
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
        allowed_hosts: Option<Vec<AllowedHost>>,
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
    pub apt: Option<Vec<PackageSpec>>,
    #[serde(default)]
    pub cargo: Option<Vec<PackageSpec>>,
    #[serde(default)]
    pub gem: Option<Vec<PackageSpec>>,
    #[serde(default)]
    pub go: Option<Vec<PackageSpec>>,
    #[serde(default)]
    pub npm: Option<Vec<PackageSpec>>,
    #[serde(default)]
    pub pip: Option<Vec<PackageSpec>>,
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
/// replace when present; an explicit nullable `description` clears the value,
/// while omission preserves the authored value.
/// `metadata` is a patch where an entry's `null` value removes the key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentUpdateParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub config: Option<Option<EnvironmentConfigUpdateParams>>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    pub scope: Option<EnvironmentScope>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentConfigUpdateParams {
    Cloud {
        #[serde(default, deserialize_with = "super::presence::double_option")]
        networking: Option<Option<CloudNetworkingUpdateParams>>,
        #[serde(default, deserialize_with = "super::presence::double_option")]
        packages: Option<Option<PackagesUpdateParams>>,
    },
    SelfHosted {},
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudNetworkingUpdateParams {
    Unrestricted,
    Limited {
        #[serde(default, deserialize_with = "super::presence::double_option")]
        allowed_hosts: Option<Option<Vec<AllowedHost>>>,
        #[serde(default, deserialize_with = "super::presence::double_option")]
        allow_mcp_servers: Option<Option<bool>>,
        #[serde(default, deserialize_with = "super::presence::double_option")]
        allow_package_managers: Option<Option<bool>>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackagesUpdateParams {
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub apt: Option<Option<Vec<PackageSpec>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub cargo: Option<Option<Vec<PackageSpec>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub gem: Option<Option<Vec<PackageSpec>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub go: Option<Option<Vec<PackageSpec>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub npm: Option<Option<Vec<PackageSpec>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub pip: Option<Option<Vec<PackageSpec>>>,
    #[serde(rename = "type", default)]
    pub kind: Option<PackagesKind>,
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
/// merge (a string upserts and null deletes the key).
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkUpdateParams {
    pub metadata: BTreeMap<String, Option<String>>,
}

/// `BetaSelfHostedWorkStopRequest`.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkStopParams {
    #[serde(default)]
    pub force: Option<bool>,
}

/// `BetaWorkSecret` before base64url encoding into `BetaSelfHostedWork.secret`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkSecret {
    pub sessions_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
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
    pub description: Option<String>,
    pub metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub config: awaken_environment_contract::EnvironmentConfig,
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type")]
pub enum WorkData {
    #[serde(rename = "healthcheck")]
    HealthCheck { id: String },
    #[serde(rename = "session")]
    Session { id: String },
}

/// `BetaSelfHostedWork` — a work item on an environment's queue. `secret` is
/// present only on the one Session poll response that acquired the lease;
/// list/retrieve and HealthCheck projections keep it `null`.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Queued,
    Starting,
    Active,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WorkObjectType {
    #[serde(rename = "work")]
    Work,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WorkQueueStatsObjectType {
    #[serde(rename = "work_queue_stats")]
    WorkQueueStats,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum WorkHeartbeatObjectType {
    #[serde(rename = "work_heartbeat")]
    WorkHeartbeat,
}

/// A response field that is required on the wire while its value may be JSON
/// `null`. Keeping this distinct from `Option<T>` prevents generated schemas
/// from confusing Anthropic's `field: T | null` with `field?: T | null`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(inline))]
#[serde(transparent)]
pub struct RequiredNullable<T>(pub Option<T>);

impl<T> From<Option<T>> for RequiredNullable<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Work {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: WorkObjectType,
    pub environment_id: String,
    pub data: WorkData,
    pub metadata: BTreeMap<String, String>,
    pub state: WorkState,
    pub secret: RequiredNullable<String>,
    pub acknowledged_at: RequiredNullable<String>,
    pub latest_heartbeat_at: RequiredNullable<String>,
    pub created_at: String,
    pub started_at: RequiredNullable<String>,
    pub stop_requested_at: RequiredNullable<String>,
    pub stopped_at: RequiredNullable<String>,
}

/// `BetaSelfHostedWorkQueueStats` — the queue's depth + pending count
/// (`GET .../work/stats`).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkQueueStats {
    #[serde(rename = "type")]
    pub object_type: WorkQueueStatsObjectType,
    pub depth: usize,
    pub pending: usize,
    pub oldest_queued_at: RequiredNullable<String>,
    pub workers_polling: RequiredNullable<i64>,
}

/// `BetaSelfHostedWorkHeartbeatResponse` — the heartbeat receipt
/// (`POST .../work/:wid/heartbeat`): lease extended + TTL.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkHeartbeat {
    #[serde(rename = "type")]
    pub object_type: WorkHeartbeatObjectType,
    pub last_heartbeat: String,
    pub lease_extended: bool,
    pub state: WorkState,
    pub ttl_seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_environment_wire_remains_exactly_anthropic_owned() {
        // Cause/effect graph: official SDK JSON -> protocol-managed tagged
        // union -> neutral Environment config. Awaken SandboxExecutionPolicy is
        // a sibling /v1/awaken API and must never enter this wire object.
        // Decision table: R1 official cloud networking/packages create input ->
        // accepted; R2 requests or limits in create -> reject unknown field; R3
        // the same private fields in partial update -> reject; R4 serialized
        // official response contains only the Managed Environment config and no
        // policy/request/limit projection. Together these pin both ingress DTOs
        // and the response shape used by an unmodified Anthropic SDK.
        let official = serde_json::json!({
            "name": "browser",
            "description": "managed environment",
            "metadata": {"owner": "design"},
            "scope": "organization",
            "config": {
                "type": "cloud",
                "networking": {"type": "unrestricted"},
                "packages": {"type": "packages", "npm": ["playwright@1.54.1"]}
            }
        });
        assert!(
            serde_json::from_value::<EnvironmentCreateParams>(official).is_ok(),
            "R1"
        );

        for private_field in ["requests", "limits"] {
            let mut create = serde_json::json!({
                "name": "browser",
                "config": {"type": "cloud"}
            });
            create["config"][private_field] = serde_json::json!({"cpu_millis": 500});
            assert!(
                serde_json::from_value::<EnvironmentCreateParams>(create).is_err(),
                "R2 {private_field}"
            );

            let mut update = serde_json::json!({"config": {"type": "cloud"}});
            update["config"][private_field] = serde_json::json!({"memory_bytes": 1073741824u64});
            assert!(
                serde_json::from_value::<EnvironmentUpdateParams>(update).is_err(),
                "R3 {private_field}"
            );
        }

        let response = Environment {
            id: "env_1".into(),
            object_type: "environment",
            archived_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            name: "browser".into(),
            description: Some("managed environment".into()),
            metadata: BTreeMap::new(),
            scope: Some("organization".into()),
            config: awaken_environment_contract::EnvironmentConfig::Cloud {
                networking: Default::default(),
                packages: Default::default(),
            },
        };
        let encoded = serde_json::to_value(response).unwrap();
        assert_eq!(encoded["config"]["type"], "cloud", "R4");
        assert!(encoded["config"].get("requests").is_none(), "R4");
        assert!(encoded["config"].get("limits").is_none(), "R4");
        assert!(encoded.get("sandbox_policy").is_none(), "R4");
    }
}
