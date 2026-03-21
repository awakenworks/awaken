use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::StateError;
use crate::model::{EffectSpec, JsonValue, Phase, ScheduledActionSpec};
use crate::runtime::{
    EffectHandlerArc, PhaseHook, PhaseHookArc, ScheduledActionHandlerArc, TypedEffectAdapter,
    TypedEffectHandler, TypedScheduledActionAdapter, TypedScheduledActionHandler,
};
use crate::state::{MergeStrategy, StateKey, StateKeyOptions, StateMap};

use super::Plugin;

#[derive(Clone)]
pub(crate) struct KeyRegistration {
    pub(crate) type_id: TypeId,
    pub(crate) key: String,
    pub(crate) options: StateKeyOptions,
    pub(crate) merge_strategy: MergeStrategy,
    pub(crate) export: fn(&StateMap) -> Result<Option<JsonValue>, StateError>,
    pub(crate) import: fn(&mut StateMap, JsonValue) -> Result<(), StateError>,
    pub(crate) clear: fn(&mut StateMap),
}

impl KeyRegistration {
    pub(crate) fn new<K: StateKey>(options: StateKeyOptions) -> Self {
        Self {
            type_id: TypeId::of::<K>(),
            key: K::KEY.into(),
            options,
            merge_strategy: K::MERGE,
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
    pub(crate) keys_by_type: HashMap<TypeId, KeyRegistration>,
    pub(crate) keys_by_name: HashMap<String, KeyRegistration>,
}

pub struct InstalledPlugin {
    pub(crate) plugin: Arc<dyn Plugin>,
    pub(crate) owned_key_type_ids: Vec<TypeId>,
}

impl PluginRegistry {
    pub(crate) fn merge_strategy(&self, key: &str) -> MergeStrategy {
        self.keys_by_name
            .get(key)
            .map(|reg| reg.merge_strategy)
            .unwrap_or(MergeStrategy::Exclusive)
    }

    pub(crate) fn ensure_key(&self, key: &str) -> Result<(), StateError> {
        if self.keys_by_name.contains_key(key) {
            Ok(())
        } else {
            Err(StateError::UnknownKey { key: key.into() })
        }
    }
}

pub struct PluginRegistrar {
    pub(crate) keys: Vec<KeyRegistration>,
    key_type_ids: HashSet<TypeId>,
    key_names: HashSet<String>,
    pub(crate) scheduled_actions: Vec<ScheduledActionHandlerRegistration>,
    scheduled_action_keys: HashSet<String>,
    pub(crate) effects: Vec<EffectHandlerRegistration>,
    effect_keys: HashSet<String>,
    pub(crate) phase_hooks: Vec<PhaseHookRegistration>,
}

impl PluginRegistrar {
    pub(crate) fn new() -> Self {
        Self {
            keys: Vec::new(),
            key_type_ids: HashSet::new(),
            key_names: HashSet::new(),
            scheduled_actions: Vec::new(),
            scheduled_action_keys: HashSet::new(),
            effects: Vec::new(),
            effect_keys: HashSet::new(),
            phase_hooks: Vec::new(),
        }
    }

    pub fn register_key<K>(&mut self, options: StateKeyOptions) -> Result<(), StateError>
    where
        K: StateKey,
    {
        let type_id = TypeId::of::<K>();
        if !self.key_type_ids.insert(type_id) || !self.key_names.insert(K::KEY.to_string()) {
            return Err(StateError::KeyAlreadyRegistered {
                key: K::KEY.to_string(),
            });
        }

        self.keys.push(KeyRegistration::new::<K>(options));
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
