use std::sync::Arc;

use awaken_contract::{BuiltinSeedSet, BuiltinSpec, SkillSpec, SkillSpecSink};
use parking_lot::Mutex;

use super::tests::make_manager_with_store;

#[derive(Default)]
struct RecordingSkillSpecSink {
    specs: Mutex<Vec<SkillSpec>>,
}

impl SkillSpecSink for RecordingSkillSpecSink {
    fn validate_skill_specs(&self, specs: &[SkillSpec]) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        for spec in specs {
            if !ids.insert(spec.id.clone()) {
                return Err(format!("duplicate skill id: {}", spec.id));
            }
        }
        Ok(())
    }

    fn replace_skill_specs(&self, specs: Vec<SkillSpec>) -> Result<(), String> {
        *self.specs.lock() = specs;
        Ok(())
    }
}

#[tokio::test]
async fn apply_publishes_config_managed_skills_to_sink() {
    let (manager, store) = make_manager_with_store().await;
    let sink = Arc::new(RecordingSkillSpecSink::default());
    let manager = manager.with_skill_spec_sink(sink.clone());

    let seed = BuiltinSeedSet {
        binary_version: "test".into(),
        specs: vec![BuiltinSpec::skill(SkillSpec {
            id: "db-management".into(),
            name: "Database Management".into(),
            description: "Helps with database operations".into(),
            instructions_md: "Inspect schema before running SQL.".into(),
            ..Default::default()
        })],
    };

    manager.apply_seed(&seed).await.expect("apply seed");
    manager.apply().await.expect("publish managed skills");

    let specs = sink.specs.lock().clone();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id, "db-management");
    assert!(
        store
            .get("skills", "db-management")
            .await
            .expect("read skill")
            .is_some()
    );
}
