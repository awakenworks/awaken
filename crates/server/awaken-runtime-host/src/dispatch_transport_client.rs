//! The database-less worker's dispatch client: a `Dispatch` implementation whose
//! claim/settle verbs are HTTP calls to a cell server's `dispatch_transport_router`,
//! so a worker drives runs without ever opening the store.
//!
//! Only the worker verbs cross the wire — `enqueue`, `claim`, `renew_lease`,
//! `renew_owned_leases`, `settle`. The operational verbs (reap, dead-letter, purge,
//! supersede, cancel, list) and the `Inbox`/`Outbox` aggregates are server-local:
//! the worker never runs them, so they fail closed rather than pretend. Wrap this in
//! `AnyDispatchStore::from_dispatch` to hand it to the pool.

use async_trait::async_trait;
use serde_json::json;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    CasOutcome, Claimed, DispatchError, DispatchOutcome, DispatchQueue, DispatchSummary, Inbox,
    Outbox, PendingInput, PendingRecord, RunExecutionRequest, SubmitOptions,
};
use awaken_runtime_contract::resume::ResumeResult;

/// A `Dispatch` store whose worker verbs are HTTP calls to a cell server.
pub struct HttpDispatchQueue {
    base_url: String,
    client: reqwest::Client,
}

impl HttpDispatchQueue {
    /// Point a worker at `base_url` (the cell server's origin, e.g.
    /// `http://server:8080`); the transport routes are appended.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, DispatchError> {
        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .json(&body)
            .send()
            .await
            .map_err(|e| DispatchError::Rejected(format!("dispatch transport: {e}")))?;
        if !resp.status().is_success() {
            return Err(DispatchError::Rejected(format!(
                "dispatch transport server returned {}",
                resp.status()
            )));
        }
        resp.json()
            .await
            .map_err(|e| DispatchError::Rejected(format!("dispatch transport decode: {e}")))
    }

    fn server_local<T>(verb: &str) -> Result<T, DispatchError> {
        Err(DispatchError::Rejected(format!(
            "{verb} is a server-local operation, not available on the worker dispatch transport"
        )))
    }
}

#[async_trait]
impl DispatchQueue for HttpDispatchQueue {
    async fn enqueue_with(
        &self,
        request: RunExecutionRequest,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        self.post(
            "/v1/worker/dispatch/enqueue",
            json!({ "request": request, "options": options }),
        )
        .await?;
        Ok(())
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/claim",
                json!({ "owner": owner, "lease_ms": lease_ms, "now_ms": now_ms }),
            )
            .await?;
        serde_json::from_value(v.get("claimed").cloned().unwrap_or(serde_json::Value::Null))
            .map_err(|e| DispatchError::Rejected(format!("decode claimed: {e}")))
    }

    async fn renew_lease(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/renew",
                json!({ "run_id": run_id.0, "owner": owner, "lease_ms": lease_ms, "now_ms": now_ms }),
            )
            .await?;
        Ok(v.get("renewed").and_then(|r| r.as_bool()).unwrap_or(false))
    }

    async fn renew_owned_leases(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/renew_owned",
                json!({ "owner": owner, "lease_ms": lease_ms, "now_ms": now_ms }),
            )
            .await?;
        Ok(v.get("renewed").and_then(|r| r.as_u64()).unwrap_or(0) as usize)
    }

    async fn settle(
        &self,
        run_id: &RunId,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<(), DispatchError> {
        self.post(
            "/v1/worker/dispatch/settle",
            json!({ "run_id": run_id.0, "outcome": outcome, "consumed": consumed }),
        )
        .await?;
        Ok(())
    }

    // --- server-local operational verbs: a worker never runs these ---

    async fn reap(&self, _max_attempts: u64, _now_ms: u64) -> Result<usize, DispatchError> {
        Self::server_local("reap")
    }
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        Self::server_local("dead_letters")
    }
    async fn requeue(&self, _run_id: &RunId) -> Result<bool, DispatchError> {
        Self::server_local("requeue")
    }
    async fn cancel(&self, _run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        Self::server_local("cancel")
    }
    async fn parked_run(&self, _thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        Self::server_local("parked_run")
    }
    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        Self::server_local("purge_dead_letters")
    }
    async fn purge_dead_letters_before(&self, _cutoff_ms: u64) -> Result<usize, DispatchError> {
        Self::server_local("purge_dead_letters_before")
    }
    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        Self::server_local("superseded")
    }
    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        Self::server_local("list_dispatches")
    }
}

#[async_trait]
impl Inbox for HttpDispatchQueue {
    async fn append(&self, _input: PendingInput) -> Result<bool, DispatchError> {
        Self::server_local("inbox.append")
    }
    async fn list(&self, _thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        Self::server_local("inbox.list")
    }
    async fn retract(
        &self,
        _message_id: &str,
        _expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        Self::server_local("inbox.retract")
    }
    async fn edit(
        &self,
        _message_id: &str,
        _expected_revision: u64,
        _result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        Self::server_local("inbox.edit")
    }
}

#[async_trait]
impl Outbox for HttpDispatchQueue {
    async fn stage(&self, _input: PendingInput) -> Result<bool, DispatchError> {
        Self::server_local("outbox.stage")
    }
    async fn relay(&self) -> Result<usize, DispatchError> {
        Self::server_local("outbox.relay")
    }
}
