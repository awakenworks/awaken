use std::sync::Arc;

use awaken_contract::{BuiltinSeedSet, BuiltinSpec, PreparedSkillSpecs, SkillSpec, SkillSpecSink};
use parking_lot::Mutex;

use super::tests::make_manager_with_store;

#[derive(Default)]
struct RecordingSkillSpecSink {
    specs: Arc<Mutex<Vec<SkillSpec>>>,
    replace_calls: Arc<Mutex<usize>>,
    fail_prepare: Arc<Mutex<Option<String>>>,
}

impl SkillSpecSink for RecordingSkillSpecSink {
    fn prepare_skill_specs(
        &self,
        specs: Vec<SkillSpec>,
    ) -> Result<Box<dyn PreparedSkillSpecs>, String> {
        if let Some(error) = self.fail_prepare.lock().clone() {
            return Err(error);
        }
        let mut ids = std::collections::HashSet::new();
        for spec in &specs {
            if !ids.insert(spec.id.clone()) {
                return Err(format!("duplicate skill id: {}", spec.id));
            }
        }
        Ok(Box::new(RecordingPreparedSkillSpecs {
            specs_target: Arc::clone(&self.specs),
            replace_calls: Arc::clone(&self.replace_calls),
            specs,
        }))
    }
}

struct RecordingPreparedSkillSpecs {
    specs_target: Arc<Mutex<Vec<SkillSpec>>>,
    replace_calls: Arc<Mutex<usize>>,
    specs: Vec<SkillSpec>,
}

impl PreparedSkillSpecs for RecordingPreparedSkillSpecs {
    fn commit(self: Box<Self>) {
        *self.specs_target.lock() = self.specs;
        *self.replace_calls.lock() += 1;
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

#[tokio::test]
async fn apply_rejects_config_managed_skills_without_sink() {
    let (manager, _store) = make_manager_with_store().await;
    let before_version = manager.runtime.registry_version();

    manager
        .apply_seed(&BuiltinSeedSet {
            binary_version: "test".into(),
            specs: vec![BuiltinSpec::skill(skill_spec("db-management"))],
        })
        .await
        .expect("apply seed");

    let error = manager
        .apply()
        .await
        .expect_err("skills require a live skill sink");
    assert!(
        error.to_string().contains("skill_spec_sink"),
        "unexpected error: {error}"
    );
    assert_eq!(
        manager.runtime.registry_version(),
        before_version,
        "failed skill publish must not replace the core runtime registry"
    );
}

#[tokio::test]
async fn skill_prepare_failure_happens_before_runtime_registry_replace() {
    let (manager, store) = make_manager_with_store().await;
    let sink = Arc::new(RecordingSkillSpecSink::default());
    *sink.fail_prepare.lock() = Some("prepared skill map failed".into());
    let manager = manager.with_skill_spec_sink(sink.clone());
    let before_version = manager.runtime.registry_version();

    store
        .put(
            "skills",
            "db-management",
            &serde_json::to_value(skill_spec("db-management")).expect("serialize skill"),
        )
        .await
        .expect("write skill");

    let error = manager
        .apply()
        .await
        .expect_err("prepare failure must fail publish");
    assert!(
        error.to_string().contains("prepared skill map failed"),
        "unexpected error: {error}"
    );
    assert_eq!(
        manager.runtime.registry_version(),
        before_version,
        "prepare failure must be atomic with respect to runtime registry replacement"
    );
    assert_eq!(*sink.replace_calls.lock(), 0);
}

#[tokio::test]
async fn apply_rejects_invalid_skill_allowed_tool_pattern_before_replace() {
    let (manager, store) = make_manager_with_store().await;
    let sink = Arc::new(RecordingSkillSpecSink::default());
    let manager = manager.with_skill_spec_sink(sink.clone());
    let before_version = manager.runtime.registry_version();

    let mut invalid_regex = skill_spec("invalid-regex-skill");
    invalid_regex.allowed_tools = vec!["/[invalid/".into()];
    store
        .put(
            "skills",
            "invalid-regex-skill",
            &serde_json::to_value(invalid_regex).expect("serialize skill"),
        )
        .await
        .expect("write invalid skill");

    let error = manager
        .apply()
        .await
        .expect_err("invalid skill matcher must fail publish");
    assert!(
        error.to_string().contains("invalid allowed-tools pattern"),
        "unexpected error: {error}"
    );
    assert_eq!(
        manager.runtime.registry_version(),
        before_version,
        "invalid skill matcher must not replace the core runtime registry"
    );
    assert_eq!(*sink.replace_calls.lock(), 0);

    store
        .delete("skills", "invalid-regex-skill")
        .await
        .expect("delete invalid regex skill");

    let mut invalid_glob = skill_spec("invalid-glob-skill");
    invalid_glob.allowed_tools = vec![r"mcp__db__*\".into()];
    store
        .put(
            "skills",
            "invalid-glob-skill",
            &serde_json::to_value(invalid_glob).expect("serialize skill"),
        )
        .await
        .expect("write invalid skill");

    let error = manager
        .apply()
        .await
        .expect_err("invalid skill glob must fail publish");
    assert!(
        error.to_string().contains("dangling escape"),
        "unexpected error: {error}"
    );
    assert_eq!(manager.runtime.registry_version(), before_version);
    assert_eq!(*sink.replace_calls.lock(), 0);
}
