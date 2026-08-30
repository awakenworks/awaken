//! Aggregate-owned physical Environment binding transitions.

use super::PersistedSession;

impl PersistedSession {
    /// Apply the one aggregate-owned physical Environment binding transition.
    /// Terminal state is checked beside execution/disposition authority before
    /// the Environment reducer can observe a create/adopt receipt.
    pub fn transition_environment_binding(
        &mut self,
        receipt: &crate::SessionEnvironmentReceipt,
    ) -> Result<bool, crate::SessionEnvironmentTransitionError> {
        if self.is_terminal() {
            return Err(crate::SessionEnvironmentTransitionError::BindingAfterTerminal);
        }
        self.environment.apply_binding_receipt(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::super::mutation_tests::session;
    use super::super::{SessionExecutionState, SessionRevision};

    #[test]
    fn terminal_aggregate_rejects_binding_before_the_environment_reducer() {
        // Cause/effect rule: C1 root execution/disposition is terminal; C2 an
        // otherwise valid Create/Adopt receipt arrives. Effect E1 the aggregate
        // returns BindingAfterTerminal and remains byte-identical. This terminal
        // authority dominates every Environment-local phase/replay condition.
        let mut value = session("terminal-binding", SessionRevision(1));
        value.execution = SessionExecutionState::Terminated;
        let before = value.clone();
        for kind in [
            crate::SessionEnvironmentEffectKind::Create,
            crate::SessionEnvironmentEffectKind::Adopt,
        ] {
            let receipt =
                crate::SessionEnvironmentReceipt::new("terminal-binding", kind, "sandbox", None);
            assert_eq!(
                value.transition_environment_binding(&receipt),
                Err(crate::SessionEnvironmentTransitionError::BindingAfterTerminal),
                "C1+C2/E1"
            );
            assert_eq!(value, before, "C1+C2/E1 byte-identical");
        }
    }
}
