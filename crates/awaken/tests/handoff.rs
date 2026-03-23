#![allow(missing_docs)]
//! Integration tests validating dynamic configuration:
//! - Hook filtering by active_hook_filter in AgentSpec
//! - Spec sections accessible in hooks via ctx.agent_spec.sections
//! - Handoff via ActiveAgentIdKey (state-driven agent switch)
//! - Changing active_hook_filter between phases via different specs

use async_trait::async_trait;
use awaken::agent::state::{
    AccumulatedOverrides, AccumulatedToolExclusions, AccumulatedToolInclusions,
    ContextMessageStore, ContextThrottleState, RunLifecycle, ToolCallStates,
};
use awaken::contract::profile::ActiveAgentIdKey;
use awaken::registry::AgentSpec;
use awaken::*;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Tracking hooks — record what they see via spec sections
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
            .agent_spec
            .sections
            .get("test.model_name")
            .and_then(|v: &serde_json::Value| v.get("name"))
            .and_then(|v: &serde_json::Value| v.as_str())
            .unwrap_or("")
            .to_string();
        let greeting = ctx
            .agent_spec
            .sections
            .get("test.greeting")
            .and_then(|v: &serde_json::Value| v.get("prefix"))
            .and_then(|v: &serde_json::Value| v.as_str())
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
// Handoff hook — writes ActiveAgentIdKey to state
// ---------------------------------------------------------------------------

struct HandoffHook {
    target_agent: String,
}

#[async_trait]
impl PhaseHook for HandoffHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.update::<ActiveAgentIdKey>(Some(self.target_agent.clone()));
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
        r.register_key::<ContextThrottleState>(StateKeyOptions::default())?;
        r.register_key::<ActiveAgentIdKey>(StateKeyOptions::default())?;
        r.register_key::<AccumulatedOverrides>(StateKeyOptions::default())?;
        r.register_key::<ContextMessageStore>(StateKeyOptions::default())?;
        r.register_key::<AccumulatedToolExclusions>(StateKeyOptions::default())?;
        r.register_key::<AccumulatedToolInclusions>(StateKeyOptions::default())?;
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
    target_agent: String,
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
                target_agent: self.target_agent.clone(),
            },
        )?;
        Ok(())
    }
}

/// Build an AgentSpec with the given active_hook_filter set.
fn spec_with_plugins(plugins: &[&str]) -> Arc<AgentSpec> {
    let mut active_hook_filter = HashSet::new();
    for p in plugins {
        active_hook_filter.insert((*p).to_string());
    }
    Arc::new(AgentSpec {
        active_hook_filter,
        ..AgentSpec::default()
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Only hooks whose plugin_id is in active_hook_filter should fire.
#[tokio::test]
async fn hook_filtering_only_active_hook_filter_fire() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.store().install_plugin(LoopStatePlugin).unwrap();

    let tracker = Arc::new(MultiTrackerPlugin {
        trackers: vec![
            ("alpha", vec![Phase::BeforeInference]),
            ("beta", vec![Phase::BeforeInference]),
        ],
        log: log.clone(),
    });
    let env = ExecutionEnv::from_plugins(&[tracker as Arc<dyn Plugin>]).unwrap();

    // Build a spec that only activates "alpha"
    let spec = spec_with_plugins(&["alpha"]);

    let ctx =
        PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot()).with_agent_spec(spec);
    runtime.run_phase_with_context(&env, ctx).await.unwrap();

    let entries = log.entries.lock().unwrap();
    let before_inf: Vec<_> = entries
        .iter()
        .filter(|e| e.phase == Phase::BeforeInference)
        .collect();

    assert_eq!(before_inf.len(), 1);
    assert_eq!(before_inf[0].plugin_id, "alpha");
}

/// When active_hook_filter is empty, all hooks run (no filtering).
#[tokio::test]
async fn empty_active_hook_filter_runs_all_hooks() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.store().install_plugin(LoopStatePlugin).unwrap();

    let tracker = Arc::new(MultiTrackerPlugin {
        trackers: vec![
            ("alpha", vec![Phase::BeforeInference]),
            ("beta", vec![Phase::BeforeInference]),
        ],
        log: log.clone(),
    });
    let env = ExecutionEnv::from_plugins(&[tracker as Arc<dyn Plugin>]).unwrap();

    // Default spec has empty active_hook_filter — no filtering, all hooks run
    let spec = Arc::new(AgentSpec::default());

    let ctx =
        PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot()).with_agent_spec(spec);
    runtime.run_phase_with_context(&env, ctx).await.unwrap();

    let entries = log.entries.lock().unwrap();
    let before_inf: Vec<_> = entries
        .iter()
        .filter(|e| e.phase == Phase::BeforeInference)
        .collect();

    assert_eq!(before_inf.len(), 2); // Both alpha and beta fire
}

