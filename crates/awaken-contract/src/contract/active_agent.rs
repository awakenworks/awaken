//! Active agent state key.

/// StateKey for the active agent ID. Handoff writes this.
pub struct ActiveAgentIdKey;

impl crate::state::StateKey for ActiveAgentIdKey {
    const KEY: &'static str = "__runtime.active_agent";
    type Value = Option<String>;
    type Update = Option<String>;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_agent_key_apply() {
        use crate::state::StateKey;
        let mut val: Option<String> = None;
        ActiveAgentIdKey::apply(&mut val, Some("reviewer".into()));
        assert_eq!(val.as_deref(), Some("reviewer"));
        ActiveAgentIdKey::apply(&mut val, None);
        assert!(val.is_none());
    }
}
