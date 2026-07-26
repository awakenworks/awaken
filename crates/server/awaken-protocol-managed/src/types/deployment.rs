//! Wire types for the `deployments` + `deployment-runs` resources
//! (`beta.deployments.*` / `beta.deploymentRuns.*`): `BetaManagedAgentsDeployment`
//! and `BetaManagedAgentsDeploymentRun`.
//!
//! Pure serde shapes. Every statically-known SDK union is decoded here; only
//! content blocks and rubric bodies reuse their existing neutral/typed owners.

use std::collections::BTreeMap;

use awaken_agent_contract::agent::content::ContentBlock;
use serde::{Deserialize, Serialize};

use crate::types::agent::AgentReference;
use crate::types::resource::ResourceInput;
use crate::types::session::{AgentRef, InboundEvent, OutcomeRubric};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum DeploymentInitialEvent {
    #[serde(rename = "user.message")]
    UserMessage { content: Vec<ContentBlock> },
    #[serde(rename = "system.message")]
    SystemMessage { content: Vec<ContentBlock> },
    #[serde(rename = "user.define_outcome")]
    UserDefineOutcome {
        description: String,
        rubric: OutcomeRubric,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u32>,
    },
}

impl From<DeploymentInitialEvent> for InboundEvent {
    fn from(value: DeploymentInitialEvent) -> Self {
        match value {
            DeploymentInitialEvent::UserMessage { content } => InboundEvent::UserMessage {
                content,
                session_thread_id: None,
                model: None,
            },
            DeploymentInitialEvent::SystemMessage { content } => {
                InboundEvent::SystemMessage { content }
            }
            DeploymentInitialEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            } => InboundEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    Cron {
        expression: String,
        timezone: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_run_at: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        upcoming_runs_at: Vec<String>,
    },
}

impl Schedule {
    pub fn expression(&self) -> &str {
        match self {
            Schedule::Cron { expression, .. } => expression,
        }
    }

    pub fn with_last_run_at(&self, last: Option<String>) -> Self {
        match self {
            Schedule::Cron {
                expression,
                timezone,
                upcoming_runs_at,
                ..
            } => Schedule::Cron {
                expression: expression.clone(),
                timezone: timezone.clone(),
                last_run_at: last,
                upcoming_runs_at: upcoming_runs_at.clone(),
            },
        }
    }
}

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
    pub initial_events: Vec<DeploymentInitialEvent>,
    #[serde(default)]
    pub resources: Vec<ResourceInput>,
    #[serde(default)]
    pub schedule: Option<Schedule>,
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
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub metadata: Option<Option<BTreeMap<String, Option<String>>>>,
    #[serde(default)]
    pub initial_events: Option<Vec<DeploymentInitialEvent>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub resources: Option<Option<Vec<ResourceInput>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub schedule: Option<Option<Schedule>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub vault_ids: Option<Option<Vec<String>>>,
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
    pub initial_events: Vec<DeploymentInitialEvent>,
    pub metadata: BTreeMap<String, String>,
    pub name: String,
    pub paused_reason: Option<PausedReason>,
    pub resources: Vec<ResourceInput>,
    pub schedule: Option<Schedule>,
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
