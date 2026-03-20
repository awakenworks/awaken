//! Configuration resolution — multi-source merge at execution boundaries.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use crate::state::{Snapshot, StateSlot};

use super::profile::{ActiveConfig, OsConfig, RunOverrides};
use super::spec::ConfigMap;

/// Built-in StateSlot: plugins write this via StateCommand to switch active profile.
/// Read during resolve_config to determine which profile to use.
pub struct ActiveProfileOverride;

impl StateSlot for ActiveProfileOverride {
    const KEY: &'static str = "__runtime.active_profile_override";
    type Value = Option<String>;
    type Update = Option<String>;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

/// Resolved configuration snapshot — immutable, short-lived, produced at execution boundaries.
#[derive(Debug, Clone)]
pub struct ResolvedPhaseConfig {
    pub active_plugins: HashSet<String>,
    pub config: Arc<ConfigMap>,
}

/// Resolve effective configuration from all sources.
///
/// Precedence (highest first):
/// 1. `RunOverrides` — per-call, not persisted
/// 2. State-driven profile override (`ActiveProfileOverride` slot)
/// 3. `ActiveConfig` — runtime baseline
/// 4. Active profile config (from `OsConfig.profiles`)
/// 5. `OsConfig.defaults` — global defaults
/// 6. `C::Value::default()` — type default (handled by `get_or_default`)
pub fn resolve_config(
    os_config: &OsConfig,
    active_config: &ActiveConfig,
    snapshot: &Snapshot,
    overrides: Option<&RunOverrides>,
) -> ResolvedPhaseConfig {
    // --- Resolve active_plugins ---
    let mut active_plugins: BTreeSet<String> = active_config.active_plugins.clone();

    // State-driven profile override
    let profile_override_id = snapshot
        .get::<ActiveProfileOverride>()
        .and_then(|v| v.as_ref().cloned());

    if let Some(ref profile_id) = profile_override_id
        && let Some(profile) = os_config.profiles.get(profile_id)
    {
        for id in &profile.active_plugins {
            active_plugins.insert(id.clone());
        }
    }

    // RunOverrides: add/remove
    if let Some(ovr) = overrides {
        for id in &ovr.activate_plugins {
            active_plugins.insert(id.clone());
        }
        for id in &ovr.deactivate_plugins {
            active_plugins.remove(id);
        }
    }

    // --- Resolve config ---
    // Start from OS defaults
    let mut config = os_config.defaults.clone();

    // Layer profile config (from state-derived profile or ActiveConfig's loaded profile)
    if let Some(ref profile_id) = profile_override_id
        && let Some(profile) = os_config.profiles.get(profile_id)
    {
        config.merge_from(&profile.config);
    }

    // Layer ActiveConfig
    config.merge_from(&active_config.config);

    // Layer RunOverrides
    if let Some(ovr) = overrides {
        config.merge_from(&ovr.config);
    }

    ResolvedPhaseConfig {
        active_plugins: active_plugins.into_iter().collect(),
        config: Arc::new(config),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::profile::AgentProfile;
    use crate::state::SlotMap;
    use serde::{Deserialize, Serialize};

    struct ModelConfig;
    impl crate::config::spec::ConfigSlot for ModelConfig {
        const KEY: &'static str = "test.model";
        type Value = ModelSettings;
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct ModelSettings {
        model: String,
    }

    struct TempConfig;
    impl crate::config::spec::ConfigSlot for TempConfig {
        const KEY: &'static str = "test.temp";
        type Value = TempSettings;
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct TempSettings {
        temperature: u32,
    }

    fn empty_snapshot() -> Snapshot {
        Snapshot::new(0, Arc::new(SlotMap::default()))
    }

    fn snapshot_with_profile_override(profile_id: &str) -> Snapshot {
        let mut map = SlotMap::default();
        map.insert::<ActiveProfileOverride>(Some(profile_id.to_string()));
        Snapshot::new(0, Arc::new(map))
    }

    #[test]
    fn resolve_with_no_sources_returns_defaults() {
        let os = OsConfig::default();
        let active = ActiveConfig::default();
        let snap = empty_snapshot();

        let resolved = resolve_config(&os, &active, &snap, None);
        assert!(resolved.active_plugins.is_empty());
        assert!(resolved.config.is_empty());
    }

    #[test]
    fn resolve_active_config_provides_baseline() {
        let os = OsConfig::default();
        let mut active = ActiveConfig::new();
        active.activate("perm");
        active.set::<ModelConfig>(ModelSettings {
            model: "gpt-4o".into(),
        });

        let resolved = resolve_config(&os, &active, &empty_snapshot(), None);
        assert!(resolved.active_plugins.contains("perm"));
        assert_eq!(
            resolved.config.get::<ModelConfig>().unwrap().model,
            "gpt-4o"
        );
    }

    #[test]
    fn resolve_os_defaults_are_lowest_precedence() {
        let mut os = OsConfig::new();
        os.set_default::<ModelConfig>(ModelSettings {
            model: "default-model".into(),
        });
        os.set_default::<TempConfig>(TempSettings { temperature: 50 });

        let mut active = ActiveConfig::new();
        active.set::<ModelConfig>(ModelSettings {
            model: "active-model".into(),
        });
        // TempConfig not set in active — should fall through to OS default

        let resolved = resolve_config(&os, &active, &empty_snapshot(), None);
        assert_eq!(
            resolved.config.get::<ModelConfig>().unwrap().model,
            "active-model"
        );
        assert_eq!(resolved.config.get::<TempConfig>().unwrap().temperature, 50);
    }

    #[test]
    fn resolve_state_driven_profile_override() {
        let mut os = OsConfig::new();
        os.register_profile(
            AgentProfile::new("reviewer")
                .activate("review-plugin")
                .configure::<ModelConfig>(ModelSettings {
                    model: "reviewer-model".into(),
                }),
        );

        let mut active = ActiveConfig::new();
        active.activate("base-plugin");
        active.set::<ModelConfig>(ModelSettings {
            model: "base-model".into(),
        });

        // State says: use "reviewer" profile
        let snap = snapshot_with_profile_override("reviewer");

        let resolved = resolve_config(&os, &active, &snap, None);

        // Profile plugins merged with active plugins
        assert!(resolved.active_plugins.contains("base-plugin"));
        assert!(resolved.active_plugins.contains("review-plugin"));

        // ActiveConfig overrides profile config (higher precedence)
        assert_eq!(
            resolved.config.get::<ModelConfig>().unwrap().model,
            "base-model"
        );
    }

    #[test]
    fn resolve_run_overrides_highest_precedence() {
        let mut os = OsConfig::new();
        os.set_default::<ModelConfig>(ModelSettings {
            model: "default".into(),
        });

        let mut active = ActiveConfig::new();
        active.activate("perm");
        active.set::<ModelConfig>(ModelSettings {
            model: "active".into(),
        });

        let mut overrides = RunOverrides::default();
        overrides.config.set::<ModelConfig>(ModelSettings {
            model: "override".into(),
        });
        overrides.activate_plugins.insert("extra".into());
        overrides.deactivate_plugins.insert("perm".into());

        let resolved = resolve_config(&os, &active, &empty_snapshot(), Some(&overrides));

        // Overrides win
        assert_eq!(
            resolved.config.get::<ModelConfig>().unwrap().model,
            "override"
        );
        // perm removed, extra added
        assert!(!resolved.active_plugins.contains("perm"));
        assert!(resolved.active_plugins.contains("extra"));
    }

    #[test]
    fn resolve_full_precedence_chain() {
        // Set up all layers
        let mut os = OsConfig::new();
        os.set_default::<TempConfig>(TempSettings { temperature: 10 });
        os.register_profile(
            AgentProfile::new("prof").configure::<TempConfig>(TempSettings { temperature: 20 }),
        );

        let mut active = ActiveConfig::new();
        active.set::<TempConfig>(TempSettings { temperature: 30 });

        let snap = snapshot_with_profile_override("prof");

        let mut overrides = RunOverrides::default();
        overrides
            .config
            .set::<TempConfig>(TempSettings { temperature: 40 });

        let resolved = resolve_config(&os, &active, &snap, Some(&overrides));

        // RunOverrides (40) > ActiveConfig (30) > Profile (20) > OsDefault (10)
        assert_eq!(resolved.config.get::<TempConfig>().unwrap().temperature, 40);
    }

    #[test]
    fn resolve_profile_override_nonexistent_profile_is_ignored() {
        let os = OsConfig::default();
        let active = ActiveConfig::default();
        let snap = snapshot_with_profile_override("does-not-exist");

        let resolved = resolve_config(&os, &active, &snap, None);
        assert!(resolved.active_plugins.is_empty());
    }

    #[test]
    fn resolve_active_profile_override_cleared_uses_baseline() {
        let mut os = OsConfig::new();
        os.register_profile(AgentProfile::new("prof").activate("prof-plugin"));

        let mut active = ActiveConfig::new();
        active.activate("base-plugin");

        // State has None (no override)
        let mut map = SlotMap::default();
        map.insert::<ActiveProfileOverride>(None);
        let snap = Snapshot::new(0, Arc::new(map));

        let resolved = resolve_config(&os, &active, &snap, None);
        assert!(resolved.active_plugins.contains("base-plugin"));
        assert!(!resolved.active_plugins.contains("prof-plugin"));
    }
}
