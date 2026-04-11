use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::active_agent::ActiveAgentIdKey;
use awaken_contract::model::Phase;

use crate::phase::{ExecutionEnv, PhaseRuntime};
use crate::plugins::Plugin;
use crate::state::{KeyScope, StateStore};

use super::*;

#[test]
fn default_state_is_all_none() {
    let state = HandoffState::default();
    assert!(state.active_agent.is_none());
    assert!(state.requested_agent.is_none());
}

#[test]
fn request_sets_requested_agent() {
    let mut state = HandoffState::default();
    state.reduce(HandoffAction::Request {
        agent: "fast".into(),
    });
    assert!(state.active_agent.is_none());
    assert_eq!(state.requested_agent.as_deref(), Some("fast"));
}

#[test]
fn activate_sets_active_and_clears_requested() {
    let mut state = HandoffState {
        active_agent: None,
        requested_agent: Some("fast".into()),
    };
    state.reduce(HandoffAction::Activate {
        agent: "fast".into(),
    });
    assert_eq!(state.active_agent.as_deref(), Some("fast"));
    assert!(state.requested_agent.is_none());
}

#[test]
fn clear_resets_both() {
    let mut state = HandoffState {
        active_agent: Some("fast".into()),
        requested_agent: Some("deep".into()),
    };
    state.reduce(HandoffAction::Clear);
    assert!(state.active_agent.is_none());
    assert!(state.requested_agent.is_none());
}

#[test]
fn roundtrip_serialization() {
    let state = HandoffState {
        active_agent: Some("fast".into()),
        requested_agent: None,
    };
    let json = serde_json::to_value(&state).unwrap();
    let back: HandoffState = serde_json::from_value(json).unwrap();
    assert_eq!(state.active_agent, back.active_agent);
    assert_eq!(state.requested_agent, back.requested_agent);
}

#[test]
fn action_roundtrip() {
    let action = HandoffAction::Request {
        agent: "fast".into(),
    };
    let json = serde_json::to_value(&action).unwrap();
    let back: HandoffAction = serde_json::from_value(json).unwrap();
    assert_eq!(action, back);
}

#[test]
fn plugin_descriptor() {
    let plugin = HandoffPlugin::new(HashMap::new());
    assert_eq!(plugin.descriptor().name, HANDOFF_PLUGIN_ID);
}

#[test]
fn plugin_registers_state_key() {
    let plugin = HandoffPlugin::new(HashMap::new());
    let store = StateStore::new();
    store.install_plugin(plugin).unwrap();
    // Key should be registered
    let registry = store.registry.lock();
    assert!(registry.keys_by_name.contains_key("agent_handoff"));
    assert!(registry.keys_by_name.contains_key(
        <awaken_contract::contract::active_agent::ActiveAgentIdKey as crate::state::StateKey>::KEY,
    ));
    assert_eq!(
        registry
            .keys_by_name
            .get("agent_handoff")
            .map(|reg| reg.scope),
        Some(KeyScope::Thread)
    );
    assert_eq!(
        registry
            .keys_by_name
            .get(<awaken_contract::contract::active_agent::ActiveAgentIdKey as crate::state::StateKey>::KEY)
            .map(|reg| reg.scope),
        Some(KeyScope::Thread)
    );
}

#[test]
fn effective_agent_prefers_requested() {
    let state = HandoffState {
        active_agent: Some("slow".into()),
        requested_agent: Some("fast".into()),
    };
    assert_eq!(
        HandoffPlugin::effective_agent(&state).map(String::as_str),
        Some("fast")
    );
}

#[test]
fn effective_agent_falls_back_to_active() {
    let state = HandoffState {
        active_agent: Some("slow".into()),
        requested_agent: None,
    };
    assert_eq!(
        HandoffPlugin::effective_agent(&state).map(String::as_str),
        Some("slow")
    );
}

#[test]
fn effective_agent_none_when_empty() {
    let state = HandoffState::default();
    assert!(HandoffPlugin::effective_agent(&state).is_none());
}

#[test]
fn overlay_lookup() {
    let mut overlays = HashMap::new();
    overlays.insert(
        "fast".to_string(),
        AgentOverlay {
            model_id: Some("claude-haiku".into()),
            system_prompt: Some("You are in fast mode.".into()),
            ..Default::default()
        },
    );
    let plugin = HandoffPlugin::new(overlays);
    assert!(plugin.overlay("fast").is_some());
    assert!(plugin.overlay("missing").is_none());
}

