//! Read-only FileCatalog output observations; standard Files remains historical.

use axum::{Router, handler::Handler, routing::get};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileOutputCaptureParams {
    pub scope_id: String,
}

#[derive(Debug, Serialize)]
pub struct FileOutputCaptureResponse {
    pub scope_id: String,
    /// None is unknown; Some with no ids is an observed empty directory.
    pub capture: Option<FileOutputCapture>,
}

#[derive(Debug, Serialize)]
pub struct FileOutputCapture {
    /// Decimal string preserves the catalog revision beyond JavaScript's exact
    /// integer range. It is an observation revision, not a user acceptance flag.
    pub revision: String,
    pub file_ids: Vec<String>,
}

/// The extension owns its path and wire types; the injected method reads the
/// existing File application with the same Workspace boundary as Files GET.
pub fn file_output_capture_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/files/output-capture", get(handler))
}
