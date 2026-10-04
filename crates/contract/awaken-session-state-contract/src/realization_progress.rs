/// Persisted initial-realization retry budget state.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationProgress {
    /// Immutable creation-time request for registered-Worker preparation
    /// before any model Run. Historical Sessions and ordinary interactive
    /// Sessions do not opt in; the root remains the only scheduling truth.
    #[serde(default, skip_serializing_if = "is_false")]
    pub run_free_initial_preparation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_source_run_id: Option<Box<awaken_agent_contract::agent::run::Id>>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::SessionRealizationProgress;

    #[test]
    fn historical_progress_omits_and_defaults_run_free_preparation() {
        // Cause/effect table: S1 historical/default field absent => false and
        // the original wire shape; S2 explicit true => retained on round-trip.
        // E1 prevents old interactive Sessions from gaining new Worker work;
        // E2 preserves the exact opt-in through persistence and recovery.
        let historical = serde_json::json!({"attempts": 0});
        let decoded: SessionRealizationProgress =
            serde_json::from_value(historical.clone()).unwrap();
        assert!(!decoded.run_free_initial_preparation, "S1/E1");
        assert_eq!(serde_json::to_value(decoded).unwrap(), historical, "S1/E1");
        let selected = SessionRealizationProgress {
            run_free_initial_preparation: true,
            ..Default::default()
        };
        let wire = serde_json::to_value(&selected).unwrap();
        assert_eq!(wire["run_free_initial_preparation"], true, "S2/E2");
        assert_eq!(
            serde_json::from_value::<SessionRealizationProgress>(wire).unwrap(),
            selected,
            "S2/E2"
        );
    }
}
