//! `/v1/eval/*` route registration. Lifted out of `routes.rs` so the
//! eval surface (datasets, runs, online, importers) lives next to its
//! own handlers and stops dominating the central router file.

use axum::Router;
use axum::routing::{get, post};

use crate::app::EvalRoutesState;
use crate::services::dataset_service::{
    append_fixture, create_dataset, curate_items, delete_dataset, get_dataset, import_dialogue,
    import_traces, list_datasets, put_dataset,
};
use crate::services::eval_run_service::{get_eval_run, list_eval_runs, start_eval_run};
use crate::services::online_eval_service::start_online_eval;

pub fn eval_routes() -> Router<EvalRoutesState> {
    Router::new()
        .route("/v1/eval/datasets", get(list_datasets).post(create_dataset))
        .route(
            "/v1/eval/datasets/:id",
            get(get_dataset).put(put_dataset).delete(delete_dataset),
        )
        .route("/v1/eval/datasets/:id/items", post(curate_items))
        .route("/v1/eval/datasets/:id/fixtures", post(append_fixture))
        .route("/v1/eval/datasets/:id/import-traces", post(import_traces))
        .route(
            "/v1/eval/datasets/:id/import-dialogue",
            post(import_dialogue),
        )
        .route("/v1/eval/runs", get(list_eval_runs).post(start_eval_run))
        .route("/v1/eval/runs/:id", get(get_eval_run))
        .route("/v1/eval/online", post(start_online_eval))
}