#[test]
fn handoff_state_via_store() {
    let store = StateStore::new();
    let plugin = HandoffPlugin::new(HashMap::new());
    store.install_plugin(plugin).unwrap();

    // Request handoff
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(request_handoff("fast"));
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.requested_agent.as_deref(), Some("fast"));
    assert!(state.active_agent.is_none());

    // Activate
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(activate_handoff("fast"));
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.active_agent.as_deref(), Some("fast"));
    assert!(state.requested_agent.is_none());

    // Clear
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(clear_handoff());
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert!(state.active_agent.is_none());
    assert!(state.requested_agent.is_none());
}

#[tokio::test]
async fn run_start_syncs_requested_handoff_to_active_agent_id_key() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    let plugin: Arc<dyn Plugin> = Arc::new(HandoffPlugin::new(HashMap::new()));
    let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();

    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(request_handoff("reviewer"));
    store.commit(patch).unwrap();

    runtime.run_phase(&env, Phase::RunStart).await.unwrap();

    assert_eq!(
        store.read::<ActiveAgentIdKey>(),
        Some(Some("reviewer".into()))
    );

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.active_agent.as_deref(), Some("reviewer"));
    assert!(state.requested_agent.is_none());
}

#[test]
fn request_overwrites_previous_request() {
    let mut state = HandoffState::default();
    state.reduce(HandoffAction::Request {
        agent: "first".into(),
    });
    state.reduce(HandoffAction::Request {
        agent: "second".into(),
    });
    assert_eq!(state.requested_agent.as_deref(), Some("second"));
}

#[test]
fn activate_different_agent_than_requested() {
    let mut state = HandoffState {
        active_agent: None,
        requested_agent: Some("fast".into()),
    };
    // Can activate a different agent than requested
    state.reduce(HandoffAction::Activate {
        agent: "slow".into(),
    });
    assert_eq!(state.active_agent.as_deref(), Some("slow"));
    assert!(state.requested_agent.is_none());
}

#[test]
fn request_does_not_affect_active() {
    let mut state = HandoffState {
        active_agent: Some("current".into()),
        requested_agent: None,
    };
    state.reduce(HandoffAction::Request {
        agent: "next".into(),
    });
    assert_eq!(state.active_agent.as_deref(), Some("current"));
    assert_eq!(state.requested_agent.as_deref(), Some("next"));
}

#[test]
fn activate_replaces_active() {
    let mut state = HandoffState {
        active_agent: Some("old".into()),
        requested_agent: Some("new".into()),
    };
    state.reduce(HandoffAction::Activate {
        agent: "new".into(),
    });
    assert_eq!(state.active_agent.as_deref(), Some("new"));
    assert!(state.requested_agent.is_none());
}

#[test]
fn clear_on_already_empty_is_noop() {
    let mut state = HandoffState::default();
    state.reduce(HandoffAction::Clear);
    assert!(state.active_agent.is_none());
    assert!(state.requested_agent.is_none());
}

#[test]
fn action_all_variants_serialization() {
    let actions = vec![
        HandoffAction::Request {
            agent: "test".into(),
        },
        HandoffAction::Activate {
            agent: "test".into(),
        },
        HandoffAction::Clear,
    ];
    for action in actions {
        let json = serde_json::to_value(&action).unwrap();
        let back: HandoffAction = serde_json::from_value(json).unwrap();
        assert_eq!(action, back);
    }
}

#[test]
fn overlay_default_is_all_none() {
    let overlay = AgentOverlay::default();
    assert!(overlay.system_prompt.is_none());
    assert!(overlay.model_id.is_none());
    assert!(overlay.allowed_tools.is_none());
    assert!(overlay.excluded_tools.is_none());
}

#[test]
fn overlay_serialization_roundtrip() {
    let overlay = AgentOverlay {
        system_prompt: Some("You are helpful".into()),
        model_id: Some("gpt-4".into()),
        allowed_tools: Some(vec!["search".into(), "read".into()]),
        excluded_tools: Some(vec!["delete".into()]),
    };
    let json = serde_json::to_value(&overlay).unwrap();
    let back: AgentOverlay = serde_json::from_value(json).unwrap();
    assert_eq!(back.system_prompt.as_deref(), Some("You are helpful"));
    assert_eq!(back.model_id.as_deref(), Some("gpt-4"));
    assert_eq!(back.allowed_tools.as_ref().unwrap().len(), 2);
    assert_eq!(back.excluded_tools.as_ref().unwrap().len(), 1);
}

