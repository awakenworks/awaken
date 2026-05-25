//! Serializable model pool: a named set of model offerings that behaves like
//! a single model to agents, with sticky per-session routing and failover.
//!
//! Carved out of `registry_spec/mod.rs` so the file stays under the
//! repository's per-file line cap. Public types are re-exported from
//! `registry_spec` so import paths remain unchanged.
//!
//! A pool is referenced by `AgentSpec.model_id` exactly where a `ModelSpec`
//! id would be. To the runtime it resolves to a single `LlmExecutor`, so the
//! run loop, streaming, retry, and context-window clamping all treat it
//! identically to a plain model. Each agent is routed to one stable "home"
//! member (prompt-cache affinity); the active member is held for the duration
//! of a session and only changes on sustained failure or quota pressure.

use serde::{Deserialize, Serialize};

/// A named pool of member models, addressable like a single model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPoolSpec {
    /// Stable id, unique across the combined model + pool id namespace.
    pub id: String,
    /// Ordered set of member models. Must be non-empty.
    pub members: Vec<PoolMemberSpec>,
    /// Home-selection and stickiness policy.
    #[serde(default)]
    pub routing: PoolRoutingPolicy,
    /// When the pool abandons the active member for another.
    #[serde(default)]
    pub switch: PoolSwitchPolicy,
}

/// One member of a [`ModelPoolSpec`], referencing a `ModelSpec` by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolMemberSpec {
    /// References a `ModelSpec.id` in the same registry.
    pub model_id: String,
    /// Relative selection weight for home distribution. `None` is treated
    /// as `1`. Must be greater than zero when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// Whether the member is a home candidate or a failover-only target.
    #[serde(default)]
    pub role: PoolMemberRole,
}

/// Eligibility of a pool member for initial home selection.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PoolMemberRole {
    /// Eligible as both a home target and a failover target.
    #[default]
    Member,
    /// Never selected as home; used only after failover from other members.
    FailoverOnly,
}

/// How a session picks its initial member and how long that choice is held.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolRoutingPolicy {
    /// Strategy for choosing the home member at session start.
    #[serde(default)]
    pub home: HomeStrategy,
    /// Lifetime over which the active member is held.
    #[serde(default)]
    pub sticky_scope: StickyScope,
}

/// Strategy for choosing a session's home member.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HomeStrategy {
    /// Stable hash of the routing key over healthy members: the same agent
    /// always homes to the same member (prompt-cache affinity) while
    /// different agents spread across the pool.
    #[default]
    Deterministic,
    /// Assign homes round-robin as sessions start. Spreads load but provides
    /// no cache affinity across process restarts.
    RoundRobin,
    /// Always home to the first healthy member in declaration order.
    FirstHealthy,
}

/// Lifetime over which a session holds its active member.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StickyScope {
    /// Hold routing for the lifetime of a thread (conversation), maximizing
    /// within-conversation prompt-cache reuse across runs.
    #[default]
    Thread,
    /// Hold routing only for a single run.
    Run,
}

/// When the pool abandons the active member for another one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoolSwitchPolicy {
    /// Switch when the active member's circuit breaker is open (sustained
    /// failure). Transient single-request errors are absorbed by the
    /// member's own retry policy and never trigger a switch.
    #[serde(default = "default_true")]
    pub on_circuit_open: bool,
    /// Switch on rate-limit / overload (quota) signals.
    #[serde(default = "default_true")]
    pub on_quota: bool,
    /// Only treat a quota signal as switch-worthy when the provider's
    /// retry-after hint meets or exceeds this many seconds. `None` switches
    /// on any quota signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_retry_after_threshold_secs: Option<u64>,
    /// Switch on permanent member errors (unauthorized, model-not-found).
    #[serde(default = "default_true")]
    pub on_permanent: bool,
    /// Cap on consecutive member switches within one failure incident for a
    /// session. A successful request or cleanly drained stream resets this
    /// budget so long-lived threads can recover from future independent
    /// incidents. `None` is unbounded (still bounded by the number of members
    /// for any single call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_switches_per_session: Option<u32>,
}

impl Default for PoolSwitchPolicy {
    fn default() -> Self {
        Self {
            on_circuit_open: true,
            on_quota: true,
            quota_retry_after_threshold_secs: None,
            on_permanent: true,
            max_switches_per_session: None,
        }
    }
}

fn default_true() -> bool {
    true
}

impl ModelPoolSpec {
    /// Convenience constructor for tests and bootstrap code. Routing and
    /// switch policies default; members are taken as `Member`-role with no
    /// explicit weight.
    pub fn new<I, S>(id: impl Into<String>, member_model_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            id: id.into(),
            members: member_model_ids
                .into_iter()
                .map(|model_id| PoolMemberSpec {
                    model_id: model_id.into(),
                    weight: None,
                    role: PoolMemberRole::Member,
                })
                .collect(),
            routing: PoolRoutingPolicy::default(),
            switch: PoolSwitchPolicy::default(),
        }
    }
}
