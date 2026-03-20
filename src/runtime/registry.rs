use crate::foundation::*;
use crate::plugin::{PluginRegistrar, StatePlugin};
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use super::handlers::{
    EffectHandlerArc, ScheduledActionHandlerArc, TypedEffectAdapter, TypedEffectHandler,
    TypedScheduledActionAdapter, TypedScheduledActionHandler,
};

#[derive(Default)]
pub(crate) struct InstalledRuntimePlugin {
    pub(crate) scheduled_action_keys: Vec<String>,
    pub(crate) effect_keys: Vec<String>,
}

#[derive(Default)]
pub(crate) struct RuntimeRegistry {
    pub(crate) scheduled_action_handlers: HashMap<String, ScheduledActionHandlerArc>,
    pub(crate) effect_handlers: HashMap<String, EffectHandlerArc>,
    pub(crate) installed_plugins: HashMap<TypeId, InstalledRuntimePlugin>,
}

pub(crate) struct RuntimeQueuePlugin;

impl StatePlugin for RuntimeQueuePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "phase-runtime",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        let runtime_options = SlotOptions {
            persistent: true,
            retain_on_uninstall: false,
        };
        registrar.register_slot::<PendingScheduledActions>(runtime_options)?;
        registrar.register_slot::<FailedScheduledActions>(runtime_options)?;
        registrar.register_slot::<ScheduledActionLog>(runtime_options)?;
        registrar.register_slot::<EffectLog>(runtime_options)?;
        Ok(())
    }
}

pub(crate) struct ScheduledActionHandlerRegistration {
    pub(crate) key: String,
    pub(crate) handler: ScheduledActionHandlerArc,
}

pub(crate) struct EffectHandlerRegistration {
    pub(crate) key: String,
    pub(crate) handler: EffectHandlerArc,
}

pub struct RuntimePluginRegistrar {
    pub(crate) scheduled_actions: Vec<ScheduledActionHandlerRegistration>,
    pub(crate) effects: Vec<EffectHandlerRegistration>,
}

impl RuntimePluginRegistrar {
    pub(crate) fn new() -> Self {
        Self {
            scheduled_actions: Vec::new(),
            effects: Vec::new(),
        }
    }

    pub fn register_scheduled_action<A, H>(&mut self, handler: H) -> Result<(), StateError>
    where
        A: ScheduledActionSpec,
        H: TypedScheduledActionHandler<A>,
    {
        let key = A::KEY.to_string();
        if self.scheduled_actions.iter().any(|entry| entry.key == key) {
            return Err(StateError::HandlerAlreadyRegistered { key });
        }

        self.scheduled_actions
            .push(ScheduledActionHandlerRegistration {
                key: A::KEY.to_string(),
                handler: Arc::new(TypedScheduledActionAdapter::<A, H> {
                    handler,
                    _marker: std::marker::PhantomData,
                }),
            });
        Ok(())
    }

    pub fn register_effect<E, H>(&mut self, handler: H) -> Result<(), StateError>
    where
        E: EffectSpec,
        H: TypedEffectHandler<E>,
    {
        let key = E::KEY.to_string();
        if self.effects.iter().any(|entry| entry.key == key) {
            return Err(StateError::EffectHandlerAlreadyRegistered { key });
        }

        self.effects.push(EffectHandlerRegistration {
            key: E::KEY.to_string(),
            handler: Arc::new(TypedEffectAdapter::<E, H> {
                handler,
                _marker: std::marker::PhantomData,
            }),
        });
        Ok(())
    }
}

pub trait RuntimePlugin: StatePlugin {
    fn register_runtime(&self, registrar: &mut RuntimePluginRegistrar) -> Result<(), StateError> {
        let _ = registrar;
        Ok(())
    }
}
