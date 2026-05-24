//! Eval dataset as a `ConfigStore` record (ADR-0032 D6).
//!
//! A [`DatasetSpec`] is the persisted form of "a named collection of
//! fixtures the server can replay on demand". Dataset records live in the
//! same `ConfigStore` that holds `AgentSpec` / `ToolSpec`, so they inherit
//! revision tracking, builtin/user provenance, and patch validation for
//! free.
//!
//! Fields are intentionally whole-spec replace (no per-field merge):
//! editing a dataset is "send the new fixture list", not "patch the third
//! fixture's expectation block". That keeps the patch surface honest and
//! avoids non-trivial Vec-merge semantics. [`NoConfigPatch`] enforces it
//! at the contract layer.

use awaken_contract::config_record::{ConfigRecordMerge, NoConfigPatch};
use serde::{Deserialize, Serialize};

use crate::fixture::Fixture;

/// `ConfigStore` namespace under which [`DatasetSpec`] records are stored.
///
/// Single source of truth for both the server's CRUD handlers and the
/// eval-run service that loads datasets to replay. Hand-copying this
/// string would silently break the wiring on a future rename.
pub const DATASETS_NAMESPACE: &str = "eval_datasets";

/// Spec for an eval dataset — a named collection of fixtures.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DatasetSpec {
    /// Operator-facing description shown in the admin UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Fixtures included in this dataset. May be authored by hand, curated
    /// from a production trace via `awaken-eval curate`, or added through
    /// `POST /v1/eval/datasets/:id/items { from_run_id }` (ADR-0032 D5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixtures: Vec<Fixture>,
}

impl DatasetSpec {
    /// Reject duplicate `fixture.id` values. Required at every dataset
    /// write site — `diff_against_baseline` / `diff_eval_items` key by
    /// fixture_id, so duplicates would silently overwrite each other in
    /// the diff map and produce a result whose meaning depends on Vec
    /// insertion order.
    pub fn validate_unique_fixture_ids(&self) -> Result<(), String> {
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.fixtures.len());
        for f in &self.fixtures {
            if !seen.insert(f.id.as_str()) {
                return Err(format!("duplicate fixture id in dataset: {}", f.id));
            }
        }
        Ok(())
    }
}

impl ConfigRecordMerge for DatasetSpec {
    // Datasets are whole-spec replace. [`NoConfigPatch`] rejects any
    // non-empty `user_overrides` payload at validation time so a
    // stray field-level override fails fast instead of silently being
    // dropped on read.
    type Patch = NoConfigPatch;

    fn merge_patch(self, _patch: NoConfigPatch) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expectation::Expectation;
    use crate::fixture::{Fixture, MockResponse};
    use awaken_contract::config_record::{
        ConfigRecord, RecordMeta, validate_config_record, validate_config_record_overrides,
    };
    use serde_json::json;

    fn sample_fixture(id: &str) -> Fixture {
        Fixture {
            id: id.into(),
            description: None,
            user_input: "what is six times seven".into(),
            provider_script: Vec::new(),
            provider_script_error: None,
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: MockResponse::Text { text: "42".into() },
            expect: Expectation::default(),
            continued_turns: vec![],
        }
    }

    #[test]
    fn default_is_empty_description_and_no_fixtures() {
        let spec = DatasetSpec::default();
        assert!(spec.description.is_empty());
        assert!(spec.fixtures.is_empty());
    }

    #[test]
    fn merge_patch_is_identity_under_no_config_patch() {
        let spec = DatasetSpec {
            description: "smoke".into(),
            fixtures: vec![sample_fixture("a")],
        };
        let merged = spec.clone().merge_patch(NoConfigPatch::default());
        assert_eq!(merged, spec);
    }

    #[test]
    fn serde_round_trip_preserves_description_and_fixtures() {
        let spec = DatasetSpec {
            description: "round trip".into(),
            fixtures: vec![sample_fixture("alpha"), sample_fixture("beta")],
        };
        let json = serde_json::to_value(&spec).unwrap();
        let back: DatasetSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn empty_description_is_skipped_on_serialize() {
        // skip_serializing_if keeps the on-disk form compact for the
        // common case where the operator hasn't bothered writing a blurb.
        let spec = DatasetSpec::default();
        let s = serde_json::to_string(&spec).unwrap();
        assert!(!s.contains("description"));
    }

    #[test]
    fn empty_fixtures_is_skipped_on_serialize() {
        let spec = DatasetSpec::default();
        let s = serde_json::to_string(&spec).unwrap();
        assert!(!s.contains("fixtures"));
    }

    #[test]
    fn config_record_envelope_round_trips() {
        // Datasets live in the same ConfigStore that holds AgentSpec.
        // Wrap, encode, decode — the envelope must survive intact.
        let record = ConfigRecord {
            spec: DatasetSpec {
                description: "wrap".into(),
                fixtures: vec![sample_fixture("solo")],
            },
            meta: RecordMeta::new_user(),
        };
        let value = serde_json::to_value(&record).unwrap();
        let decoded = validate_config_record::<DatasetSpec>(value).unwrap();
        assert_eq!(decoded.spec, record.spec);
    }

    #[test]
    fn user_overrides_payload_is_rejected_for_no_patch_spec() {
        // NoConfigPatch denies unknown fields. A stray override on a
        // dataset record must fail validation rather than being silently
        // dropped on the read path.
        let record = ConfigRecord {
            spec: DatasetSpec::default(),
            meta: RecordMeta {
                user_overrides: Some(json!({"description": "rogue"})),
                ..RecordMeta::new_user()
            },
        };
        let err = validate_config_record_overrides::<DatasetSpec>(&record).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid config record overrides"),
            "wrong error variant: {msg}"
        );
    }
}
