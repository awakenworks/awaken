//! The write-plane half of the cell's worker seam: a database-less worker pushes
//! its committed facts (a neutral [`ThreadCommit`]) to the cell server, which
//! applies them through the thread's single-writer [`Coordinator`]. The worker
//! never touches the store — the server stays the sole writer of committed truth.
//!
//! Paired with the dispatch transport (control plane): a worker claims a run over
//! the dispatch transport, drives it, then commits its facts here.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Json, Router};
use serde_json::{Value, json};

use awaken_agent_contract::agent::run::Phase;
use awaken_agent_contract::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::store::run_store::RunStore;

use crate::host::{HostError, HostErrorKind, SharedHost};

/// The worker-facing commit-ingest router. Mount it on a cell server alongside the
/// dispatch transport; a database-less worker's [`RemoteCoordinator`] posts here.
pub fn commit_ingest_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route("/v1/worker/commit", axum::routing::post(commit_ingest))
        .with_state(host)
}

fn respond(result: Result<Value, HostError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                HostErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                HostErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}

async fn commit_ingest(
    State(host): State<Arc<SharedHost>>,
    Json(commit): Json<ThreadCommit>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        // Resolve the thread's single-writer coordinator and apply the worker's
        // staged commit through it — the server is the sole writer of committed truth.
        let thread = commit.thread_id.0.clone();
        let ctx = host.ctx_for(&thread, None).await?;
        // Idempotent redelivery (at-least-once → exactly-once effect): if this run's
        // fact is already committed to a terminal phase, an earlier delivery landed —
        // return success without re-applying, so a worker's retry is a no-op instead
        // of a rejected double-commit. A parked (`Waiting`) run is not terminal: a
        // later commit is its wake, so it is applied normally.
        if let Some(existing) = RunStore::get(&*ctx.commit, &commit.run_fact.run_id)
            && matches!(existing.phase, Phase::Ended(_))
        {
            return Ok(json!({ "sequence": 0 }));
        }
        let record = ctx
            .commit
            .commit(commit)
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(serde_json::to_value(record).expect("CommitRecord serializes"))
    }
    .await;
    respond(result)
}

/// A database-less worker's [`Coordinator`]: `commit` posts the staged
/// [`ThreadCommit`] to a cell server's [`commit_ingest_router`], which applies it
/// through the thread's single writer. The worker holds only an HTTP client.
pub struct RemoteCoordinator {
    base_url: String,
    client: reqwest::Client,
}

impl RemoteCoordinator {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Coordinator for RemoteCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        let resp = self
            .client
            .post(format!("{}/v1/worker/commit", self.base_url))
            .json(&commit)
            .send()
            .await
            .map_err(|e| CommitError::Rejected(format!("commit transport: {e}")))?;
        if !resp.status().is_success() {
            return Err(CommitError::Rejected(format!(
                "commit ingest server returned {}",
                resp.status()
            )));
        }
        resp.json()
            .await
            .map_err(|e| CommitError::Rejected(format!("commit record decode: {e}")))
    }
}
