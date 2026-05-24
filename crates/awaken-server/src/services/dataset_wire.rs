use awaken_eval::{DatasetSpec, Expectation, Fixture};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderScriptMode {
    /// Try to capture a deterministic scripted snapshot, but still
    /// create a Live-only fixture when the trace cannot be represented
    /// by today's `ProviderScriptEvent` schema.
    #[default]
    Optional,
    /// Require a replayable `provider_script`; unsupported traces 400
    /// (or are skipped by bulk import when `skip_uncuratable=true`).
    Require,
    /// Do not attempt `provider_script` conversion. The resulting
    /// fixture is explicitly Live-only.
    Skip,
}

#[derive(Debug, Serialize)]
pub struct DatasetSummaryWire {
    pub id: String,
    pub description: String,
    pub fixture_count: usize,
    pub revision: u64,
}

#[derive(Debug, Serialize)]
pub struct ListDatasetsResponse {
    pub datasets: Vec<DatasetSummaryWire>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendFixtureRequest {
    pub fixture: Fixture,
    pub expected_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurateItemsRequest {
    pub from_run_id: String,
    #[serde(default)]
    pub user_input: Option<String>,
    #[serde(default)]
    pub fixture_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub allow_unused_provider_script: bool,
    #[serde(default)]
    pub provider_script_mode: ProviderScriptMode,
    /// Operator-authored pass/fail criteria. Accept both the ADR wire
    /// name (`expected`) and the persisted fixture field (`expect`).
    #[serde(default, rename = "expected", alias = "expect")]
    pub expect: Expectation,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDatasetRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub spec: DatasetSpec,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct IdParam {
    #[serde(default)]
    pub id: Option<String>,
}

/// Query params for `DELETE /v1/eval/datasets/:id`. `expected_revision`
/// turns the delete into a compare-and-swap: the store only removes the
/// record when its current `meta.revision` matches. The trace → fixture
/// flow uses this to roll back an inline-created dataset *without* risking
/// a concurrent operator's fixture that landed between create and curate
/// (plain delete would wipe it). Absent → unconditional delete.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DeleteDatasetParams {
    #[serde(default)]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutDatasetRequest {
    pub expected_revision: u64,
    pub spec: DatasetSpec,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ImportTracesRequest {
    pub expected_revision: u64,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub since_secs: Option<u64>,
    #[serde(default)]
    pub max_count: Option<usize>,
    #[serde(default)]
    pub skip_uncuratable: bool,
    #[serde(default)]
    pub provider_script_mode: ProviderScriptMode,
    #[serde(default, rename = "expected", alias = "expect")]
    pub expect: Expectation,
}

#[derive(Debug, Serialize)]
pub struct ImportTracesResponse {
    pub imported_count: usize,
    pub skipped_count: usize,
    pub dataset_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportDialogueRequest {
    pub expected_revision: u64,
    pub run_ids: Vec<String>,
    #[serde(default)]
    pub fixture_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub provider_script_mode: ProviderScriptMode,
    #[serde(default, rename = "expected", alias = "expect")]
    pub expect: Expectation,
}

#[derive(Debug, Serialize)]
pub struct ImportDialogueResponse {
    pub fixture_id: String,
    pub dataset_revision: u64,
}
