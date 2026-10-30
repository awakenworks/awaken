/// Persisted initial-realization retry budget state.
///
/// Aggregate assignment operations own changes; readers retain the original
/// serialized observations, including historical unknowns. The containing
/// aggregate exposes only a shared view. Whole-root deserialization remains
/// the historical import boundary, not a repository mutation capability.
///
/// The immutable observations and the original decoder remain usable:
/// ```
/// use awaken_session_contract::SessionRealizationProgress;
/// let progress: SessionRealizationProgress = serde_json::from_str(r#"{"attempts":0}"#)?;
/// assert!(!progress.run_free_initial_preparation());
/// assert_eq!(progress.attempts(), 0);
/// assert_eq!(progress.last_error(), None);
/// assert_eq!(progress.failed_at_unix_ms(), None);
/// assert_eq!(progress.failure_source_run_id(), None);
/// assert_eq!(progress.uncounted_failures(), 0);
/// # Ok::<(), serde_json::Error>(())
/// ```
/// Each direct write is rejected independently (W4: a valid imported/default
/// value does not confer an aggregate assignment capability):
/// ```compile_fail,E0616
/// # use awaken_session_contract::SessionRealizationProgress;
/// let mut progress = SessionRealizationProgress::default();
/// progress.run_free_initial_preparation = true;
/// ```
/// ```compile_fail,E0616
/// # use awaken_session_contract::SessionRealizationProgress;
/// let mut progress = SessionRealizationProgress::default();
/// progress.attempts = 0;
/// ```
/// ```compile_fail,E0616
/// # use awaken_session_contract::SessionRealizationProgress;
/// let mut progress = SessionRealizationProgress::default();
/// progress.last_error = None;
/// ```
/// ```compile_fail,E0616
/// # use awaken_session_contract::SessionRealizationProgress;
/// let mut progress = SessionRealizationProgress::default();
/// progress.failed_at_unix_ms = None;
/// ```
/// ```compile_fail,E0616
/// # use awaken_session_contract::SessionRealizationProgress;
/// let mut progress = SessionRealizationProgress::default();
/// progress.failure_source_run_id = None;
/// ```
/// ```compile_fail,E0616
/// # use awaken_session_contract::SessionRealizationProgress;
/// let mut progress = SessionRealizationProgress::default();
/// progress.uncounted_failures = 0;
/// ```
/// A struct literal cannot bypass the assignment owner either:
/// ```compile_fail,E0451
/// # use awaken_session_contract::SessionRealizationProgress;
/// let progress = SessionRealizationProgress { attempts: 1, ..Default::default() };
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationProgress {
    /// Immutable creation-time request for registered-Worker preparation
    /// before any model Run. Historical Sessions and ordinary interactive
    /// Sessions do not opt in; the root remains the only scheduling truth.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) run_free_initial_preparation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failed_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub(crate) attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failure_source_run_id: Option<Box<awaken_agent_contract::agent::run::Id>>,
    /// Consecutive failed effects of assignments that counted no initial
    /// attempt (Idle Run-free preparation, Running/Rescheduling recovery).
    /// Absent on historical roots and omitted while zero, so their wire shape
    /// is unchanged. A counted claim or published realization readiness resets it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) uncounted_failures: u32,
}

impl SessionRealizationProgress {
    /// The stored request flag, independent of current preparation eligibility.
    #[must_use]
    pub const fn run_free_initial_preparation(&self) -> bool {
        self.run_free_initial_preparation
    }

    /// Counted initial attempts after any accepted capacity refund.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The original failure cause, if one was recorded.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Historical absence stays unknown; reading never invents a time.
    #[must_use]
    pub const fn failed_at_unix_ms(&self) -> Option<u64> {
        self.failed_at_unix_ms
    }

    /// Consecutive failed effects of uncounted (recovery or Run-free)
    /// assignments since the last counted claim or acknowledged realization.
    #[must_use]
    pub const fn uncounted_failures(&self) -> u32 {
        self.uncounted_failures
    }

    /// The original optional failure Run, without transferring ownership.
    #[must_use]
    pub fn failure_source_run_id(&self) -> Option<&awaken_agent_contract::agent::run::Id> {
        self.failure_source_run_id.as_deref()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &u32) -> bool {
    *value == 0
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

    #[test]
    fn progress_readers_preserve_historical_wire_and_failure_coordinates() {
        // W3/W4 compatibility decision table. C1 absent/true request;
        // C2 zero/MAX historical attempts; C3 absent/present failure cause;
        // C4 unknown/known-zero/known-positive failure time;
        // C5 absent/exact source Run. The 48-row product is unconstrained at
        // this DTO decoder: invalid aggregate/projection combinations must
        // still reach their existing validation boundary (including AF7).
        // E1 readers expose the exact observations without mutation;
        // E2 original serde omission/field names/values round-trip unchanged.
        // Known zero must stay distinct from unknown; no clock or new claim
        // is involved. Existing assignment tests own legal transitions and
        // count/refund overflow rejection, rather than duplicating that law.
        for requested in [false, true] {
            for attempts in [0, u32::MAX] {
                for error in [None, Some("original failure")] {
                    for failed_at in [None, Some(0_u64), Some(100)] {
                        for source in [None, Some("original-run")] {
                            let mut wire = serde_json::json!({ "attempts": attempts });
                            if requested {
                                wire["run_free_initial_preparation"] = true.into();
                            }
                            if let Some(error) = error {
                                wire["last_error"] = error.into();
                            }
                            if let Some(failed_at) = failed_at {
                                wire["failed_at_unix_ms"] = failed_at.into();
                            }
                            if let Some(source) = source {
                                wire["failure_source_run_id"] = source.into();
                            }
                            let progress: SessionRealizationProgress =
                                serde_json::from_value(wire.clone()).unwrap();
                            assert_eq!(progress.run_free_initial_preparation(), requested, "E1");
                            assert_eq!(progress.attempts(), attempts, "E1");
                            assert_eq!(progress.last_error(), error, "E1");
                            assert_eq!(progress.failed_at_unix_ms(), failed_at, "E1");
                            assert_eq!(
                                progress.failure_source_run_id().map(|run| run.0.as_str()),
                                source,
                                "E1 exact source"
                            );
                            assert_eq!(serde_json::to_value(progress).unwrap(), wire, "E2");
                        }
                    }
                }
            }
        }
    }
}