#[test]
fn plugin_overlay_returns_configured_overlay() {
    let mut overlays = HashMap::new();
    overlays.insert(
        "fast".to_string(),
        AgentOverlay {
            model_id: Some("haiku".into()),
            ..Default::default()
        },
    );
    overlays.insert(
        "deep".to_string(),
        AgentOverlay {
            model_id: Some("opus".into()),
            ..Default::default()
        },
    );
    let plugin = HandoffPlugin::new(overlays);
    assert_eq!(
        plugin.overlay("fast").unwrap().model_id.as_deref(),
        Some("haiku")
    );
    assert_eq!(
        plugin.overlay("deep").unwrap().model_id.as_deref(),
        Some("opus")
    );
    assert!(plugin.overlay("nonexistent").is_none());
}

#[test]
fn handoff_full_lifecycle_via_store() {
    let store = StateStore::new();
    store
        .install_plugin(HandoffPlugin::new(HashMap::new()))
        .unwrap();

    // Initial: no active, no requested
    let state = store.read::<ActiveAgentKey>();
    assert!(state.is_none() || state.unwrap().active_agent.is_none());

    // Request fast
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(request_handoff("fast"));
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.requested_agent.as_deref(), Some("fast"));
    assert!(state.active_agent.is_none());

    // Activate fast
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(activate_handoff("fast"));
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.active_agent.as_deref(), Some("fast"));
    assert!(state.requested_agent.is_none());

    // Request deep (while fast is active)
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(request_handoff("deep"));
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.active_agent.as_deref(), Some("fast"));
    assert_eq!(state.requested_agent.as_deref(), Some("deep"));

    // Effective should prefer requested
    assert_eq!(
        HandoffPlugin::effective_agent(&state).map(String::as_str),
        Some("deep")
    );

    // Clear
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(clear_handoff());
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert!(HandoffPlugin::effective_agent(&state).is_none());
}

// ---------------------------------------------------------------------------
// on_deactivate lifecycle tests
// ---------------------------------------------------------------------------

/// Helper: create a store with handoff keys registered via ExecutionEnv,
/// returning (store, plugin_arc) so on_deactivate can be called on the Arc.
fn store_with_handoff_plugin() -> (StateStore, Arc<HandoffPlugin>) {
    let store = StateStore::new();
    let plugin = Arc::new(HandoffPlugin::new(HashMap::new()));
    let plugin_dyn: Arc<dyn Plugin> = plugin.clone();
    let env = ExecutionEnv::from_plugins(&[plugin_dyn], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();
    (store, plugin)
}

#[test]
fn on_deactivate_clears_active_agent_key() {
    let (store, plugin) = store_with_handoff_plugin();

    // Set up active agent state
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(HandoffAction::Activate {
        agent: "fast".into(),
    });
    patch.update::<ActiveAgentIdKey>(Some("fast".into()));
    store.commit(patch).unwrap();

    // Verify state is set
    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.active_agent.as_deref(), Some("fast"));
    assert_eq!(store.read::<ActiveAgentIdKey>(), Some(Some("fast".into())));

    // Call on_deactivate and commit its mutations
    let mut deactivate_patch = store.begin_mutation();
    plugin.on_deactivate(&mut deactivate_patch).unwrap();
    store.commit(deactivate_patch).unwrap();

    // Both keys should be cleared
    let state = store.read::<ActiveAgentKey>().unwrap();
    assert!(state.active_agent.is_none());
    assert!(state.requested_agent.is_none());
    assert_eq!(store.read::<ActiveAgentIdKey>(), Some(None));
}

