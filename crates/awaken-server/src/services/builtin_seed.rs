//! Protocol for applying a [`BuiltinSeedSet`] to a [`ConfigStore`].
//!
//! See [`apply_builtin_seed`] for the full semantics.

use std::collections::{HashMap, HashSet};

use awaken_contract::contract::storage::StorageError;
use awaken_contract::{
    BuiltinSeedSet, BuiltinSpec, ConfigRecord, ConfigStore, RecordMeta, RecordSource,
};

use crate::services::config_service::ConfigNamespace;

const SEED_LIST_PAGE_SIZE: usize = 256;

// ── public types ─────────────────────────────────────────────────────────────

/// Report produced by [`apply_builtin_seed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub created: Vec<RecordRef>,
    pub updated: Vec<RecordRef>,
    pub unchanged: Vec<RecordRef>,
    pub deleted: Vec<RecordRef>,
    pub preserved_user: Vec<RecordRef>,
}

/// Identifies a single record in a ConfigStore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRef {
    pub namespace: String,
    pub id: String,
}

impl RecordRef {
    fn new(namespace: &str, id: &str) -> Self {
        Self {
            namespace: namespace.to_owned(),
            id: id.to_owned(),
        }
    }
}

/// Errors returned by [`apply_builtin_seed`].
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

// ── apply_builtin_seed ────────────────────────────────────────────────────────

