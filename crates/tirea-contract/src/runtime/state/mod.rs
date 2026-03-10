pub mod scope_context;
pub mod scope_registry;
pub mod serialized_state_action;
pub mod spec;

pub use scope_context::ScopeContext;
pub use scope_registry::StateScopeRegistry;
pub use serialized_state_action::{
    SerializedStateAction, StateActionDecodeError, StateActionDeserializerRegistry,
};
pub use spec::{reduce_state_actions, AnyStateAction, StateScope, StateSpec};
