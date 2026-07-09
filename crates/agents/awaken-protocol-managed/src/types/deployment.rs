//! Wire types for the `deployments` + `deployment-runs` resources
//! (`beta.deployments.*` / `beta.deploymentRuns.*`): `BetaManagedAgentsDeployment`
//! and `BetaManagedAgentsDeploymentRun`.
//!
//! Pure serde shapes. Genuinely polymorphic sub-fields the SDK models as rich
//! unions (the agent reference, the schedule, the paused-reason union, the trigger
//! context, initial events, resources) stay opaque `Value`s — exactly as the
//! session shapes keep `resources`/`stats`/`usage` — because reproducing every
//! union buys nothing this single-machine surface exercises. The store and the
//! record→wire projection live in `routes::deployments`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::agent::AgentReference;
use crate::types::session::AgentRef;

/// `DeploymentCreateParams` — the `POST /v1/deployments` body. `agent` is the
/// client input reference (id string or `{id, version?}`); the composite fields
/// the SDK models as unions (`initial_events`, `resources`, `schedule`) stay opaque
/// `Value`s.
#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentCreateParams {
    pub agent: AgentRef,
    pub environment_id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub initial_events: Vec<Value>,
    #[serde(default)]
    pub resources: Vec<Value>,
    #[serde(default)]
    pub schedule: Option<Value>,
    #[serde(default)]
    pub vault_ids: Vec<String>,
}

/// `DeploymentUpdateParams` — a partial update; every field replaces when present.
#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentUpdateParams {
    #[serde(default)]
    pub agent: Option<AgentRef>,
    #[serde(default)]
    pub environment_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub initial_events: Option<Vec<Value>>,
    #[serde(default)]
    pub resources: Option<Vec<Value>>,
    #[serde(default)]
    pub schedule: Option<Value>,
    #[serde(default)]
    pub vault_ids: Option<Vec<String>>,
}

/// `BetaManagedAgentsTriggerContext` — why a deployment run started. This surface
/// only mints manual runs; `Schedule` is modeled for wire completeness.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerContext {
    Manual,
    Schedule { scheduled_at: String },
}

/// `BetaManagedAgentsDeploymentPausedReason` — why a deployment is paused. This
/// surface only pauses manually (`BetaManagedAgentsManualDeploymentPausedReason`);
/// the auto-pause error union is not reproduced (never emitted here).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PausedReason {
    Manual,
}

/// `BetaManagedAgentsRunError` — a run's terminal error. Runs never fail on this
/// single-machine surface, so this is never constructed; it types
/// [`DeploymentRun::error`] (always `null` here) rather than leaving it `Value`.
#[derive(Debug, Clone, Serialize)]
pub struct RunError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// `BetaManagedAgentsDeployment` — an agent bound to an environment with initial
/// events and a schedule.
#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub agent: AgentReference,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub description: Option<String>,
    pub environment_id: String,
    pub initial_events: Vec<Value>,
    pub metadata: BTreeMap<String, String>,
    pub name: String,
    pub paused_reason: Option<PausedReason>,
    pub resources: Vec<Value>,
    /// `BetaManagedAgentsSchedule`, or `null`.
    pub schedule: Option<Value>,
    /// `"active"` | `"paused"`.
    pub status: &'static str,
    pub vault_ids: Vec<String>,
}

/// `BetaManagedAgentsDeploymentRun` — one triggered run of a deployment.
#[derive(Debug, Clone, Serialize)]
pub struct DeploymentRun {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub agent: AgentReference,
    pub created_at: String,
    pub deployment_id: String,
    pub error: Option<RunError>,
    pub session_id: Option<String>,
    pub trigger_context: TriggerContext,
}