/// Spec sections are accessible in hooks via ctx.agent_spec.sections.
/// Overridden sections take precedence over defaults.
#[tokio::test]
async fn config_values_accessible_in_hooks() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.store().install_plugin(LoopStatePlugin).unwrap();

    let tracker = Arc::new(SingleTrackerPlugin {
        id: "tracker",
        log: log.clone(),
        phases: vec![Phase::BeforeInference],
    });
    let env = ExecutionEnv::from_plugins(&[tracker as Arc<dyn Plugin>]).unwrap();

    // Build a spec with sections for model_name and greeting
    let spec = Arc::new(
        AgentSpec::new("test")
            .with_section("test.model_name", json!({"name": "custom-model"}))
            .with_section("test.greeting", json!({"prefix": "Hello"})),
    );

    let ctx =
        PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot()).with_agent_spec(spec);
    runtime.run_phase_with_context(&env, ctx).await.unwrap();

    let entries = log.entries.lock().unwrap();
    let entry = entries
        .iter()
        .find(|e| e.phase == Phase::BeforeInference)
        .unwrap();

    assert_eq!(entry.model_name, "custom-model");
    assert_eq!(entry.greeting, "Hello");
}

/// Handoff hook writes ActiveAgentIdKey; at the next phase boundary the
/// runtime resolves the new spec from the registry. The new spec's
/// active_hook_filter and sections take effect.
#[tokio::test]
async fn handoff_switches_spec_at_next_boundary() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.store().install_plugin(LoopStatePlugin).unwrap();

    let handoff_plugin = Arc::new(HandoffPlugin {
        target_agent: "reviewer".into(),
    });
    let tracker = Arc::new(SingleTrackerPlugin {
        id: "review-tracker",
        log: log.clone(),
        phases: vec![Phase::BeforeInference],
    });
    let plugins: Vec<Arc<dyn Plugin>> = vec![handoff_plugin, tracker];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    // Build a reviewer spec with sections
    let reviewer_spec = Arc::new(
        AgentSpec::new("reviewer")
            .with_hook_filter("review-tracker")
            .with_section("test.model_name", json!({"name": "reviewer-model"})),
    );

    // Phase 1: RunStart with a spec that only activates "handoff"
    // The handoff hook writes ActiveAgentIdKey = "reviewer"
    let handoff_spec = spec_with_plugins(&["handoff"]);
    let run_start_ctx = PhaseContext::new(Phase::RunStart, runtime.store().snapshot())
        .with_agent_spec(handoff_spec);
    runtime
        .run_phase_with_context(&env, run_start_ctx)
        .await
        .unwrap();

    // After RunStart, read ActiveAgentIdKey from state
    let active_id = runtime
        .store()
        .read::<ActiveAgentIdKey>()
        .and_then(|v| v.clone());
    assert_eq!(active_id.as_deref(), Some("reviewer"));

    // Phase 2: BeforeInference with the resolved reviewer spec
    // "review-tracker" should now be active and see "reviewer-model" in sections
    let before_inf_ctx = PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot())
        .with_agent_spec(reviewer_spec);
    runtime
        .run_phase_with_context(&env, before_inf_ctx)
        .await
        .unwrap();

    let entries = log.entries.lock().unwrap();
    let before_inf: Vec<_> = entries
        .iter()
        .filter(|e| e.phase == Phase::BeforeInference)
        .collect();

    assert_eq!(before_inf.len(), 1);
    assert_eq!(before_inf[0].plugin_id, "review-tracker");
    assert_eq!(before_inf[0].model_name, "reviewer-model");
}

/// Switching to a spec without the tracker in active_hook_filter
/// effectively deactivates it mid-run.
#[tokio::test]
async fn deactivate_plugin_mid_run_via_configure() {
    let log = HookLog::default();

    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    runtime.store().install_plugin(LoopStatePlugin).unwrap();

    let tracker = Arc::new(SingleTrackerPlugin {
        id: "tracker",
        log: log.clone(),
        phases: vec![Phase::RunStart, Phase::BeforeInference, Phase::RunEnd],
    });
    let env = ExecutionEnv::from_plugins(&[tracker as Arc<dyn Plugin>]).unwrap();

    // Phase 1: RunStart with tracker active
    let spec_with_tracker = spec_with_plugins(&["tracker"]);
    let ctx = PhaseContext::new(Phase::RunStart, runtime.store().snapshot())
        .with_agent_spec(spec_with_tracker);
    runtime.run_phase_with_context(&env, ctx).await.unwrap();

    {
        let entries = log.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].phase, Phase::RunStart);
    }

    // Phase 2: BeforeInference with a spec that does NOT include "tracker"
    // (non-empty active_hook_filter without "tracker" means tracker is filtered out)
    let spec_without_tracker = spec_with_plugins(&["other-plugin"]);
    let ctx = PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot())
        .with_agent_spec(spec_without_tracker);
    runtime.run_phase_with_context(&env, ctx).await.unwrap();

    {
        let entries = log.entries.lock().unwrap();
        assert_eq!(entries.len(), 1); // Still just the RunStart entry
    }
}
