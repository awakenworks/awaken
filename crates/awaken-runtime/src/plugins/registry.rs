use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::phase::{
    EffectHandlerArc, PhaseHook, PhaseHookArc, ScheduledActionHandlerArc, TypedEffectAdapter,
    TypedEffectHandler, TypedScheduledActionAdapter, TypedScheduledActionHandler,
};
use crate::state::{KeyScope, MergeStrategy, StateKey, StateKeyOptions, StateMap};
use awaken_contract::StateError;
use awaken_contract::contract::profile_store::ProfileKey;
use awaken_contract::contract::tool::Tool;
use awaken_contract::model::{EffectSpec, JsonValue, Phase, ScheduledActionSpec};

#[derive(Clone)]
pub(crate) struct KeyRegistration {
    pub(crate) type_id: TypeId,
    pub(crate) key: String,
    pub(crate) options: StateKeyOptions,
    pub(crate) merge_strategy: MergeStrategy,
    pub(crate) scope: KeyScope,
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
            scope: options.scope,
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

#[derive(Clone)]
pub struct ProfileKeyRegistration {
    pub type_id: TypeId,
    pub key: String,
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

pub(crate) type RequestTransformArc =
    std::sync::Arc<dyn awaken_contract::contract::transform::InferenceRequestTransform>;

pub(crate) struct RequestTransformRegistration {
    pub(crate) plugin_id: String,
    pub(crate) transform: RequestTransformArc,
}

pub(crate) struct ToolRegistration {
    pub(crate) id: String,
    pub(crate) tool: Arc<dyn Tool>,
}

#[derive(Default)]
pub struct PluginRegistry {
    pub(crate) plugins: HashMap<TypeId, InstalledPlugin>,
    pub(crate) keys_by_type: HashMap<TypeId, KeyRegistration>,
    pub(crate) keys_by_name: HashMap<String, KeyRegistration>,
}

pub struct InstalledPlugin {
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
    pub profile_keys: Vec<ProfileKeyRegistration>,
    profile_key_type_ids: HashSet<TypeId>,
    profile_key_names: HashSet<String>,
    pub(crate) scheduled_actions: Vec<ScheduledActionHandlerRegistration>,
    scheduled_action_keys: HashSet<String>,
    pub(crate) effects: Vec<EffectHandlerRegistration>,
    effect_keys: HashSet<String>,
    pub(crate) phase_hooks: Vec<PhaseHookRegistration>,
    pub(crate) request_transforms: Vec<RequestTransformRegistration>,
    pub(crate) tools: Vec<ToolRegistration>,
    tool_ids: HashSet<String>,
}

impl PluginRegistrar {
    pub(crate) fn new() -> Self {
        Self {
            keys: Vec::new(),
            key_type_ids: HashSet::new(),
            key_names: HashSet::new(),
            profile_keys: Vec::new(),
            profile_key_type_ids: HashSet::new(),
            profile_key_names: HashSet::new(),
            scheduled_actions: Vec::new(),
            scheduled_action_keys: HashSet::new(),
            effects: Vec::new(),
            effect_keys: HashSet::new(),
            phase_hooks: Vec::new(),
            request_transforms: Vec::new(),
            tools: Vec::new(),
            tool_ids: HashSet::new(),
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

    /// Register a tool provided by this plugin.
    ///
    /// The tool becomes available to agents that activate this plugin.
    /// Tool IDs must be unique across all plugins; duplicates cause a resolve error.
    pub fn register_tool(
        &mut self,
        id: impl Into<String>,
        tool: Arc<dyn Tool>,
    ) -> Result<(), StateError> {
        let id = id.into();
        if !self.tool_ids.insert(id.clone()) {
            return Err(StateError::ToolAlreadyRegistered { tool_id: id });
        }
        self.tools.push(ToolRegistration { id, tool });
        Ok(())
    }

    /// Register a request transform applied after message assembly, before LLM call.
    pub fn register_request_transform<T>(&mut self, plugin_id: impl Into<String>, transform: T)
    where
        T: awaken_contract::contract::transform::InferenceRequestTransform + 'static,
    {
        self.request_transforms.push(RequestTransformRegistration {
            plugin_id: plugin_id.into(),
            transform: Arc::new(transform),
        });
    }

    /// Register a profile key for typed profile storage access.
    pub fn register_profile_key<K: ProfileKey>(&mut self) -> Result<(), StateError> {
        let type_id = TypeId::of::<K>();
        if !self.profile_key_type_ids.insert(type_id)
            || !self.profile_key_names.insert(K::KEY.to_string())
        {
            return Err(StateError::KeyAlreadyRegistered {
                key: K::KEY.to_string(),
            });
        }
        self.profile_keys.push(ProfileKeyRegistration {
            type_id,
            key: K::KEY.to_string(),
        });
        Ok(())
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_for_test() -> Self {
        Self::new()
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn profile_keys_for_test(&self) -> Vec<ProfileKeyRegistration> {
        self.profile_keys.clone()
    }

    /// Returns the list of registered tool IDs (test helper).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn tool_ids_for_test(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.id.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::profile_store::ProfileKey;

    struct TestLocale;
    impl ProfileKey for TestLocale {
        const KEY: &'static str = "locale";
        type Value = String;
    }

    #[test]
    fn register_profile_key_succeeds() {
        let mut registrar = PluginRegistrar::new_for_test();
        registrar.register_profile_key::<TestLocale>().unwrap();
        let keys = registrar.profile_keys_for_test();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, "locale");
    }

    #[test]
    fn register_duplicate_profile_key_errors() {
        let mut registrar = PluginRegistrar::new_for_test();
        registrar.register_profile_key::<TestLocale>().unwrap();
        let err = registrar.register_profile_key::<TestLocale>().unwrap_err();
        assert!(err.to_string().contains("locale"));
    }
}
