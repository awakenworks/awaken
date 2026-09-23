/// Persisted initial-realization retry budget state.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_source_run_id: Option<Box<awaken_agent_contract::agent::run::Id>>,
}
