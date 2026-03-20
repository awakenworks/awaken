use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::StateError;
use crate::model::{EffectSpec, JsonValue, Phase, ScheduledActionSpec};
use crate::runtime::{
    EffectHandlerArc, PhaseHook, PhaseHookArc, ScheduledActionHandlerArc, TypedEffectAdapter,
    TypedEffectHandler, TypedScheduledActionAdapter, TypedScheduledActionHandler,
};
use crate::state::{SlotMap, SlotOptions, StateSlot};

use super::Plugin;

#[derive(Clone)]
pub(crate) struct SlotRegistration {
    pub(crate) type_id: TypeId,
    pub(crate) key: String,
    pub(crate) options: SlotOptions,
    pub(crate) export: fn(&SlotMap) -> Result<Option<JsonValue>, StateError>,
    pub(crate) import: fn(&mut SlotMap, JsonValue) -> Result<(), StateError>,
    pub(crate) clear: fn(&mut SlotMap),
}

impl SlotRegistration {
    pub(crate) fn new<K: StateSlot>(options: SlotOptions) -> Self {
        Self {
            type_id: TypeId::of::<K>(),
            key: K::KEY.into(),
            options,
            export: |map| match map.get::<K>() {
                Some(value) => K::encode(value).map(Some),
                None => Ok(None),
            },
            import: |map, json| {
                let value = K::decode(json)?;
                map.insert::<K>(value);
                Ok(())
            },
            clear: |map| {
                let _ = map.remove::<K>();
            },
        }
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

pub(crate) struct PhaseHookRegistration {
    pub(crate) phase: Phase,
    pub(crate) plugin_id: String,
    pub(crate) hook: PhaseHookArc,
}

#[derive(Default)]
pub struct PluginRegistry {
    pub(crate) plugins: HashMap<TypeId, InstalledPlugin>,
    pub(crate) slots_by_type: HashMap<TypeId, SlotRegistration>,
    pub(crate) slots_by_key: HashMap<String, SlotRegistration>,
}

pub struct InstalledPlugin {
    pub(crate) plugin: Arc<dyn Plugin>,
    pub(crate) owned_slot_type_ids: Vec<TypeId>,
}

impl PluginRegistry {
    pub(crate) fn ensure_slot(&self, key: &str) -> Result<(), StateError> {
        if self.slots_by_key.contains_key(key) {
            Ok(())
        } else {
            Err(StateError::UnknownSlot { key: key.into() })
        }
    }
}

pub struct PluginRegistrar {
    pub(crate) slots: Vec<SlotRegistration>,
    slot_type_ids: HashSet<TypeId>,
    slot_keys: HashSet<String>,
    pub(crate) scheduled_actions: Vec<ScheduledActionHandlerRegistration>,
    scheduled_action_keys: HashSet<String>,
    pub(crate) effects: Vec<EffectHandlerRegistration>,
    effect_keys: HashSet<String>,
    pub(crate) phase_hooks: Vec<PhaseHookRegistration>,
}

impl PluginRegistrar {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            slot_type_ids: HashSet::new(),
            slot_keys: HashSet::new(),
            scheduled_actions: Vec::new(),
            scheduled_action_keys: HashSet::new(),
            effects: Vec::new(),
            effect_keys: HashSet::new(),
            phase_hooks: Vec::new(),
        }
    }

    pub fn register_slot<K>(&mut self, options: SlotOptions) -> Result<(), StateError>
    where
        K: StateSlot,
    {
        let type_id = TypeId::of::<K>();
        if !self.slot_type_ids.insert(type_id) || !self.slot_keys.insert(K::KEY.to_string()) {
            return Err(StateError::SlotAlreadyRegistered {
                key: K::KEY.to_string(),
            });
        }

        self.slots.push(SlotRegistration::new::<K>(options));
        Ok(())
    }

    pub fn register_scheduled_action<A, H>(&mut self, handler: H) -> Result<(), StateError>
    where
        A: ScheduledActionSpec,
        H: TypedScheduledActionHandler<A>,
    {
        let key = A::KEY.to_string();
        if !self.scheduled_action_keys.insert(key.clone()) {
            return Err(StateError::HandlerAlreadyRegistered { key });
        }

        self.scheduled_actions
            .push(ScheduledActionHandlerRegistration {
                key,
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
        if !self.effect_keys.insert(key.clone()) {
            return Err(StateError::EffectHandlerAlreadyRegistered { key });
        }

        self.effects.push(EffectHandlerRegistration {
            key,
            handler: Arc::new(TypedEffectAdapter::<E, H> {
                handler,
                _marker: std::marker::PhantomData,
            }),
        });
        Ok(())
    }

    pub fn register_phase_hook<H>(
        &mut self,
        plugin_id: impl Into<String>,
        phase: Phase,
        hook: H,
    ) -> Result<(), StateError>
    where
        H: PhaseHook,
    {
        self.phase_hooks.push(PhaseHookRegistration {
            phase,
            plugin_id: plugin_id.into(),
            hook: Arc::new(hook),
        });
        Ok(())
    }
}
