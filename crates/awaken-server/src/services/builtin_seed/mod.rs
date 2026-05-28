//! Protocol for applying a [`BuiltinSeedSet`] to a [`ConfigStore`].
//!
//! See [`apply_builtin_seed`] for the full semantics.

use std::collections::{HashMap, HashSet};

use awaken_server_contract::contract::storage::StorageError;
use awaken_server_contract::{
    BuiltinSeedSet, BuiltinSpec, ConfigRecord, ConfigStore, RecordMeta, RecordSource, SkillSpec,
    validate_model_pool_spec_struct,
};

const SEED_LIST_PAGE_SIZE: usize = 256;
const BUILTIN_SEED_NAMESPACES: [&str; 7] = [
    "agents",
    "providers",
    "models",
    "model-pools",
    "mcp-servers",
    "tools",
    "skills",
];

// ── public types ─────────────────────────────────────────────────────────────

/// Report produced by [`apply_builtin_seed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub created: Vec<RecordRef>,
    pub updated: Vec<RecordRef>,
    pub unchanged: Vec<RecordRef>,
    pub deleted: Vec<RecordRef>,
    pub preserved_user: Vec<RecordRef>,
    /// Builtin records orphaned by this seed (id no longer registered) but
    /// carrying a non-empty `user_overrides`. Marked `hidden=true` instead
    /// of being deleted, so re-introducing the spec in a later binary
    /// transparently restores the override.
    pub preserved_overridden: Vec<RecordRef>,
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
    /// Built-in agent spec failed `AgentSpec::validate_catalog`. Mirrors
    /// the write-path guard in `ConfigService::validate_payload`.
    #[error("agent spec '{id}' has invalid tool catalog: {errors}")]
    InvalidAgentCatalog { id: String, errors: String },
    /// Built-in skill spec failed the config write-path validation.
    #[error("skill spec '{id}' is invalid: {errors}")]
    InvalidSkillSpec { id: String, errors: String },
    /// Built-in model pool spec failed the config write-path validation.
    #[error("model pool spec '{id}' is invalid: {errors}")]
    InvalidModelPoolSpec { id: String, errors: String },
}

// ── apply_builtin_seed ────────────────────────────────────────────────────────

