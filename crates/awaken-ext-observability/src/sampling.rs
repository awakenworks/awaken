//! ADR-0030 D5: per-run sampling decision.

#[derive(Debug, Clone, Copy, Default)]
pub enum SamplingMode {
    #[default]
    Always,
    Never,
    /// Probability in [0.0, 1.0]. Decision is deterministic per run via
    /// a 64-bit hash of the run id; no per-run RNG state.
    Proportional(f32),
}

#[derive(Debug, Clone)]
pub struct SamplingPolicy {
    pub error_traces: SamplingMode,
    pub low_judge_score: SamplingMode,
    /// Reserved: the policy slot exists so an operator-flagged run
    /// (HITL reject, thumbs-down, ad-hoc admin pin) can be force-kept
    /// once a producer surfaces. No call site sets the flag in this
    /// delivery — `RunOutcome.explicit_flag` is hardcoded `false`
    /// at the `RunEnd` evaluation point. Errors and low judge scores
    /// already cover the common promotion cases under the default
    /// policy; this slot is wired but inert until a feedback signal
    /// (HITL or otherwise) is plumbed through.
    pub explicit_flag: SamplingMode,
    pub normal_traces: SamplingMode,
    pub low_judge_threshold: f32,
}

impl Default for SamplingPolicy {
    fn default() -> Self {
        Self {
            error_traces: SamplingMode::Always,
            low_judge_score: SamplingMode::Always,
            explicit_flag: SamplingMode::Always,
            normal_traces: SamplingMode::Proportional(0.01),
            low_judge_threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    pub had_error: bool,
    /// Reserved: see `SamplingPolicy::explicit_flag`. The field is read
    /// by `should_persist` but no producer feeds it today — the
    /// `RunEnd` path in `persistent.rs` constructs this struct with
    /// `explicit_flag: false`. The `explicit_flag_always_persists` test
    /// pins the future routing so the policy slot cannot regress once
    /// a producer is added.
    pub explicit_flag: bool,
    pub judge_score: Option<f32>,
}

pub fn should_persist(policy: &SamplingPolicy, run_id: &str, outcome: &RunOutcome) -> bool {
    if outcome.explicit_flag {
        return decide(policy.explicit_flag, run_id);
    }
    if outcome.had_error {
        return decide(policy.error_traces, run_id);
    }
    if let Some(score) = outcome.judge_score
        && score < policy.low_judge_threshold
    {
        return decide(policy.low_judge_score, run_id);
    }
    decide(policy.normal_traces, run_id)
}

fn decide(mode: SamplingMode, run_id: &str) -> bool {
    match mode {
        SamplingMode::Always => true,
        SamplingMode::Never => false,
        SamplingMode::Proportional(p) => {
            if p >= 1.0 {
                return true;
            }
            if p <= 0.0 {
                return false;
            }
            // Stable 64-bit hash so the same run id always yields the same
            // decision for the same policy.
            let hash = stable_hash(run_id);
            ((hash as f64) / (u64::MAX as f64)) < (p as f64)
        }
    }
}

/// Stable 64-bit hash whose output is deterministic across Rust versions
/// and process restarts. We deliberately avoid `DefaultHasher` because its
/// output is documented to vary between toolchain versions, which would
/// silently reassign sampling buckets after a `cargo update` of the
/// compiler. FNV-1a (64-bit, public-domain) is small enough to inline and
/// stable by construction — adequate for sampling decisions where the
/// only requirement is uniform bucketing, not cryptographic strength.
fn stable_hash(s: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for byte in s.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_flag_always_persists() {
        let policy = SamplingPolicy {
            normal_traces: SamplingMode::Never,
            ..Default::default()
        };
        let outcome = RunOutcome {
            explicit_flag: true,
            ..Default::default()
        };
        assert!(should_persist(&policy, "any", &outcome));
    }

    #[test]
    fn errors_always_persist_under_default_policy() {
        let policy = SamplingPolicy::default();
        let outcome = RunOutcome {
            had_error: true,
            ..Default::default()
        };
        assert!(should_persist(&policy, "any", &outcome));
    }

    #[test]
    fn low_judge_score_always_persists_under_default_policy() {
        let policy = SamplingPolicy::default();
        let outcome = RunOutcome {
            judge_score: Some(0.3),
            ..Default::default()
        };
        assert!(should_persist(&policy, "any", &outcome));
    }

    #[test]
    fn normal_traces_at_zero_proportional_drop() {
        let policy = SamplingPolicy {
            normal_traces: SamplingMode::Proportional(0.0),
            ..Default::default()
        };
        let outcome = RunOutcome::default();
        assert!(!should_persist(&policy, "any", &outcome));
    }

    #[test]
    fn proportional_decision_is_stable_per_run() {
        let policy = SamplingPolicy {
            normal_traces: SamplingMode::Proportional(0.5),
            ..Default::default()
        };
        let outcome = RunOutcome::default();
        let a = should_persist(&policy, "stable-id-1", &outcome);
        let b = should_persist(&policy, "stable-id-1", &outcome);
        assert_eq!(a, b);
    }

    #[test]
    fn stable_hash_pinned_outputs() {
        // Pin the FNV-1a output for known inputs so a future change to the
        // hash function (which would reassign every sampling bucket across
        // a fleet) shows up here as a test failure rather than silent
        // post-deploy behaviour drift.
        assert_eq!(super::stable_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(super::stable_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(super::stable_hash("foobar"), 0x8594_4171_f739_67e8);
    }
}