/// Apply a seed to the given ConfigStore.
///
/// Behavior per spec in `seed.specs`:
/// - No existing record → create new Builtin record. (created)
/// - Existing Builtin, same binary_version, spec equal → no-op. (unchanged)
/// - Existing Builtin, same binary_version, spec differs → replace spec, refresh updated_at. (updated)
/// - Existing Builtin, different binary_version → replace spec + version, preserve hidden, refresh updated_at. (updated)
/// - Existing User → leave entirely untouched. (preserved_user)
///
/// After processing seed entries, scans all four spec namespaces
/// (`agents`, `providers`, `models`, `mcp-servers`) and deletes any
/// Builtin record whose ID is not in this seed (orphan cleanup).
/// User records are never deleted by orphan cleanup.
///
/// **Concurrency precondition:** Must not run concurrently with any other
/// writer to the four spec namespaces (`agents`, `providers`, `models`,
/// `mcp-servers`). Intended for boot-time invocation before
/// `ConfigRuntimeManager::apply()`. With this precondition, the
/// snapshot-then-delete orphan-cleanup pattern is safe.
pub async fn apply_builtin_seed(
    store: &dyn ConfigStore,
    seed: &BuiltinSeedSet,
) -> Result<SeedReport, SeedError> {
    let mut report = SeedReport::default();

    // Track seeded (namespace, id) pairs for orphan cleanup.
    let mut seeded: HashMap<&str, HashSet<String>> = HashMap::new();
    for ns in ConfigNamespace::iter_str() {
        seeded.insert(ns, HashSet::new());
    }

    // ── Phase 1: upsert seed entries ────────────────────────────────────────
    for spec in &seed.specs {
        let namespace = spec.namespace();
        let id = spec.id();
        let new_spec_value = builtin_spec_to_value(spec)?;

        seeded.entry(namespace).or_default().insert(id.to_owned());

        let existing_raw = store.get(namespace, id).await?;

        match existing_raw {
            None => {
                // Create new Builtin record.
                let record = ConfigRecord {
                    spec: new_spec_value,
                    meta: RecordMeta::new_builtin(&seed.binary_version),
                };
                store.put(namespace, id, &record.to_value()?).await?;
                report.created.push(RecordRef::new(namespace, id));
            }
            Some(raw) => {
                let existing: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw)?;

                match &existing.meta.source {
                    RecordSource::User => {
                        // Never touch user records.
                        report.preserved_user.push(RecordRef::new(namespace, id));
                    }
                    RecordSource::Builtin {
                        binary_version: stored_version,
                    } => {
                        let same_version = stored_version == &seed.binary_version;
                        let same_spec = existing.spec == new_spec_value;

                        if same_version && same_spec {
                            // No-op.
                            report.unchanged.push(RecordRef::new(namespace, id));
                        } else {
                            // Update: refresh spec and/or version; preserve
                            // hidden flag, user_overrides, and created_at.
                            let now = awaken_contract::time::now_ms();
                            let record = ConfigRecord {
                                spec: new_spec_value,
                                meta: RecordMeta {
                                    source: RecordSource::Builtin {
                                        binary_version: seed.binary_version.clone(),
                                    },
                                    hidden: existing.meta.hidden,
                                    user_overrides: existing.meta.user_overrides,
                                    created_at: existing.meta.created_at,
                                    updated_at: now,
                                },
                            };
                            store.put(namespace, id, &record.to_value()?).await?;
                            report.updated.push(RecordRef::new(namespace, id));
                        }
                    }
                }
            }
        }
    }

    // ── Phase 2: orphan cleanup ──────────────────────────────────────────────
    //
    // Two-pass snapshot-then-delete to avoid the pagination skew that
    // interleaved deletes would cause: deleting a record shifts later entries
    // forward in the store's ordering, so a single combined loop would skip
    // records that move into already-visited slots.
    //
    // Pass 1 (read-only): collect all deletion candidates into a Vec.
    // Pass 2 (write): delete each candidate.
    //
    // Safe under the boot-time single-writer precondition documented above.
    for namespace in ConfigNamespace::iter_str() {
        let empty = HashSet::new();
        let seeded_ids: &HashSet<String> = seeded.get(namespace).unwrap_or(&empty);

        // Pass 1: snapshot deletion candidates.
        let mut candidates: Vec<String> = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = store.list(namespace, offset, SEED_LIST_PAGE_SIZE).await?;
            let page_len = page.len();

            for (id, raw) in page {
                if seeded_ids.contains(&id) {
                    continue;
                }
                // Decode to check source; legacy bare-spec becomes User.
                let record: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw)?;
                if matches!(record.meta.source, RecordSource::Builtin { .. }) {
                    candidates.push(id);
                }
                // User records (including legacy-bare ones) are left alone.
            }

            if page_len < SEED_LIST_PAGE_SIZE {
                break;
            }
            offset += page_len;
        }

        // Pass 2: delete all candidates collected above.
        for id in candidates {
            store.delete(namespace, &id).await?;
            report.deleted.push(RecordRef::new(namespace, &id));
        }
    }

    Ok(report)
}

// ── helper ───────────────────────────────────────────────────────────────────

