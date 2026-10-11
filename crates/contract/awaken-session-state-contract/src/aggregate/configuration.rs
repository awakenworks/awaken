use super::{
    SessionAggregateCommandError, SessionConfigurationOperations, SessionToolConfiguration,
};
use std::collections::BTreeMap;

impl SessionConfigurationOperations<'_> {
    /// Replace or clear the compatible title field.
    pub fn set_title(&mut self, title: Option<String>) {
        self.aggregate.state.title = title;
    }

    /// Clear the whole mutable metadata projection explicitly.
    pub fn clear_metadata(&mut self) {
        self.aggregate.state.metadata.clear();
    }

    /// Patch metadata by key; None removes exactly that key.
    pub fn patch_metadata(&mut self, patch: &BTreeMap<String, Option<String>>) {
        for (key, value) in patch {
            match value {
                Some(value) => {
                    self.aggregate
                        .state
                        .metadata
                        .insert(key.clone(), value.clone());
                }
                None => {
                    self.aggregate.state.metadata.remove(key);
                }
            }
        }
    }

    /// Install the exact neutral mutable tool configuration.
    pub fn set_tools(
        &mut self,
        tools: SessionToolConfiguration,
    ) -> Result<(), SessionAggregateCommandError> {
        if self
            .aggregate
            .state
            .frozen_baseline()
            .is_none_or(|baseline| !baseline.mutation_policy.admits_public_agent_mutation())
        {
            return Err(SessionAggregateCommandError::FrozenTools);
        }
        self.aggregate.state.tools = tools;
        Ok(())
    }
}
