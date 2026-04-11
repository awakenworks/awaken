use std::ops::{Deref, DerefMut};

use crate::StateError;
use crate::model::{EffectSpec, ScheduledAction, ScheduledActionSpec, TypedEffect};

use super::{MergeStrategy, MutationBatch};

/// A command that carries state mutations, scheduled actions, and effects.
///
/// This is the primary mechanism for both plugin hooks and tools to express
/// side-effects. Plugins return `StateCommand` from phase hooks; tools return
/// it alongside `ToolResult` to declare their side-effects using the same
/// machinery.
#[derive(Debug)]
pub struct StateCommand {
    pub patch: MutationBatch,
    pub scheduled_actions: Vec<ScheduledAction>,
    pub effects: Vec<TypedEffect>,
}

impl StateCommand {
    pub fn new() -> Self {
        Self {
            patch: MutationBatch::new(),
            scheduled_actions: Vec::new(),
            effects: Vec::new(),
        }
    }

    pub fn with_base_revision(mut self, revision: u64) -> Self {
        self.patch = self.patch.with_base_revision(revision);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.patch.is_empty() && self.scheduled_actions.is_empty() && self.effects.is_empty()
    }

    /// Inspect scheduled actions (useful for testing).
    pub fn scheduled_actions(&self) -> &[ScheduledAction] {
        &self.scheduled_actions
    }

    pub fn emit<E: EffectSpec>(&mut self, payload: E::Payload) -> Result<(), StateError> {
        self.effects.push(TypedEffect::from_spec::<E>(&payload)?);
        Ok(())
    }

    pub fn schedule_action<A: ScheduledActionSpec>(
        &mut self,
        payload: A::Payload,
    ) -> Result<(), StateError> {
        self.scheduled_actions.push(ScheduledAction::new(
            A::PHASE,
            A::KEY,
            A::encode_payload(&payload)?,
        ));
        Ok(())
    }

    pub fn extend(&mut self, mut other: Self) -> Result<(), StateError> {
        self.patch.extend(other.patch)?;
        self.scheduled_actions.append(&mut other.scheduled_actions);
        self.effects.append(&mut other.effects);
        Ok(())
    }

    /// Merge two commands from parallel execution using the given merge strategy.
    pub fn merge_parallel<F>(self, other: Self, strategy: F) -> Result<Self, StateError>
    where
        F: Fn(&str) -> MergeStrategy,
    {
        let patch = self.patch.merge_parallel(other.patch, strategy)?;
        let mut scheduled_actions = self.scheduled_actions;
        scheduled_actions.extend(other.scheduled_actions);
        let mut effects = self.effects;
        effects.extend(other.effects);
        Ok(Self {
            patch,
            scheduled_actions,
            effects,
        })
    }
}

impl Default for StateCommand {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for StateCommand {
    type Target = MutationBatch;

    fn deref(&self) -> &Self::Target {
        &self.patch
    }
}

impl DerefMut for StateCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.patch
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{EffectSpec, Phase, ScheduledActionSpec};

    use super::*;

    struct TestAction;

    impl ScheduledActionSpec for TestAction {
        const KEY: &'static str = "test.action";
        const PHASE: Phase = Phase::RunStart;
        type Payload = String;
    }

    struct CustomEffect;

    impl EffectSpec for CustomEffect {
        const KEY: &'static str = "test.custom_effect";
        type Payload = String;
    }

    #[test]
    fn state_command_accumulates_actions_and_effects() {
        let mut command = StateCommand::new();
        command
            .schedule_action::<TestAction>("go".into())
            .expect("schedule should succeed");
        command
            .emit::<CustomEffect>("payload".into())
            .expect("effect should encode");

        assert!(!command.is_empty());
        assert_eq!(command.scheduled_actions.len(), 1);
        assert_eq!(command.effects.len(), 1);
    }

    #[test]
    fn state_command_extend_merges_all() {
        let mut left = StateCommand::new();
        left.schedule_action::<TestAction>("left".into()).unwrap();

        let mut right = StateCommand::new();
        right.emit::<CustomEffect>("effect".into()).unwrap();

        left.extend(right).unwrap();
        assert_eq!(left.scheduled_actions.len(), 1);
        assert_eq!(left.effects.len(), 1);
    }

    #[test]
    fn state_command_new_is_empty() {
        let cmd = StateCommand::new();
        assert!(cmd.is_empty());
    }

    #[test]
    fn state_command_merge_parallel_combines_all_fields() {
        let mut left = StateCommand::new();
        left.schedule_action::<TestAction>("left_action".into())
            .unwrap();
        left.emit::<CustomEffect>("left_effect".into()).unwrap();

        let mut right = StateCommand::new();
        right
            .schedule_action::<TestAction>("right_action".into())
            .unwrap();
        right.emit::<CustomEffect>("right_effect".into()).unwrap();

        let merged = left
            .merge_parallel(right, |_| MergeStrategy::Commutative)
            .unwrap();
        assert_eq!(merged.scheduled_actions.len(), 2);
        assert_eq!(merged.effects.len(), 2);
    }
}
