use crate::runtime::phase::step::StepContext;
use crate::runtime::phase::Phase;

/// Unified action trait for all phase effects.
///
/// Actions are emitted by [`AgentBehavior`](super::behavior::AgentBehavior)
/// hooks and tool effects, then applied to [`StepContext`] by the runtime
/// after validation. Extension plugins and tools can define their own actions
/// by implementing this trait.
pub trait Action: Send + 'static {
    /// Human-readable label for diagnostics.
    fn label(&self) -> &'static str;

    /// Check whether this action is valid in the given phase.
    ///
    /// Returns `Ok(())` if allowed, or `Err(reason)` describing the
    /// violation. The default implementation accepts all phases.
    fn validate(&self, _phase: Phase) -> Result<(), String> {
        Ok(())
    }

    /// Apply this action to the mutable step context.
    ///
    /// Consumes `self` (boxed) so that actions can move data into the
    /// step context without cloning.
    fn apply(self: Box<Self>, step: &mut StepContext<'_>);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyAction;

    impl Action for DummyAction {
        fn label(&self) -> &'static str {
            "dummy"
        }

        fn validate(&self, phase: Phase) -> Result<(), String> {
            if phase == Phase::RunStart {
                Err("not allowed in RunStart".into())
            } else {
                Ok(())
            }
        }

        fn apply(self: Box<Self>, _step: &mut StepContext<'_>) {
            // no-op for testing
        }
    }

    #[test]
    fn action_label() {
        let action = DummyAction;
        assert_eq!(action.label(), "dummy");
    }

    #[test]
    fn action_validate_ok() {
        let action = DummyAction;
        assert!(action.validate(Phase::BeforeInference).is_ok());
    }

    #[test]
    fn action_validate_err() {
        let action = DummyAction;
        assert!(action.validate(Phase::RunStart).is_err());
    }
}
