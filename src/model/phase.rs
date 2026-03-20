use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    RunStart,
    StepStart,
    BeforeInference,
    AfterInference,
    BeforeToolExecute,
    AfterToolExecute,
    StepEnd,
    RunEnd,
}

impl Phase {
    /// All phases in execution order.
    pub const ALL: [Phase; 8] = [
        Phase::RunStart,
        Phase::StepStart,
        Phase::BeforeInference,
        Phase::AfterInference,
        Phase::BeforeToolExecute,
        Phase::AfterToolExecute,
        Phase::StepEnd,
        Phase::RunEnd,
    ];

    /// Whether this phase runs once per run (not per step).
    pub fn is_run_level(self) -> bool {
        matches!(self, Phase::RunStart | Phase::RunEnd)
    }

    /// Whether this phase runs within the step loop.
    pub fn is_step_level(self) -> bool {
        !self.is_run_level()
    }

    /// Return mutation policy for this phase.
    pub const fn policy(self) -> PhasePolicy {
        match self {
            Self::BeforeInference => PhasePolicy {
                allow_tool_filter_mutation: true,
                allow_run_action_mutation: true,
                allow_tool_gate_mutation: false,
            },
            Self::AfterInference => PhasePolicy {
                allow_tool_filter_mutation: false,
                allow_run_action_mutation: true,
                allow_tool_gate_mutation: false,
            },
            Self::BeforeToolExecute => PhasePolicy {
                allow_tool_filter_mutation: false,
                allow_run_action_mutation: false,
                allow_tool_gate_mutation: true,
            },
            Self::RunStart
            | Self::StepStart
            | Self::AfterToolExecute
            | Self::StepEnd
            | Self::RunEnd => PhasePolicy::read_only(),
        }
    }
}

/// Mutation policy enforced for each phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhasePolicy {
    /// Whether tool filtering can be mutated.
    pub allow_tool_filter_mutation: bool,
    /// Whether run action (continue/terminate) can be mutated.
    pub allow_run_action_mutation: bool,
    /// Whether tool execution gate (blocked/pending) can be mutated.
    pub allow_tool_gate_mutation: bool,
}

impl PhasePolicy {
    pub const fn read_only() -> Self {
        Self {
            allow_tool_filter_mutation: false,
            allow_run_action_mutation: false,
            allow_tool_gate_mutation: false,
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::RunStart => write!(f, "RunStart"),
            Phase::StepStart => write!(f, "StepStart"),
            Phase::BeforeInference => write!(f, "BeforeInference"),
            Phase::AfterInference => write!(f, "AfterInference"),
            Phase::BeforeToolExecute => write!(f, "BeforeToolExecute"),
            Phase::AfterToolExecute => write!(f, "AfterToolExecute"),
            Phase::StepEnd => write!(f, "StepEnd"),
            Phase::RunEnd => write!(f, "RunEnd"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_all_has_8_variants() {
        assert_eq!(Phase::ALL.len(), 8);
    }

    #[test]
    fn phase_all_order_matches_lifecycle() {
        let order = Phase::ALL;
        assert_eq!(order[0], Phase::RunStart);
        assert_eq!(order[1], Phase::StepStart);
        assert_eq!(order[2], Phase::BeforeInference);
        assert_eq!(order[3], Phase::AfterInference);
        assert_eq!(order[4], Phase::BeforeToolExecute);
        assert_eq!(order[5], Phase::AfterToolExecute);
        assert_eq!(order[6], Phase::StepEnd);
        assert_eq!(order[7], Phase::RunEnd);
    }

    #[test]
    fn phase_run_level_vs_step_level() {
        assert!(Phase::RunStart.is_run_level());
        assert!(Phase::RunEnd.is_run_level());
        assert!(!Phase::RunStart.is_step_level());

        for phase in [
            Phase::StepStart,
            Phase::BeforeInference,
            Phase::AfterInference,
            Phase::BeforeToolExecute,
            Phase::AfterToolExecute,
            Phase::StepEnd,
        ] {
            assert!(phase.is_step_level(), "{phase} should be step-level");
            assert!(!phase.is_run_level(), "{phase} should not be run-level");
        }
    }

    #[test]
    fn phase_serde_roundtrip() {
        for phase in Phase::ALL {
            let json = serde_json::to_string(&phase).unwrap();
            let parsed: Phase = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, phase);
        }
    }

    #[test]
    fn phase_display() {
        assert_eq!(Phase::StepStart.to_string(), "StepStart");
        assert_eq!(Phase::BeforeInference.to_string(), "BeforeInference");
    }

    #[test]
    fn phase_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&Phase::StepStart).unwrap(),
            "\"step_start\""
        );
        assert_eq!(
            serde_json::to_string(&Phase::BeforeToolExecute).unwrap(),
            "\"before_tool_execute\""
        );
    }

    #[test]
    fn phase_display_all_variants() {
        let expected = [
            "RunStart",
            "StepStart",
            "BeforeInference",
            "AfterInference",
            "BeforeToolExecute",
            "AfterToolExecute",
            "StepEnd",
            "RunEnd",
        ];
        for (phase, name) in Phase::ALL.iter().zip(expected.iter()) {
            assert_eq!(phase.to_string(), *name);
        }
    }

    #[test]
    fn phase_equality_and_hash() {
        assert_eq!(Phase::RunStart, Phase::RunStart);
        assert_ne!(Phase::RunStart, Phase::RunEnd);

        let mut set = std::collections::HashSet::new();
        for phase in Phase::ALL {
            assert!(set.insert(phase), "duplicate phase: {phase}");
        }
        assert_eq!(set.len(), 8);
    }

    #[test]
    fn phase_clone() {
        let phase = Phase::BeforeInference;
        let cloned = phase;
        assert_eq!(phase, cloned);
    }

    #[test]
    fn phase_policy_before_inference_allows_tool_filter_and_run_action() {
        let p = Phase::BeforeInference.policy();
        assert!(p.allow_tool_filter_mutation);
        assert!(p.allow_run_action_mutation);
        assert!(!p.allow_tool_gate_mutation);
    }

    #[test]
    fn phase_policy_after_inference_allows_run_action_only() {
        let p = Phase::AfterInference.policy();
        assert!(!p.allow_tool_filter_mutation);
        assert!(p.allow_run_action_mutation);
        assert!(!p.allow_tool_gate_mutation);
    }

    #[test]
    fn phase_policy_before_tool_execute_allows_gate_only() {
        let p = Phase::BeforeToolExecute.policy();
        assert!(!p.allow_tool_filter_mutation);
        assert!(!p.allow_run_action_mutation);
        assert!(p.allow_tool_gate_mutation);
    }

    #[test]
    fn phase_policy_read_only_phases() {
        let read_only_phases = [
            Phase::RunStart,
            Phase::StepStart,
            Phase::AfterToolExecute,
            Phase::StepEnd,
            Phase::RunEnd,
        ];
        for phase in read_only_phases {
            let p = phase.policy();
            assert_eq!(p, PhasePolicy::read_only(), "{phase} should be read-only");
        }
    }
}
