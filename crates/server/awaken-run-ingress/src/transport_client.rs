//! The database-less worker's dispatch client: a `Dispatch` implementation whose
//! claim/settle verbs are HTTP calls to a cell server's `dispatch_transport_router`,
//! so a worker drives runs without ever opening the store.
//!
//! Only the worker verbs cross the wire — `enqueue`, `claim_new_run`, `claim`,
//! `renew_lease`, `renew_owned_leases`, and `settle`. Claimed commits use the separate atomic
//! server operation; this transport exposes no check-then-commit fence read. The
//! operational verbs (reap, dead-letter, purge, supersede, cancel, requeue,
//! awaiting-run, list-dispatches) and the `Inbox`/`Outbox` write + relay aggregates
//! are server-local: the worker never runs them, so they fail closed (`Rejected`)
//! rather than pretend a mutation the server didn't perform. The sole exception is
//! `Inbox::list`, which the db-less worker's own drive calls (`worker.rs`) to drain
//! a thread's unbound input — the pending is already in `Claimed.pending`, so an
//! empty list is the correct answer, not a pretended one. Wrap this in
//! `AnyDispatchStore::from_dispatch` to hand it to the pool.

use async_trait::async_trait;
use serde_json::json;

use crate::{
    CasOutcome, Claimed, CommitEpochGuard, DispatchError, DispatchOutcome, DispatchQueue,
    DispatchSummary, Inbox, Outbox, PendingInput, PendingRecord, RunClaim, RunDispatch,
    SettleOutcome, SubmitOptions,
};
use crate::{WorkerIdentity, WorkerSnapshot};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_runtime_contract::resume::ResumeResult;

/// Build a worker's process dispatch store: an [`HttpDispatchQueue`] pointed at the
/// cell server, wrapped as the injectable `AnyDispatchStore` the pool drains. Pass
/// it to `init_shared_dispatch_store` so `ensure_dispatch_pool` claims/settles over
/// the transport instead of a local queue.
pub fn worker_dispatch_store(
    server_url: impl Into<String>,
) -> std::sync::Arc<crate::AnyDispatchStore> {
    std::sync::Arc::new(crate::AnyDispatchStore::from_dispatch(std::sync::Arc::new(
        HttpDispatchQueue::new(server_url),
    )
        as std::sync::Arc<dyn crate::Dispatch>))
}

/// A `Dispatch` store whose worker verbs are HTTP calls to a cell server.
pub struct HttpDispatchQueue {
    base_url: String,
    client: reqwest::Client,
    default_worker_id: String,
    worker_identity: Option<WorkerIdentity>,
    legacy_claimed_owners: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl HttpDispatchQueue {
    /// Point a worker at `base_url` (the cell server's origin, e.g.
    /// `http://server:8080`); the transport routes are appended.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            default_worker_id: std::env::var("AWAKEN_WORKER_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "awaken-worker".to_string()),
            worker_identity: None,
            legacy_claimed_owners: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Configure the authenticated identity used for owner-less worker verbs such
    /// as enqueue and settle. Claim methods still use their `owner` argument so the
    /// neutral `DispatchQueue` caller and HTTP identity remain aligned.
    #[must_use]
    pub fn with_worker_id(mut self, worker_id: impl Into<String>) -> Self {
        self.default_worker_id = worker_id.into();
        self
    }

    /// Pin the durable registry identity allocated at startup. Registered-worker
    /// transports use it for every authority-bearing verb.
    #[must_use]
    pub fn with_worker_identity(mut self, identity: WorkerIdentity) -> Self {
        self.default_worker_id = identity.worker_id.clone();
        self.worker_identity = Some(identity);
        self
    }

    /// Use a caller-configured client (for example one carrying a worker mTLS
    /// identity) instead of the default client.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
        worker_id: &str,
    ) -> Result<serde_json::Value, DispatchError> {
        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .header("x-awaken-worker-id", worker_id)
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

    fn authenticated_worker<'a>(&'a self, legacy_owner: &'a str) -> &'a str {
        if self.worker_identity.is_some() {
            &self.default_worker_id
        } else {
            legacy_owner
        }
    }

    fn remember_legacy_claim(&self, claimed: &Option<Claimed>) {
        if self.worker_identity.is_none()
            && let Some(claimed) = claimed
            && let Ok(mut owners) = self.legacy_claimed_owners.lock()
        {
            owners.insert(claimed.lease.run_id.0.clone(), claimed.lease.owner.clone());
        }
    }

    fn settlement_worker(&self, run_id: &RunId) -> String {
        if self.worker_identity.is_some() {
            return self.default_worker_id.clone();
        }
        self.legacy_claimed_owners
            .lock()
            .ok()
            .and_then(|owners| owners.get(&run_id.0).cloned())
            .unwrap_or_else(|| self.default_worker_id.clone())
    }
}

#[async_trait]
impl DispatchQueue for HttpDispatchQueue {
    async fn lock_commit_epoch(
        &self,
        _claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        Err(DispatchError::Rejected(
            "remote claims require the atomic claimed-commit endpoint".to_string(),
        ))
    }