/// Extract the inner spec JSON from a [`BuiltinSpec`].
///
/// The wire format stored in the envelope's `spec` field is the plain inner
/// spec (e.g. `AgentSpec` JSON), not the tagged `BuiltinSpec` form.
fn builtin_spec_to_value(spec: &BuiltinSpec) -> Result<serde_json::Value, serde_json::Error> {
    match spec {
        BuiltinSpec::Agent(s) => serde_json::to_value(s.as_ref()),
        BuiltinSpec::Provider(s) => serde_json::to_value(s),
        BuiltinSpec::Model(s) => serde_json::to_value(s),
        BuiltinSpec::McpServer(s) => serde_json::to_value(s),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::config_record::ConfigRecord;
    use awaken_contract::{AgentSpec, McpServerSpec, ModelBindingSpec, ProviderSpec};
    use awaken_stores::memory::InMemoryStore;

    // ── spec constructors ────────────────────────────────────────────────────

    fn agent_spec(id: &str, prompt: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            model_id: "gpt-4o".to_owned(),
            system_prompt: prompt.to_owned(),
            ..Default::default()
        }
    }

    fn provider_spec(id: &str) -> ProviderSpec {
        ProviderSpec {
            id: id.to_owned(),
            adapter: "openai".to_owned(),
            ..Default::default()
        }
    }

    fn model_spec(id: &str) -> ModelBindingSpec {
        ModelBindingSpec {
            id: id.to_owned(),
            provider_id: "openai".to_owned(),
            upstream_model: "gpt-4o".to_owned(),
            created_at: None,
            updated_at: None,
        }
    }

    fn mcp_spec(id: &str) -> McpServerSpec {
        McpServerSpec {
            id: id.to_owned(),
            ..Default::default()
        }
    }

    fn seed_v1(specs: Vec<BuiltinSpec>) -> BuiltinSeedSet {
        BuiltinSeedSet {
            binary_version: "v1".to_owned(),
            specs,
        }
    }

    fn seed_v2(specs: Vec<BuiltinSpec>) -> BuiltinSeedSet {
        BuiltinSeedSet {
            binary_version: "v2".to_owned(),
            specs,
        }
    }

    fn store() -> InMemoryStore {
        InMemoryStore::new()
    }

    // ── test 1 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cold_seed_creates_all_records() {
        let s = store();
        let seed = seed_v1(vec![
            BuiltinSpec::Agent(Box::new(agent_spec("a1", "hello"))),
            BuiltinSpec::Provider(provider_spec("p1")),
            BuiltinSpec::Model(model_spec("m1")),
        ]);

        let report = apply_builtin_seed(&s, &seed).await.unwrap();

        assert_eq!(report.created.len(), 3, "expected 3 created");
        assert!(report.updated.is_empty());
        assert!(report.unchanged.is_empty());
        assert!(report.deleted.is_empty());
        assert!(report.preserved_user.is_empty());

        // Verify stored records have Builtin source.
        for (ns, id) in [("agents", "a1"), ("providers", "p1"), ("models", "m1")] {
            let raw = s.get(ns, id).await.unwrap().expect("record missing");
            let rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
            assert_eq!(
                rec.meta.source,
                RecordSource::Builtin {
                    binary_version: "v1".to_owned()
                }
            );
        }
    }

    // ── test 2 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn idempotent_re_apply_is_noop() {
        let s = store();
        let seed = seed_v1(vec![
            BuiltinSpec::Agent(Box::new(agent_spec("a1", "hello"))),
            BuiltinSpec::Provider(provider_spec("p1")),
            BuiltinSpec::Model(model_spec("m1")),
        ]);

        apply_builtin_seed(&s, &seed).await.unwrap();

        // Read updated_at before second apply.
        let raw_before = s.get("agents", "a1").await.unwrap().unwrap();
        let rec_before: ConfigRecord<serde_json::Value> =
            ConfigRecord::from_value(raw_before).unwrap();
        let updated_at_before = rec_before.meta.updated_at;

        let report = apply_builtin_seed(&s, &seed).await.unwrap();

        assert_eq!(report.unchanged.len(), 3, "expected 3 unchanged");
        assert!(report.created.is_empty());
        assert!(report.updated.is_empty());
        assert!(report.deleted.is_empty());
        assert!(report.preserved_user.is_empty());

        // updated_at must not have changed.
        let raw_after = s.get("agents", "a1").await.unwrap().unwrap();
        let rec_after: ConfigRecord<serde_json::Value> =
            ConfigRecord::from_value(raw_after).unwrap();
        assert_eq!(rec_after.meta.updated_at, updated_at_before);
    }

    // ── test 3 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn same_version_edit_updates_record() {
        let s = store();

        apply_builtin_seed(
            &s,
            &seed_v1(vec![BuiltinSpec::Agent(Box::new(agent_spec(
                "a1",
                "old prompt",
            )))]),
        )
        .await
        .unwrap();

        let report = apply_builtin_seed(
            &s,
            &seed_v1(vec![BuiltinSpec::Agent(Box::new(agent_spec(
                "a1",
                "new prompt",
            )))]),
        )
        .await
        .unwrap();

        assert_eq!(report.updated.len(), 1);
        assert!(report.created.is_empty());
        assert!(report.unchanged.is_empty());

        let raw = s.get("agents", "a1").await.unwrap().unwrap();
        let rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        assert_eq!(rec.spec["system_prompt"], "new prompt");
    }

    // ── test 4 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn version_upgrade_refreshes_record() {
        let s = store();

        apply_builtin_seed(
            &s,
            &seed_v1(vec![BuiltinSpec::Agent(Box::new(agent_spec("a1", "v1")))]),
        )
        .await
        .unwrap();

        let report = apply_builtin_seed(
            &s,
            &seed_v2(vec![BuiltinSpec::Agent(Box::new(agent_spec("a1", "v2")))]),
        )
        .await
        .unwrap();

        assert_eq!(report.updated.len(), 1);

        let raw = s.get("agents", "a1").await.unwrap().unwrap();
        let rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        assert_eq!(
            rec.meta.source,
            RecordSource::Builtin {
                binary_version: "v2".to_owned()
            }
        );
        assert_eq!(rec.spec["system_prompt"], "v2");
    }

    // ── test 5 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn user_record_preserved_through_seed() {
        let s = store();

        // Pre-populate user record.
        let user_record = ConfigRecord {
            spec: serde_json::to_value(agent_spec("coder", "user version")).unwrap(),
            meta: RecordMeta::new_user(),
        };
        s.put("agents", "coder", &user_record.to_value().unwrap())
            .await
            .unwrap();

        let report = apply_builtin_seed(
            &s,
            &seed_v1(vec![BuiltinSpec::Agent(Box::new(agent_spec(
                "coder",
                "builtin version",
            )))]),
        )
        .await
        .unwrap();

        assert_eq!(report.preserved_user.len(), 1);
        assert!(report.created.is_empty());
        assert!(report.updated.is_empty());

        // Original record still intact.
        let raw = s.get("agents", "coder").await.unwrap().unwrap();
        let rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        assert_eq!(rec.meta.source, RecordSource::User);
        assert_eq!(rec.spec["system_prompt"], "user version");
    }

    // ── test 6 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn orphan_builtin_cleaned() {
        let s = store();

        apply_builtin_seed(
            &s,
            &seed_v1(vec![
                BuiltinSpec::Agent(Box::new(agent_spec("a1", "a"))),
                BuiltinSpec::Agent(Box::new(agent_spec("b1", "b"))),
            ]),
        )
        .await
        .unwrap();

        // v2 seed only has a1.
        let report = apply_builtin_seed(
            &s,
            &seed_v2(vec![BuiltinSpec::Agent(Box::new(agent_spec("a1", "a")))]),
        )
        .await
        .unwrap();

        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].id, "b1");

        assert!(s.get("agents", "b1").await.unwrap().is_none());
        assert!(s.get("agents", "a1").await.unwrap().is_some());
    }

    // ── test 7 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn orphan_cleanup_only_targets_builtin() {
        let s = store();

        // Pre-populate user record.
        let user_record = ConfigRecord {
            spec: serde_json::to_value(agent_spec("user-only", "user")).unwrap(),
            meta: RecordMeta::new_user(),
        };
        s.put("agents", "user-only", &user_record.to_value().unwrap())
            .await
            .unwrap();

        // Seed does NOT include user-only.
        let report = apply_builtin_seed(&s, &seed_v1(vec![])).await.unwrap();

        assert!(!report.deleted.iter().any(|r| r.id == "user-only"));
        assert!(s.get("agents", "user-only").await.unwrap().is_some());
    }

    // ── test 8 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn hidden_flag_preserved_across_upgrade() {
        let s = store();

        apply_builtin_seed(
            &s,
            &seed_v1(vec![BuiltinSpec::Agent(Box::new(agent_spec("a1", "v1")))]),
        )
        .await
        .unwrap();

        // Set hidden = true on stored record.
        let raw = s.get("agents", "a1").await.unwrap().unwrap();
        let mut rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        rec.meta.hidden = true;
        s.put("agents", "a1", &rec.to_value().unwrap())
            .await
            .unwrap();

        // Apply v2 with new content.
        apply_builtin_seed(
            &s,
            &seed_v2(vec![BuiltinSpec::Agent(Box::new(agent_spec("a1", "v2")))]),
        )
        .await
        .unwrap();

        let raw = s.get("agents", "a1").await.unwrap().unwrap();
        let rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        assert!(rec.meta.hidden, "hidden flag must be preserved");
        assert_eq!(
            rec.meta.source,
            RecordSource::Builtin {
                binary_version: "v2".to_owned()
            }
        );
        assert_eq!(rec.spec["system_prompt"], "v2");
    }

    // ── test 9 ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn mixed_namespace_seed_routes_correctly() {
        let s = store();

        let seed = seed_v1(vec![
            BuiltinSpec::Agent(Box::new(agent_spec("agent-1", "hi"))),
            BuiltinSpec::Provider(provider_spec("prov-1")),
            BuiltinSpec::Model(model_spec("model-1")),
            BuiltinSpec::McpServer(mcp_spec("mcp-1")),
        ]);

        let report = apply_builtin_seed(&s, &seed).await.unwrap();
        assert_eq!(report.created.len(), 4);

        // Each spec lands in the correct namespace.
        assert!(s.get("agents", "agent-1").await.unwrap().is_some());
        assert!(s.get("providers", "prov-1").await.unwrap().is_some());
        assert!(s.get("models", "model-1").await.unwrap().is_some());
        assert!(s.get("mcp-servers", "mcp-1").await.unwrap().is_some());

        // Wrong namespace: not there.
        assert!(s.get("providers", "agent-1").await.unwrap().is_none());
    }

    // ── test 10 ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn legacy_bare_spec_treated_as_user_during_seed() {
        let s = store();

        // Write bare AgentSpec (no envelope) directly to the store.
        let bare = serde_json::to_value(agent_spec("legacy", "bare")).unwrap();
        s.put("agents", "legacy", &bare).await.unwrap();

        // Seed v1 does NOT contain "legacy".
        let report = apply_builtin_seed(
            &s,
            &seed_v1(vec![BuiltinSpec::Agent(Box::new(agent_spec(
                "other", "other",
            )))]),
        )
        .await
        .unwrap();

        // Orphan cleanup must not touch legacy (decoded as User).
        assert!(!report.deleted.iter().any(|r| r.id == "legacy"));
        assert!(s.get("agents", "legacy").await.unwrap().is_some());
    }

    // ── test 11 ──────────────────────────────────────────────────────────────

    /// Regression test for the pagination skew bug: interleaving deletes with
    /// list() calls caused records past the first page boundary to be skipped.
    /// This test inserts 300 Builtin records (> SEED_LIST_PAGE_SIZE = 256),
    /// then applies an empty seed and asserts all 300 are cleaned up.
    #[tokio::test]
    async fn orphan_cleanup_handles_more_than_one_page() {
        const RECORD_COUNT: usize = 300;
        const _: () = assert!(
            RECORD_COUNT > SEED_LIST_PAGE_SIZE,
            "test must exceed page size to exercise the multi-page path"
        );

        let s = store();

        // Insert 300 Builtin provider records directly (fast, minimal fields).
        for i in 0..RECORD_COUNT {
            let id = format!("prov-{i:04}");
            let record = ConfigRecord {
                spec: serde_json::to_value(provider_spec(&id)).unwrap(),
                meta: RecordMeta::new_builtin("v1"),
            };
            s.put("providers", &id, &record.to_value().unwrap())
                .await
                .unwrap();
        }

        // Apply an empty v2 seed — none of the 300 records should survive.
        let report = apply_builtin_seed(&s, &seed_v2(vec![])).await.unwrap();

        assert_eq!(
            report.deleted.len(),
            RECORD_COUNT,
            "all {RECORD_COUNT} orphans must be deleted, not just the first page"
        );
        assert!(report.created.is_empty());
        assert!(report.updated.is_empty());
        assert!(report.unchanged.is_empty());
        assert!(report.preserved_user.is_empty());

        // Spot-check a record from the second page is gone.
        assert!(
            s.get("providers", "prov-0256").await.unwrap().is_none(),
            "record past first page boundary must also be deleted"
        );
    }

    // ── test 11b ─────────────────────────────────────────────────────────────

    /// user_overrides set on a Builtin record before a version upgrade must be
    /// preserved after the upgrade, just like the `hidden` flag.
    #[tokio::test]
    async fn seed_upgrade_preserves_user_overrides() {
        let s = store();

        apply_builtin_seed(
            &s,
            &seed_v1(vec![BuiltinSpec::Agent(Box::new(agent_spec("a1", "v1")))]),
        )
        .await
        .unwrap();

        // Set user_overrides on the stored record.
        let raw = s.get("agents", "a1").await.unwrap().unwrap();
        let mut rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        rec.meta.user_overrides = Some(serde_json::json!({"system_prompt": "user-custom"}));
        s.put("agents", "a1", &rec.to_value().unwrap())
            .await
            .unwrap();

        // Apply v2 with a new spec.
        apply_builtin_seed(
            &s,
            &seed_v2(vec![BuiltinSpec::Agent(Box::new(agent_spec("a1", "v2")))]),
        )
        .await
        .unwrap();

        let raw = s.get("agents", "a1").await.unwrap().unwrap();
        let rec: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw).unwrap();
        assert_eq!(
            rec.meta.source,
            RecordSource::Builtin {
                binary_version: "v2".to_owned()
            },
            "binary_version must be updated to v2"
        );
        assert_eq!(
            rec.meta.user_overrides,
            Some(serde_json::json!({"system_prompt": "user-custom"})),
            "user_overrides must be preserved across version upgrade"
        );
        // Base spec in store reflects v2 defaults.
        assert_eq!(rec.spec["system_prompt"], "v2");
    }

    // ── test 12 ──────────────────────────────────────────────────────────────

    /// Sanity check: orphan cleanup iterates every namespace via
    /// `ConfigNamespace::iter_str()`. Pre-populate one Builtin orphan in each
    /// of the four namespaces, apply an empty seed, and assert all four are
    /// deleted — proving the loop visited every namespace.
    #[tokio::test]
    async fn orphan_cleanup_uses_config_namespace_iter() {
        let s = store();

        let namespaces_and_ids = [
            ("agents", "orphan-agent"),
            ("providers", "orphan-provider"),
            ("models", "orphan-model"),
            ("mcp-servers", "orphan-mcp"),
        ];

        for (ns, id) in namespaces_and_ids {
            let spec_value = serde_json::json!({ "id": id, "ns": ns });
            let record = ConfigRecord {
                spec: spec_value,
                meta: RecordMeta::new_builtin("v1"),
            };
            s.put(ns, id, &record.to_value().unwrap()).await.unwrap();
        }

        let report = apply_builtin_seed(&s, &seed_v1(vec![])).await.unwrap();

        assert_eq!(
            report.deleted.len(),
            4,
            "expected one deleted orphan per namespace"
        );
        for (ns, id) in namespaces_and_ids {
            assert!(
                report
                    .deleted
                    .iter()
                    .any(|r| r.namespace == ns && r.id == id),
                "deleted must contain {ns}/{id}"
            );
            assert!(
                s.get(ns, id).await.unwrap().is_none(),
                "{ns}/{id} must be removed from the store"
            );
        }
    }
}
