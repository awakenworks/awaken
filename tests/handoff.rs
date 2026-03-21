#![allow(missing_docs)]
//! Integration tests validating dynamic configuration:
//! - Hook filtering by active_plugins in AgentProfile
//! - Profile sections accessible in hooks via ctx.profile.sections
//! - Handoff via ActiveAgentKey (state-driven profile switch)
//! - Changing active_plugins between phases via different profiles

use async_trait::async_trait;
use awaken::agent::state::{RunLifecycle, ToolCallStates};
use awaken::contract::profile::{ActiveAgentKey, AgentProfile, AgentRegistry, MapAgentRegistry};
use awaken::*;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Tracking hooks — record what they see via profile sections
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct HookLog {
    entries: Arc<Mutex<Vec<HookEntry>>>,
}

#[derive(Debug, Clone)]
struct HookEntry {
    plugin_id: String,
    phase: Phase,
    model_name: String,
    greeting: String,
}

struct TrackingHook {
    plugin_id: String,
    log: HookLog,
}

#[async_trait]
impl PhaseHook for TrackingHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let model_name = ctx
            .profile
            .sections
            .get("test.model_name")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let greeting = ctx
            .profile
            .sections
            .get("test.greeting")
            .and_then(|v| v.get("prefix"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.log.entries.lock().unwrap().push(HookEntry {
            plugin_id: self.plugin_id.clone(),
            phase: ctx.phase,
            model_name,
            greeting,
        });
        Ok(StateCommand::new())
    }
}

// ---------------------------------------------------------------------------
// Handoff hook — writes ActiveAgentKey to state
// ---------------------------------------------------------------------------

struct HandoffHook {
    target_profile: String,
}

#[async_trait]
impl PhaseHook for HandoffHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.update::<ActiveAgentKey>(Some(self.target_profile.clone()));
        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// Plugin wrappers
// ---------------------------------------------------------------------------

struct LoopStatePlugin;
impl Plugin for LoopStatePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "loop-state" }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_key::<RunLifecycle>(StateKeyOptions::default())?;
        r.register_key::<ToolCallStates>(StateKeyOptions::default())?;
        r.register_key::<ActiveAgentKey>(StateKeyOptions::default())?;
        Ok(())
    }
}

/// A single plugin that registers hooks for multiple plugin_ids.
/// Avoids TypeId conflicts from multiple instances of the same struct.
struct MultiTrackerPlugin {
    trackers: Vec<(&'static str, Vec<Phase>)>,
    log: HookLog,
}

impl Plugin for MultiTrackerPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "multi-tracker",
        }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        for (id, phases) in &self.trackers {
            for &phase in phases {
                r.register_phase_hook(
                    *id,
                    phase,
                    TrackingHook {
                        plugin_id: (*id).into(),
                        log: self.log.clone(),
                    },
                )?;
            }
        }
        Ok(())
    }
}

/// Single-id tracker for simple cases.
struct SingleTrackerPlugin {
    id: &'static str,
    log: HookLog,
    phases: Vec<Phase>,
}

impl Plugin for SingleTrackerPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: self.id }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        for &phase in &self.phases {
            r.register_phase_hook(
                self.id,
                phase,
                TrackingHook {
                    plugin_id: self.id.into(),
                    log: self.log.clone(),
                },
            )?;
        }
        Ok(())
    }
}

struct HandoffPlugin {
    target_profile: String,
}

impl Plugin for HandoffPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "handoff" }
    }
    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_phase_hook(
            "handoff",
            Phase::RunStart,
            HandoffHook {
                target_profile: self.target_profile.clone(),
            },
        )?;
        Ok(())
    }
}

/// Build an AgentProfile with the given active_plugins set.
fn profile_with_plugins(plugins: &[&str]) -> Arc<AgentProfile> {
    let mut active_plugins = HashSet::new();
    for p in plugins {
        active_plugins.insert((*p).to_string());
    }
    Arc::new(AgentProfile {
        active_plugins,
        ..AgentProfile::default()
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Only hooks whose plugin_id is in active_plugins should fire.
#[tokio::test]
async fn hook_filtering_only_active_plugins_fire() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.install_plugin(LoopStatePlugin).unwrap();

    // Install tracker with two plugin_ids
    runtime
        .install_plugin(MultiTrackerPlugin {
            trackers: vec![
                ("alpha", vec![Phase::BeforeInference]),
                ("beta", vec![Phase::BeforeInference]),
            ],
            log: log.clone(),
        })
        .unwrap();

    // Build a profile that only activates "alpha"
    let profile = profile_with_plugins(&["alpha"]);

    let ctx =
        PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot()).with_profile(profile);
    runtime.run_phase_with_context(ctx).await.unwrap();

    let entries = log.entries.lock().unwrap();
    let before_inf: Vec<_> = entries
        .iter()
        .filter(|e| e.phase == Phase::BeforeInference)
        .collect();

    assert_eq!(before_inf.len(), 1);
    assert_eq!(before_inf[0].plugin_id, "alpha");
}

/// When active_plugins is empty, all hooks run (no filtering).
#[tokio::test]
async fn empty_active_plugins_runs_all_hooks() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.install_plugin(LoopStatePlugin).unwrap();

    runtime
        .install_plugin(MultiTrackerPlugin {
            trackers: vec![
                ("alpha", vec![Phase::BeforeInference]),
                ("beta", vec![Phase::BeforeInference]),
            ],
            log: log.clone(),
        })
        .unwrap();

    // Default profile has empty active_plugins — no filtering, all hooks run
    let profile = Arc::new(AgentProfile::default());

    let ctx =
        PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot()).with_profile(profile);
    runtime.run_phase_with_context(ctx).await.unwrap();

    let entries = log.entries.lock().unwrap();
    let before_inf: Vec<_> = entries
        .iter()
        .filter(|e| e.phase == Phase::BeforeInference)
        .collect();

    assert_eq!(before_inf.len(), 2); // Both alpha and beta fire
}

