//! The database-less worker's dispatch client: a `Dispatch` implementation whose
//! claim/settle verbs are HTTP calls to the Coordinator's registered Worker router,
//! so a worker drives runs without ever opening the store.
//!
//! Only the worker verbs cross the wire — `enqueue`, `claim_new_run`, `claim`,
//! `renew_lease`, `renew_owned_leases`, and `settle`. Claimed commits use the separate atomic
//! server operation; this transport exposes no check-then-commit fence read. The
//! server-local operational verbs (manual quarantine, purge, supersede, cancel, requeue,
//! awaiting-run, list-dispatches) and the `Inbox`/`Outbox` write + relay aggregates
//! are server-local: the worker never runs them, so they fail closed (`Rejected`)
//! rather than pretend a mutation the server didn't perform. The sole exception is
//! `Inbox::list`, which the db-less worker's own drive calls (`worker.rs`) to drain
//! a thread's unbound input — the pending is already in `Claimed.pending`, so an
//! empty list is the correct answer, not a pretended one. Wrap this in
//! `AnyDispatchStore::from_dispatch` to hand it to the pool.

use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::Error as StreamError;
use awaken_run_ingress_contract::{
    BindSandboxRequest, CasOutcome, CheckpointRequest, ClaimNewRunRequest, ClaimRunRequest,
    ClaimWorkerRequest, Claimed, CommitEpochGuard, CredentialRealizationReceipt,
    CredentialRealizationRequest, DeliverAndClaimRequest, DispatchError, DispatchOutcome,
    DispatchQueue, DispatchSummary, EnqueueRequest, Inbox, Outbox, PendingInput, PendingRecord,
    RecoveryRequest, RelinquishRequest, RenewRequest, RunClaim, RunDispatch, SettleOutcome,
    SettleRequest, StreamEventRequest, SubmitOptions,
};
use awaken_run_ingress_contract::{
    ClaimedStreamPublisher, WorkerIdentity, WorkerSnapshot,
    worker_credential_realization_capabilities,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_worker_transport_security::{WorkerRequestAuthorizer, WorkerUpstream};

const IDEMPOTENT_TRANSPORT_ATTEMPTS: usize = 3;
const IDEMPOTENT_TRANSPORT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

fn installed_worker_credential_capabilities(
    worker: &WorkerSnapshot,
) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, DispatchError> {
    worker_credential_realization_capabilities(worker)
        .map_err(|error| DispatchError::Rejected(error.to_string()))
}

/// A `Dispatch` store whose worker verbs are HTTP calls to a cell server.
pub struct HttpDispatchQueue {
    base_url: String,
    client: reqwest::Client,
    worker_identity: WorkerIdentity,
    request_authorizer: Option<std::sync::Arc<dyn WorkerRequestAuthorizer>>,
}

/// Build the Worker's single authenticated dispatch/live transport instance.
pub fn dispatch_transport_with_upstream(
    upstream: &WorkerUpstream,
    identity: WorkerIdentity,
) -> Arc<HttpDispatchQueue> {
    Arc::new(
        HttpDispatchQueue::new(upstream.base_url(), identity)
            .with_client(upstream.client().clone())
            .with_request_authorizer(upstream.request_authorizer()),
    )
}

/// Expose the one transport through the two neutral ports consumed by a Worker
/// composition, without making the Worker depend on a Coordinator store adapter.
pub fn worker_transports_with_upstream(
    upstream: &WorkerUpstream,
    identity: WorkerIdentity,
) -> (
    Arc<dyn awaken_run_ingress_contract::Dispatch>,
    Arc<dyn ClaimedStreamPublisher>,
) {
    let transport = dispatch_transport_with_upstream(upstream, identity);
    (transport.clone(), transport)
}

impl HttpDispatchQueue {
    /// Point one registered Worker incarnation at the private Control plane.
    /// Ambient egress proxies are disabled by default; callers can opt into an
    /// intentional proxy or mTLS configuration with [`Self::with_client`].
    pub fn new(base_url: impl Into<String>, worker_identity: WorkerIdentity) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("the default dispatch HTTP client should build"),
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

    async fn post<T: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &T,
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
            let status = resp.status();
            let detail = resp
                .text()
                .await
                .unwrap_or_default()
                .trim()
                .chars()
                .take(512)
                .collect::<String>();
            return Err(DispatchError::Rejected(format!(
                "dispatch transport server returned {status}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )));
        }
        resp.json()
            .await
            .map_err(|e| DispatchError::Rejected(format!("dispatch transport decode: {e}")))
    }

    /// Retry a claim-fenced operation whose repeated application is explicitly
    /// idempotent. This is intentionally separate from `post`: claim/admission
    /// responses cannot be replayed safely after an ambiguous receipt, while an
    /// exact-claim sandbox bind writes the same opaque reference each time.
    async fn post_idempotent<T: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &T,
        worker_id: &str,
    ) -> Result<serde_json::Value, DispatchError> {
        let mut last_retryable_error = None;
        for attempt in 1..=IDEMPOTENT_TRANSPORT_ATTEMPTS {
            let request = self.client.post(format!("{}{}", self.base_url, path));
            let request = if let Some(authorizer) = &self.request_authorizer {
                authorizer
                    .authorize("POST", path, worker_id, request)
                    .map_err(DispatchError::Rejected)?
            } else {
                request.header("x-awaken-worker-id", worker_id)
            };
            let response = match request.json(&body).send().await {
                Ok(response) => response,
                Err(error) => {
                    last_retryable_error = Some(format!("idempotent dispatch transport: {error}"));
                    if attempt < IDEMPOTENT_TRANSPORT_ATTEMPTS {
                        tokio::time::sleep(IDEMPOTENT_TRANSPORT_RETRY_DELAY).await;
                        continue;
                    }
                    break;
                }
            };
            if response.status().is_server_error() {
                last_retryable_error = Some(format!(
                    "idempotent dispatch transport server returned {}",
                    response.status()
                ));
                if attempt < IDEMPOTENT_TRANSPORT_ATTEMPTS {
                    tokio::time::sleep(IDEMPOTENT_TRANSPORT_RETRY_DELAY).await;
                    continue;
                }
                break;
            }
            if !response.status().is_success() {
                let status = response.status();
                let detail = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .trim()
                    .chars()
                    .take(512)
                    .collect::<String>();
                return Err(DispatchError::Rejected(format!(
                    "dispatch transport server returned {status}{}",
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                )));
            }
            match response.json().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    last_retryable_error =
                        Some(format!("idempotent dispatch transport decode: {error}"));
                    if attempt < IDEMPOTENT_TRANSPORT_ATTEMPTS {
                        tokio::time::sleep(IDEMPOTENT_TRANSPORT_RETRY_DELAY).await;
                    }
                }
            }
        }
        Err(DispatchError::Rejected(
            last_retryable_error.unwrap_or_else(|| {
                "idempotent dispatch transport exhausted without a response".to_string()
            }),
        ))
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
impl ClaimedStreamPublisher for HttpDispatchQueue {
    async fn publish(&self, claim: &RunClaim, event: StreamEvent) -> Result<(), StreamError> {
        // `classify` is the single routing truth. Complete content/lifecycle Facts
        // are committed through the durable path and must never be posted to the
        // Coordinator's live-only observation endpoint.
        if !awaken_agent_contract::event::classify(&event.kind).live {
            return Ok(());
        }
        let result = self
            .post(
                "/v1/worker/dispatch/stream",
                &StreamEventRequest {
                    claim: claim.clone(),
                    identity: self.worker_identity.clone(),
                    event,
                },
                self.worker_id(),
            )
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, "best-effort live Worker event was not delivered");
        }
        Ok(())
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
                &RecoveryRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                },
                self.worker_id(),
            )
            .await?;
        Ok(value
            .get("current")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/credential_realization",
                &CredentialRealizationRequest {
                    claim: claim.clone(),
                    receipt,
                    identity: Some(self.worker_identity.clone()),
                },
                self.worker_id(),
            )
            .await?;
        Ok(
            if value.get("applied").and_then(serde_json::Value::as_bool) == Some(true) {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    async fn worker_owns_run(
        &self,
        _identity: &WorkerIdentity,
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
                &CheckpointRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                    checkpoint: None,
                },
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
                &CheckpointRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                    checkpoint: Some(checkpoint),
                },
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
                &CheckpointRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                    checkpoint: None,
                },
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
            .post_idempotent(
                "/v1/worker/recovery/snapshot",
                &RecoveryRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                },
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
            .post_idempotent(
                "/v1/worker/dispatch/bind_sandbox",
                &BindSandboxRequest {
                    claim: claim.clone(),
                    sandbox_ref: sandbox_ref.to_string(),
                    identity: Some(self.worker_identity.clone()),
                },
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
            &EnqueueRequest {
                request,
                options: Some(options),
            },
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
        _capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_new_run",
                &ClaimNewRunRequest {
                    request,
                    identity: Some(self.worker_identity.clone()),
                },
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
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.claim_new_run(
            request,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            &capabilities,
        )
        .await
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
        _capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/deliver_and_claim",
                &DeliverAndClaimRequest {
                    input,
                    identity: Some(self.worker_identity.clone()),
                },
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
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.deliver_and_claim(
            input,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            &capabilities,
        )
        .await
    }

    async fn claim(
        &self,
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
        _capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/claim",
                &ClaimWorkerRequest {
                    identity: Some(self.worker_identity.clone()),
                },
                self.worker_id(),
            )
            .await?;
        let claimed =
            serde_json::from_value(v.get("claimed").cloned().unwrap_or(serde_json::Value::Null))
                .map_err(|e| DispatchError::Rejected(format!("decode claimed: {e}")))?;
        let _ = (lease_ms, now_ms);
        Ok(claimed)
    }

    async fn claim_retry_exhausted(
        &self,
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
        _max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        // Retry limit, clock, and lease are control-side policy. The Worker
        // supplies only its authenticated identity and receives an ordinary
        // claimed epoch to terminalize through its existing commit transport.
        let value = self
            .post(
                "/v1/worker/dispatch/claim_retry_exhausted",
                &ClaimWorkerRequest {
                    identity: Some(self.worker_identity.clone()),
                },
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

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.claim(
            &self.worker_identity.lease_owner(),
            lease_ms,
            now_ms,
            &capabilities,
        )
        .await
    }

    async fn claim_run(
        &self,
        run_id: &RunId,
        _owner: &str,
        lease_ms: u64,
        now_ms: u64,
        _capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_run",
                &ClaimRunRequest {
                    run_id: run_id.0.clone(),
                    identity: Some(self.worker_identity.clone()),
                },
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
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.claim_run(
            run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            now_ms,
            &capabilities,
        )
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
                &RenewRequest {
                    run_id: run_id.0.clone(),
                    identity: Some(self.worker_identity.clone()),
                },
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
                &ClaimWorkerRequest {
                    identity: Some(self.worker_identity.clone()),
                },
                self.worker_id(),
            )
            .await?;
        let _ = (lease_ms, now_ms);
        Ok(v.get("renewed").and_then(|r| r.as_u64()).unwrap_or(0) as usize)
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let v = self
            .post_idempotent(
                "/v1/worker/dispatch/relinquish",
                &RelinquishRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                },
                self.worker_id(),
            )
            .await?;
        Ok(
            if v.get("relinquished")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    async fn settle(
        &self,
        run_id: &RunId,
        epoch: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let v = self
            .post_idempotent(
                "/v1/worker/dispatch/settle",
                &SettleRequest {
                    run_id: run_id.0.clone(),
                    epoch,
                    outcome,
                    consumed: consumed.to_vec(),
                    identity: Some(self.worker_identity.clone()),
                },
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

    // --- server-local operational verbs: the SERVER owns manual quarantine/GC.
    // A database-less worker never legitimately drives them, so they FAIL CLOSED
    // (Rejected) rather than pretend a mutation/read the server didn't perform — a
    // silent `Ok` no-op here would let a remote worker believe it quarantined,
    // purged, or relayed when it did nothing. The pool's maintenance loop ticks purge/
    // relay but discards their result (`let _ =` / `unwrap_or(0)`), so a rejection
    // is swallowed there; anywhere the result is consumed, the fault surfaces. ---

    async fn quarantine_retry_exhausted(
        &self,
        _max_attempts: u64,
        _now_ms: u64,
    ) -> Result<usize, DispatchError> {
        Self::server_local("quarantine_retry_exhausted")
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
