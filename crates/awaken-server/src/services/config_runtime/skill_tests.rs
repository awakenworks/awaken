use std::sync::Arc;

use awaken_contract::{BuiltinSeedSet, BuiltinSpec, SkillSpec, SkillSpecSink};
use parking_lot::Mutex;

use super::tests::make_manager_with_store;

#[derive(Default)]
struct RecordingSkillSpecSink {
    specs: Mutex<Vec<SkillSpec>>,
    replace_calls: Mutex<usize>,
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
        *self.replace_calls.lock() += 1;
        Ok(())
    }
}

fn skill_spec(id: &str) -> SkillSpec {
    SkillSpec {
        id: id.into(),
        name: "Database Management".into(),
        description: "Helps with database operations".into(),
        instructions_md: "Inspect schema before running SQL.".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn apply_publishes_config_managed_skills_to_sink() {
    let (manager, store) = make_manager_with_store().await;
    let sink = Arc::new(RecordingSkillSpecSink::default());
    let manager = manager.with_skill_spec_sink(sink.clone());

    let seed = BuiltinSeedSet {
        binary_version: "test".into(),
        specs: vec![BuiltinSpec::skill(skill_spec("db-management"))],
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

#[tokio::test]
async fn apply_removes_deleted_config_managed_skills_from_sink() {
    let (manager, store) = make_manager_with_store().await;
    let sink = Arc::new(RecordingSkillSpecSink::default());
    let manager = manager.with_skill_spec_sink(sink.clone());

    manager
        .apply_seed(&BuiltinSeedSet {
            binary_version: "test".into(),
            specs: vec![BuiltinSpec::skill(skill_spec("db-management"))],
        })
        .await
        .expect("apply seed");
    manager.apply().await.expect("publish managed skills");
    assert_eq!(sink.specs.lock().len(), 1);

    store
        .delete("skills", "db-management")
        .await
        .expect("delete skill");
    manager.apply().await.expect("publish removal");

    assert!(sink.specs.lock().is_empty());
    assert_eq!(*sink.replace_calls.lock(), 2);
}

#[tokio::test]
async fn apply_rejects_duplicate_skill_ids_before_replacing_sink() {
    let (manager, store) = make_manager_with_store().await;
    let sink = Arc::new(RecordingSkillSpecSink::default());
    let manager = manager.with_skill_spec_sink(sink.clone());

    store
        .put(
            "skills",
            "primary",
            &serde_json::to_value(skill_spec("db-management")).expect("serialize skill"),
        )
        .await
        .expect("write primary skill");
    manager.apply().await.expect("publish primary");
    assert_eq!(sink.specs.lock().len(), 1);
    assert_eq!(*sink.replace_calls.lock(), 1);

    store
        .put(
            "skills",
            "duplicate-key",
            &serde_json::to_value(skill_spec("db-management")).expect("serialize skill"),
        )
        .await
        .expect("write duplicate skill");

    let error = manager
        .apply()
        .await
        .expect_err("duplicate skill id must fail publish");
    assert!(
        error
            .to_string()
            .contains("duplicate skill id: db-management"),
        "unexpected error: {error}"
    );
    assert_eq!(
        sink.specs.lock()[0].id,
        "db-management",
        "failed publish must leave the previous live specs intact"
    );
    assert_eq!(
        *sink.replace_calls.lock(),
        1,
        "validation failure must happen before replacing live specs"
    );
}