/// Profile sections are accessible in hooks via ctx.profile.sections.
/// Overridden sections take precedence over defaults.
#[tokio::test]
async fn config_values_accessible_in_hooks() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.install_plugin(LoopStatePlugin).unwrap();

    runtime
        .install_plugin(SingleTrackerPlugin {
            id: "tracker",
            log: log.clone(),
            phases: vec![Phase::BeforeInference],
        })
        .unwrap();

    // Build a profile with sections for model_name (overridden) and greeting (default)
    let profile = Arc::new(
        AgentProfile::new("test")
            .with_section("test.model_name", json!({"name": "custom-model"}))
            .with_section("test.greeting", json!({"prefix": "Hello"})),
    );

    let ctx =
        PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot()).with_profile(profile);
    runtime.run_phase_with_context(ctx).await.unwrap();

    let entries = log.entries.lock().unwrap();
    let entry = entries
        .iter()
        .find(|e| e.phase == Phase::BeforeInference)
        .unwrap();

    // ModelName set via profile section
    assert_eq!(entry.model_name, "custom-model");
    // Greeting set via profile section
    assert_eq!(entry.greeting, "Hello");
}

/// Handoff hook writes ActiveAgentKey; at the next phase boundary the
/// runtime resolves the new profile from the registry. The new profile's
/// active_plugins and sections take effect.
#[tokio::test]
async fn handoff_switches_profile_at_next_boundary() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.install_plugin(LoopStatePlugin).unwrap();

    // Handoff plugin writes ActiveAgentKey at RunStart
    runtime
        .install_plugin(HandoffPlugin {
            target_profile: "reviewer".into(),
        })
        .unwrap();

    // Tracker registered as "review-tracker" — only active when reviewer profile is active
    runtime
        .install_plugin(SingleTrackerPlugin {
            id: "review-tracker",
            log: log.clone(),
            phases: vec![Phase::BeforeInference],
        })
        .unwrap();

    // Build a registry with the "reviewer" profile
    let mut registry = MapAgentRegistry::new();
    registry.register(
        AgentProfile::new("reviewer")
            .with_active_plugin("review-tracker")
            .with_section("test.model_name", json!({"name": "reviewer-model"})),
    );

    // Phase 1: RunStart with a profile that only activates "handoff"
    // The handoff hook writes ActiveAgentKey = "reviewer"
    let handoff_profile = profile_with_plugins(&["handoff"]);
    let run_start_ctx = PhaseContext::new(Phase::RunStart, runtime.store().snapshot())
        .with_profile(handoff_profile);
    runtime.run_phase_with_context(run_start_ctx).await.unwrap();

    // After RunStart, read ActiveAgentKey from state to resolve the new profile
    let active_id = runtime
        .store()
        .read::<ActiveAgentKey>()
        .and_then(|v| v.clone());
    assert_eq!(active_id.as_deref(), Some("reviewer"));

    // Resolve the reviewer profile from the registry
    let reviewer_profile = Arc::new(registry.get(active_id.as_deref().unwrap()).unwrap().clone());

    // Phase 2: BeforeInference with the resolved reviewer profile
    // "review-tracker" should now be active and see "reviewer-model" in sections
    let before_inf_ctx = PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot())
        .with_profile(reviewer_profile);
    runtime
        .run_phase_with_context(before_inf_ctx)
        .await
        .unwrap();

    let entries = log.entries.lock().unwrap();
    let before_inf: Vec<_> = entries
        .iter()
        .filter(|e| e.phase == Phase::BeforeInference)
        .collect();

    assert_eq!(before_inf.len(), 1);
    assert_eq!(before_inf[0].plugin_id, "review-tracker");
    // Config comes from profile section (reviewer-model)
    assert_eq!(before_inf[0].model_name, "reviewer-model");
}

/// Switching to a profile without the tracker in active_plugins
/// effectively deactivates it mid-run.
#[tokio::test]
async fn deactivate_plugin_mid_run_via_configure() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.install_plugin(LoopStatePlugin).unwrap();

    runtime
        .install_plugin(SingleTrackerPlugin {
            id: "tracker",
            log: log.clone(),
            phases: vec![Phase::RunStart, Phase::BeforeInference, Phase::RunEnd],
        })
        .unwrap();

    // Phase 1: RunStart with tracker active
    let profile_with_tracker = profile_with_plugins(&["tracker"]);
    let ctx = PhaseContext::new(Phase::RunStart, runtime.store().snapshot())
        .with_profile(profile_with_tracker);
    runtime.run_phase_with_context(ctx).await.unwrap();

    {
        let entries = log.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].phase, Phase::RunStart);
    }

    // Phase 2: BeforeInference with a profile that does NOT include "tracker"
    // (non-empty active_plugins without "tracker" means tracker is filtered out)
    let profile_without_tracker = profile_with_plugins(&["other-plugin"]);
    let ctx = PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot())
        .with_profile(profile_without_tracker);
    runtime.run_phase_with_context(ctx).await.unwrap();

    {
        let entries = log.entries.lock().unwrap();
        assert_eq!(entries.len(), 1); // Still just the RunStart entry
    }
}
