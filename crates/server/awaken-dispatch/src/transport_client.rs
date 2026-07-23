//! The database-less worker's dispatch client: a `Dispatch` implementation whose
//! claim/settle verbs are HTTP calls to the Control Node's registered Worker router,
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

/// Client-side counterpart of the Control Node's worker authenticator.
///
/// One implementation decorates every lifecycle, dispatch, recovery, and commit
/// request. The path is the absolute HTTP path (without origin or query) so
/// production implementations can bind a signed assertion to the exact route.
pub trait WorkerRequestAuthorizer: Send + Sync {
    fn authorize(
        &self,
        method: &str,
        path: &str,
        worker_id: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String>;

    /// Return an authorizer bound to the durable identity allocated by
    /// registration. Bootstrap credentials may be worker-id-only; all later
    /// requests can then bind the incarnation and generation as well.
    fn bind_worker_identity(
        &self,
        identity: &WorkerIdentity,
    ) -> std::sync::Arc<dyn WorkerRequestAuthorizer>;
}

/// A `Dispatch` store whose worker verbs are HTTP calls to a cell server.
pub struct HttpDispatchQueue {
    base_url: String,
    client: reqwest::Client,
    worker_identity: WorkerIdentity,
    request_authorizer: Option<std::sync::Arc<dyn WorkerRequestAuthorizer>>,
}

impl HttpDispatchQueue {
    /// Point one registered Worker incarnation at `base_url`.
    pub fn new(base_url: impl Into<String>, worker_identity: WorkerIdentity) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            worker_identity,
            request_authorizer: None,
        }
    }

    /// Decorate every request with the same logical Worker credential used by
    /// lifecycle and claimed-commit clients.
    #[must_use]
    pub fn with_request_authorizer(
        mut self,
        authorizer: std::sync::Arc<dyn WorkerRequestAuthorizer>,
    ) -> Self {
        self.request_authorizer = Some(authorizer.bind_worker_identity(&self.worker_identity));
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
        let request = self.client.post(format!("{}{}", self.base_url, path));
        let request = if let Some(authorizer) = &self.request_authorizer {
            authorizer
                .authorize("POST", path, worker_id, request)
                .map_err(DispatchError::Rejected)?
        } else {
            request.header("x-awaken-worker-id", worker_id)
        };
        let resp = request
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

    fn worker_id(&self) -> &str {
        &self.worker_identity.worker_id
    }
}

#[async_trait]
impl DispatchQueue for HttpDispatchQueue {
    async fn claim_is_current(
        &self,
        claim: &RunClaim,
        _now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_is_current",
                json!({ "claim": claim, "identity": &self.worker_identity }),
                self.worker_id(),
            )
            .await?;
        Ok(value
            .get("current")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    async fn worker_owns_run(
        &self,
        _identity: &crate::WorkerIdentity,
        _run_id: &RunId,
        _now_ms: u64,
    ) -> Result<bool, DispatchError> {
        Self::server_local("worker_owns_run")
    }

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
                json!({ "claim": claim, "identity": &self.worker_identity }),
                self.worker_id(),
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
                json!({ "claim": claim, "checkpoint": checkpoint, "identity": &self.worker_identity }),
                self.worker_id(),
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
                json!({ "claim": claim, "identity": &self.worker_identity }),
                self.worker_id(),
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

    async fn load_recovery_snapshot(
        &self,
        claim: &RunClaim,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, DispatchError>
    {
        let value = self
            .post(
                "/v1/worker/recovery/snapshot",
                json!({ "claim": claim, "identity": &self.worker_identity }),
                self.worker_id(),
            )
            .await?;
        serde_json::from_value(value.get("snapshot").cloned().unwrap_or_default())
            .map_err(|error| DispatchError::Rejected(format!("decode recovery snapshot: {error}")))
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
                    "identity": &self.worker_identity
                }),
                self.worker_id(),
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
            self.worker_id(),
        )
        .await?;
        Ok(())
    }

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_new_run",
                json!({ "request": request, "identity": &self.worker_identity }),
                self.worker_id(),
            )
            .await?;
        let claimed = serde_json::from_value(
            value
                .get("claimed")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode claimed: {error}")))?;
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
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/deliver_and_claim",
                json!({ "input": input, "identity": &self.worker_identity }),
                self.worker_id(),
            )
            .await?;
        let claimed = serde_json::from_value(
            value
                .get("claimed")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode claimed: {error}")))?;
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
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/claim",
                json!({ "identity": &self.worker_identity }),
                self.worker_id(),
            )
            .await?;
        let claimed =
            serde_json::from_value(v.get("claimed").cloned().unwrap_or(serde_json::Value::Null))
                .map_err(|e| DispatchError::Rejected(format!("decode claimed: {e}")))?;
        let _ = (lease_ms, now_ms);
        Ok(claimed)
    }

    async fn claim_compatible(
        &self,
        _worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        self.claim(&self.worker_identity.lease_owner(), lease_ms, now_ms)
            .await
    }

    async fn claim_run(
        &self,
        run_id: &RunId,
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_run",
                json!({ "run_id": run_id.0, "identity": &self.worker_identity }),
                self.worker_id(),
            )
            .await?;
        let claimed = serde_json::from_value(
            value
                .get("claimed")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode claimed: {error}")))?;
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
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<bool, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/renew",
                json!({ "run_id": run_id.0, "identity": &self.worker_identity }),
                self.worker_id(),
            )
            .await?;
        let _ = (lease_ms, now_ms);
        Ok(v.get("renewed").and_then(|r| r.as_bool()).unwrap_or(false))
    }

    async fn renew_owned_leases(
        &self,
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<usize, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/renew_owned",
                json!({ "identity": &self.worker_identity }),
                self.worker_id(),
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
        let v = self
            .post(
                "/v1/worker/dispatch/settle",
                json!({ "run_id": run_id.0, "epoch": epoch, "outcome": outcome, "consumed": consumed, "identity": &self.worker_identity }),
                self.worker_id(),
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
