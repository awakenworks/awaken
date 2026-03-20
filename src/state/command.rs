use std::ops::{Deref, DerefMut};

use crate::error::StateError;
use crate::model::{EffectSpec, RuntimeEffect, ScheduledAction, ScheduledActionSpec, TypedEffect};

use super::MutationBatch;

pub struct StateCommand {
    pub(crate) patch: MutationBatch,
    pub(crate) scheduled_actions: Vec<ScheduledAction>,
    pub(crate) effects: Vec<TypedEffect>,
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

    pub fn effect(&mut self, effect: RuntimeEffect) -> Result<(), StateError> {
        self.effects
            .push(TypedEffect::from_spec::<RuntimeEffect>(&effect)?);
        Ok(())
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

    #[test]
    fn state_command_accumulates_actions_and_effects() {
        let mut command = StateCommand::new();
        command
            .schedule_action::<TestAction>("go".into())
            .expect("schedule should succeed");
        command
            .effect(RuntimeEffect::Terminate {
                reason: "done".into(),
            })
            .expect("effect should encode");

        assert!(!command.is_empty());
        assert_eq!(command.scheduled_actions.len(), 1);
        assert_eq!(command.effects.len(), 1);
    }

    struct CustomEffect;

    impl EffectSpec for CustomEffect {
        const KEY: &'static str = "test.custom_effect";
        type Payload = String;
    }

    #[test]
    fn state_command_extend_merges_patch_actions_and_effects() {
        let mut left = StateCommand::new().with_base_revision(5);
        left.schedule_action::<TestAction>("left".into())
            .expect("left action should schedule");

        let mut right = StateCommand::new().with_base_revision(5);
        right
            .effect(RuntimeEffect::Suspend {
                reason: "wait".into(),
            })
            .expect("right effect should encode");

        left.extend(right).expect("commands should merge");

        assert_eq!(left.base_revision(), Some(5));
        assert_eq!(left.scheduled_actions.len(), 1);
        assert_eq!(left.effects.len(), 1);
    }

    #[test]
    fn state_command_emit_supports_custom_effect_specs() {
        let mut command = StateCommand::new();
        command
            .emit::<CustomEffect>("payload".into())
            .expect("custom effect should encode");

        let decoded = command.effects[0]
            .decode::<CustomEffect>()
            .expect("custom effect should decode");
        assert_eq!(decoded, "payload");
    }
}
