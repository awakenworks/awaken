use awaken_eval::{DatasetSpec, Expectation, Fixture};
use serde::{Deserialize, Serialize};

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
    #[serde(default, rename = "expected", alias = "expect")]
    pub expect: Expectation,
}

#[derive(Debug, Serialize)]
pub struct ImportDialogueResponse {
    pub fixture_id: String,
    pub dataset_revision: u64,
}