    async fn load_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<StreamCheckpoint>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/checkpoint/get",
                json!({ "claim": claim, "identity": self.worker_identity }),
                &self.default_worker_id,
            )
            .await?;
        serde_json::from_value(value.get("checkpoint").cloned().unwrap_or_default())
            .map_err(|error| DispatchError::Rejected(format!("decode checkpoint: {error}")))
    }

    async fn put_stream_checkpoint(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
    ) -> Result<SettleOutcome, DispatchError> {
        let value = self
            .post(
                "/v1/worker/checkpoint/put",
                json!({ "claim": claim, "checkpoint": checkpoint, "identity": self.worker_identity }),
                &self.default_worker_id,
            )
            .await?;
        Ok(
            if value.get("applied").and_then(|v| v.as_bool()) == Some(true) {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    async fn delete_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        let value = self
            .post(
                "/v1/worker/checkpoint/delete",
                json!({ "claim": claim, "identity": self.worker_identity }),
                &self.default_worker_id,
            )
            .await?;
        Ok(
            if value.get("applied").and_then(|v| v.as_bool()) == Some(true) {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/bind_sandbox",
                json!({
                    "claim": claim,
                    "sandbox_ref": sandbox_ref,
                    "identity": self.worker_identity
                }),
                &self.default_worker_id,
            )
            .await?;
        Ok(
            if value.get("applied").and_then(|value| value.as_bool()) == Some(true) {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        self.post(
            "/v1/worker/dispatch/enqueue",
            json!({ "request": request, "options": options }),
            &self.default_worker_id,
        )
        .await?;
        Ok(())
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_new_run",
                json!({ "request": request, "identity": self.worker_identity }),
                self.authenticated_worker(owner),
            )
            .await?;
        let claimed = serde_json::from_value(
            value
                .get("claimed")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode claimed: {error}")))?;
        self.remember_legacy_claim(&claimed);
        let _ = (lease_ms, now_ms);
        Ok(claimed)
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.claim_new_run(request, &worker.identity.lease_owner(), lease_ms, now_ms)
            .await
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/deliver_and_claim",
                json!({ "input": input, "identity": self.worker_identity }),
                self.authenticated_worker(owner),
            )
            .await?;
        let claimed = serde_json::from_value(
            value
                .get("claimed")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode claimed: {error}")))?;
        self.remember_legacy_claim(&claimed);
        let _ = (lease_ms, now_ms);
        Ok(claimed)
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.deliver_and_claim(input, &worker.identity.lease_owner(), lease_ms, now_ms)
            .await
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
                json!({ "identity": self.worker_identity }),
                self.authenticated_worker(owner),
            )
            .await?;
        let claimed =
            serde_json::from_value(v.get("claimed").cloned().unwrap_or(serde_json::Value::Null))
                .map_err(|e| DispatchError::Rejected(format!("decode claimed: {e}")))?;
        self.remember_legacy_claim(&claimed);
        let _ = (lease_ms, now_ms);
        Ok(claimed)
    }

    async fn claim_compatible(
        &self,
        _worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.claim(&self.default_worker_id, lease_ms, now_ms).await
    }

    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_run",
                json!({ "run_id": run_id.0, "identity": self.worker_identity }),
                self.authenticated_worker(owner),
            )
            .await?;
        let claimed = serde_json::from_value(
            value
                .get("claimed")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode claimed: {error}")))?;
        self.remember_legacy_claim(&claimed);
        let _ = (lease_ms, now_ms);
        Ok(claimed)
    }

    async fn claim_run_compatible(
        &self,
        run_id: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.claim_run(run_id, &worker.identity.lease_owner(), lease_ms, now_ms)
            .await
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
                json!({ "run_id": run_id.0, "identity": self.worker_identity }),
                self.authenticated_worker(owner),
            )
            .await?;
        let _ = (lease_ms, now_ms);
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
                json!({ "identity": self.worker_identity }),
                self.authenticated_worker(owner),
            )
            .await?;
        let _ = (lease_ms, now_ms);
        Ok(v.get("renewed").and_then(|r| r.as_u64()).unwrap_or(0) as usize)
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let worker_id = self.settlement_worker(run_id);
        let v = self
            .post(
                "/v1/worker/dispatch/settle",
                json!({ "run_id": run_id.0, "epoch": epoch, "outcome": outcome, "consumed": consumed, "identity": self.worker_identity }),
                &worker_id,
            )
            .await?;
        // `settled` is the server's fence verdict: applied vs. stale-epoch fenced.
        Ok(
            if v.get("settled").and_then(|s| s.as_bool()).unwrap_or(true) {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    // --- server-local operational verbs: the SERVER owns dead-letter/recovery GC.
    // A database-less worker never legitimately drives them, so they FAIL CLOSED
    // (Rejected) rather than pretend a mutation/read the server didn't perform — a
    // silent `Ok` no-op here would let a remote worker believe it reaped/purged/
    // relayed when it did nothing. The pool's maintenance loop ticks reap/purge/
    // relay but discards their result (`let _ =` / `unwrap_or(0)`), so a rejection
    // is swallowed there; anywhere the result is consumed, the fault surfaces. ---

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
    async fn awaiting_run(&self, _thread_id: &ThreadId) -> Result<Option<RunId>, DispatchError> {
        Self::server_local("awaiting_run")
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
    // The one legitimate read no-op (NOT fail-closed): the worker's own drive calls
    // `Inbox::list` to drain a thread's unbound input, but a db-less worker already
    // receives its run's pending input in `Claimed.pending`, and the unbound-inbox
    // drain is a server-local concern — so an empty list is the *correct* answer
    // here, not a pretended one. Failing this closed would break every db-less
    // drive (`worker.rs` consumes it with `?`).
    async fn list(&self, _thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        Ok(Vec::new())
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
