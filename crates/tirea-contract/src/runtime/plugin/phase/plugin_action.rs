use std::any::{Any, TypeId};
use std::fmt;

/// Type-erased plugin-domain action routed by `plugin_id`.
///
/// Unlike `AnyStateAction`, this action does not expose state/reducer details
/// to callers. Plugins receive their own actions and apply internal reducers.
pub struct AnyPluginAction {
    plugin_id: String,
    action_type_id: TypeId,
    action_type_name: &'static str,
    payload: Box<dyn Any + Send>,
}

impl AnyPluginAction {
    /// Create a type-erased plugin action.
    pub fn new<A>(plugin_id: impl Into<String>, action: A) -> Self
    where
        A: Send + 'static,
    {
        Self {
            plugin_id: plugin_id.into(),
            action_type_id: TypeId::of::<A>(),
            action_type_name: std::any::type_name::<A>(),
            payload: Box::new(action),
        }
    }

    /// Target plugin id.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Type id of the erased action.
    pub fn action_type_id(&self) -> TypeId {
        self.action_type_id
    }

    /// Type name of the erased action.
    pub fn action_type_name(&self) -> &'static str {
        self.action_type_name
    }

    /// Borrow typed action payload when type matches.
    pub fn downcast_ref<A>(&self) -> Option<&A>
    where
        A: Send + 'static,
    {
        self.payload.downcast_ref::<A>()
    }

    /// Consume and downcast into typed action payload.
    pub fn downcast<A>(self) -> Result<A, Self>
    where
        A: Send + 'static,
    {
        match self.payload.downcast::<A>() {
            Ok(payload) => Ok(*payload),
            Err(payload) => Err(Self {
                plugin_id: self.plugin_id,
                action_type_id: self.action_type_id,
                action_type_name: self.action_type_name,
                payload,
            }),
        }
    }
}

impl fmt::Debug for AnyPluginAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyPluginAction")
            .field("plugin_id", &self.plugin_id)
            .field("action_type_name", &self.action_type_name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::AnyPluginAction;

    #[test]
    fn any_plugin_action_roundtrip() {
        let action = AnyPluginAction::new("permission", 42usize);
        assert_eq!(action.plugin_id(), "permission");
        assert!(action.downcast_ref::<usize>().is_some());

        let typed = action
            .downcast::<usize>()
            .expect("action payload should downcast");
        assert_eq!(typed, 42);
    }

    #[test]
    fn any_plugin_action_downcast_failure_keeps_metadata() {
        let action = AnyPluginAction::new("reminder", "hello".to_string());
        let action = action
            .downcast::<usize>()
            .expect_err("downcast should fail");
        assert_eq!(action.plugin_id(), "reminder");
        assert!(
            action.action_type_name().contains("String"),
            "type metadata should be preserved"
        );
    }
}