#[test]
fn on_deactivate_clears_requested_agent_too() {
    let (store, plugin) = store_with_handoff_plugin();

    // Set up state with both active and requested agents
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(HandoffAction::Activate {
        agent: "fast".into(),
    });
    patch.update::<ActiveAgentKey>(HandoffAction::Request {
        agent: "deep".into(),
    });
    patch.update::<ActiveAgentIdKey>(Some("fast".into()));
    store.commit(patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.active_agent.as_deref(), Some("fast"));
    assert_eq!(state.requested_agent.as_deref(), Some("deep"));

    // Deactivate clears both
    let mut deactivate_patch = store.begin_mutation();
    plugin.on_deactivate(&mut deactivate_patch).unwrap();
    store.commit(deactivate_patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert!(state.active_agent.is_none());
    assert!(state.requested_agent.is_none());
    assert_eq!(store.read::<ActiveAgentIdKey>(), Some(None));
}

#[test]
fn on_deactivate_is_idempotent() {
    let (store, plugin) = store_with_handoff_plugin();

    // Call on_deactivate on already-empty state (no prior activation)
    let mut patch = store.begin_mutation();
    plugin.on_deactivate(&mut patch).unwrap();
    store.commit(patch).unwrap();

    // Should still succeed; state should be cleared/default
    let state = store.read::<ActiveAgentKey>();
    // Either None (never written) or cleared
    if let Some(state) = state {
        assert!(state.active_agent.is_none());
        assert!(state.requested_agent.is_none());
    }

    // Call again — still fine
    let mut patch = store.begin_mutation();
    plugin.on_deactivate(&mut patch).unwrap();
    store.commit(patch).unwrap();
}

#[test]
fn on_deactivate_then_reactivate_via_on_activate() {
    let (store, plugin) = store_with_handoff_plugin();

    let spec = awaken_contract::registry_spec::AgentSpec::default();

    // Activate plugin
    let mut activate_patch = store.begin_mutation();
    plugin.on_activate(&spec, &mut activate_patch).unwrap();
    store.commit(activate_patch).unwrap();

    // Set agent state
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(HandoffAction::Activate {
        agent: "fast".into(),
    });
    patch.update::<ActiveAgentIdKey>(Some("fast".into()));
    store.commit(patch).unwrap();

    // Deactivate
    let mut deactivate_patch = store.begin_mutation();
    plugin.on_deactivate(&mut deactivate_patch).unwrap();
    store.commit(deactivate_patch).unwrap();

    let state = store.read::<ActiveAgentKey>().unwrap();
    assert!(state.active_agent.is_none());
    assert_eq!(store.read::<ActiveAgentIdKey>(), Some(None));

    // Reactivate
    let mut activate_patch = store.begin_mutation();
    plugin.on_activate(&spec, &mut activate_patch).unwrap();
    store.commit(activate_patch).unwrap();

    // State should still be cleared (on_activate for HandoffPlugin is a no-op)
    let state = store.read::<ActiveAgentKey>().unwrap();
    assert!(state.active_agent.is_none());
    assert_eq!(store.read::<ActiveAgentIdKey>(), Some(None));
}

#[tokio::test]
async fn on_deactivate_clears_state_after_phase_run() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    let plugin = Arc::new(HandoffPlugin::new(HashMap::new()));
    let plugin_ref: Arc<dyn Plugin> = plugin.clone();
    let env = ExecutionEnv::from_plugins(&[plugin_ref], &Default::default()).unwrap();
    store.register_keys(&env.key_registrations).unwrap();

    // Request and sync a handoff via the phase hook
    let mut patch = store.begin_mutation();
    patch.update::<ActiveAgentKey>(request_handoff("reviewer"));
    store.commit(patch).unwrap();

    runtime.run_phase(&env, Phase::RunStart).await.unwrap();

    // Verify active agent is set after phase run
    assert_eq!(
        store.read::<ActiveAgentIdKey>(),
        Some(Some("reviewer".into()))
    );
    let state = store.read::<ActiveAgentKey>().unwrap();
    assert_eq!(state.active_agent.as_deref(), Some("reviewer"));

    // Now simulate deactivation (as orchestrator would do during handoff)
    let mut deactivate_patch = store.begin_mutation();
    for p in &env.plugins {
        p.on_deactivate(&mut deactivate_patch).unwrap();
    }
    store.commit(deactivate_patch).unwrap();

    // State should be fully cleared
    let state = store.read::<ActiveAgentKey>().unwrap();
    assert!(state.active_agent.is_none());
    assert!(state.requested_agent.is_none());
    assert_eq!(store.read::<ActiveAgentIdKey>(), Some(None));
}