/// Apply a seed to the given ConfigStore.
///
/// Behavior per spec in `seed.specs`:
/// - No existing record → create new Builtin record. (created)
/// - Existing Builtin, same binary_version, spec equal → no-op. (unchanged)
/// - Existing Builtin, same binary_version, spec differs → replace spec, refresh updated_at. (updated)
/// - Existing Builtin, different binary_version → replace spec + version, clear hidden, refresh updated_at. (updated)
/// - Existing User → leave entirely untouched. (preserved_user)
///
/// After processing seed entries, scans all built-in spec namespaces
/// (`agents`, `providers`, `models`, `mcp-servers`, `tools`, `skills`) and processes
/// each Builtin record whose ID is not in this seed:
///
/// - If it carries a `user_overrides` payload → marks it `hidden=true` instead
///   of deleting, so re-introducing the spec in a later binary transparently
///   restores the override (preserved_overridden).
/// - Otherwise → hard-deletes it (deleted).
///
/// User records are never deleted by orphan cleanup.
///
/// Seed writes use ConfigStore CAS primitives so a concurrent writer surfaces as
/// a storage conflict instead of silently overwriting records.
pub async fn apply_builtin_seed(
    store: &dyn ConfigStore,
    seed: &BuiltinSeedSet,
) -> Result<SeedReport, SeedError> {
    // Reject the whole seed up-front on any unparseable catalog pattern, so
    // bad syntax can't enter the store via the seed path and only surface
    // later as a resolve-time "no tools matched" warning.
    for spec in &seed.specs {
        match spec {
            BuiltinSpec::Agent(agent) => validate_agent_spec_catalog(agent)?,
            BuiltinSpec::ModelPool(pool) => {
                validate_model_pool_spec_struct(pool).map_err(|error| {
                    SeedError::InvalidModelPoolSpec {
                        id: pool.id.clone(),
                        errors: error.to_string(),
                    }
                })?;
            }
            BuiltinSpec::Skill(skill) => validate_builtin_skill_spec(skill)?,
            _ => {}
        }
    }

    let mut report = SeedReport::default();

    // Track seeded (namespace, id) pairs for orphan cleanup.
    let mut seeded: HashMap<&str, HashSet<String>> = HashMap::new();
    for ns in BUILTIN_SEED_NAMESPACES {
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
                let mut record = ConfigRecord {
                    spec: new_spec_value,
                    meta: RecordMeta::new_builtin(&seed.binary_version),
                };
                record.meta.revision = 1;
                store
                    .put_if_absent(namespace, id, &record.to_value()?)
                    .await?;
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
                            // user_overrides and created_at.
                            // Reintroducing a previously-orphaned spec clears
                            // `hidden`; the user override (if any) flows
                            // through unchanged.
                            let now = awaken_server_contract::time::now_ms();
                            let expected_revision = existing.meta.revision;
                            let record = ConfigRecord {
                                spec: new_spec_value,
                                meta: RecordMeta {
                                    source: RecordSource::Builtin {
                                        binary_version: seed.binary_version.clone(),
                                    },
                                    hidden: false,
                                    user_overrides: existing.meta.user_overrides,
                                    created_at: existing.meta.created_at,
                                    updated_at: now,
                                    revision: expected_revision + 1,
                                },
                            };
                            store
                                .put_if_revision(
                                    namespace,
                                    id,
                                    &record.to_value()?,
                                    expected_revision,
                                )
                                .await?;
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
    for namespace in BUILTIN_SEED_NAMESPACES {
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

        // Pass 2: delete or hide each candidate based on whether it carries
        // a user override. Hard-delete records with no override; soft-delete
        // (hidden=true) records that DO have an override so that re-introducing
        // the spec in a later binary transparently restores the override.
        for id in candidates {
            let Some(raw) = store.get(namespace, &id).await? else {
                continue;
            };
            let mut record: ConfigRecord<serde_json::Value> = ConfigRecord::from_value(raw)?;
            let expected_revision = record.meta.revision;

            if record.meta.user_overrides.is_some() {
                // Soft-delete: preserve the override under hidden=true.
                record.meta.hidden = true;
                record.meta.updated_at = awaken_server_contract::time::now_ms();
                record.meta.revision = expected_revision + 1;
                store
                    .put_if_revision(namespace, &id, &record.to_value()?, expected_revision)
                    .await?;
                report
                    .preserved_overridden
                    .push(RecordRef::new(namespace, &id));
            } else {
                store
                    .delete_if_revision(namespace, &id, expected_revision)
                    .await?;
                report.deleted.push(RecordRef::new(namespace, &id));
            }
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
        BuiltinSpec::ModelPool(s) => serde_json::to_value(s),
        BuiltinSpec::McpServer(s) => serde_json::to_value(s),
        BuiltinSpec::Tool(s) => serde_json::to_value(s),
        BuiltinSpec::Skill(s) => serde_json::to_value(s),
    }
}

/// Enforce `AgentSpec::validate_catalog` on a builtin agent, surfacing
/// errors as `InvalidAgentCatalog`. Mirrors the write-path policy.
fn validate_agent_spec_catalog(spec: &awaken_server_contract::AgentSpec) -> Result<(), SeedError> {
    let errors = crate::services::agent_catalog::collect_catalog_errors(spec);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(SeedError::InvalidAgentCatalog {
            id: spec.id.clone(),
            errors: errors.join("; "),
        })
    }
}

/// Enforce `SkillSpec` write-path validation on builtin skills.
fn validate_builtin_skill_spec(spec: &SkillSpec) -> Result<(), SeedError> {
    let value = serde_json::to_value(spec)?;
    awaken_server_contract::validate_skill_spec(value).map_err(|error| {
        SeedError::InvalidSkillSpec {
            id: spec.id.clone(),
            errors: error.to_string(),
        }
    })?;
    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────
//
// Inline tests live in `tests.rs` to keep this file under the lefthook
// code-file-length guard.

#[cfg(test)]
mod tests;
