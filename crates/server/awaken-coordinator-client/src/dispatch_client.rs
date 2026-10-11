// @formal entrypoint remote_worker.maintenance_scheduler_respects_adapter_ownership
// @formal entrypoint session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
// @formal evidence remote_worker.execution_requires_claimed_snapshot
// @formal evidence remote_worker.recovery_projection_is_committed_prefix
// @formal evidence remote_worker.single_live_claim_owner
//! The database-less worker's dispatch client: a `Dispatch` implementation whose
//! claim/settle verbs are HTTP calls to the Coordinator's registered Worker router,
//! so a worker drives runs without ever opening the store.
//!
//! Only the worker verbs cross the wire — `enqueue`, `claim_new_run`, `claim`,
//! `renew_lease` and `settle`. Claimed commits use the separate atomic
//! server operation; this transport exposes no check-then-commit fence read. The
//! Coordinator administration, maintenance, readback and inbox/outbox writes
//! have no implementation on this transport. `InboxRead::list` is the Worker's
//! read projection: pending inputs already travel in `Claimed.pending`.
//! Bind this owner with its required claimed-commit service through
//! `DispatchBinding::remote` before constructing a Worker or pool.

use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::event::Observation as StreamObservation;
use awaken_agent_contract::stream::sink::Error as StreamError;
use awaken_dispatch_contract::{
    AttemptAdmission, AttemptExecutionRequest, BindSandboxRequest, CheckpointRequest,
    ClaimNewRunRequest, ClaimRunRequest, ClaimWorkerRequest, Claimed,
    ClaimedCredentialDeliveryTransport, CredentialDeliveryRefreshRequest,
    CredentialRealizationReceipt, CredentialRealizationRequest, DeliverAndClaimRequest,
    DispatchError, DispatchOutcome, EnqueueRequest, LeaseRenewalOutcome, PendingInput,
    PendingRecord, RecoveryRequest, RenewRequest, RunClaim, RunDispatch,
    SessionRunReservationResolution, SessionRunReservationResolutionRequest, SettleOutcome,
    SettleRequest, StreamEventRequest, StreamObservationRequest, SubmitOptions,
};

use awaken_dispatch_contract::{
    ClaimedStreamPublisher, WorkerIdentity, WorkerSnapshot,
    worker_credential_realization_capabilities,
};
use awaken_worker_transport_security::{WorkerRequestAuthorizer, WorkerUpstream};

async fn dispatch_response_error(response: reqwest::Response) -> DispatchError {
    let status = response.status();
    let detail = response
        .text()
        .await
        .unwrap_or_default()
        .trim()
        .chars()
        .take(512)
        .collect::<String>();
    let message = format!(
        "dispatch transport server returned {status}{}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    );
    match status {
        reqwest::StatusCode::CONFLICT => DispatchError::Conflict(message),
        reqwest::StatusCode::BAD_GATEWAY
        | reqwest::StatusCode::SERVICE_UNAVAILABLE
        | reqwest::StatusCode::GATEWAY_TIMEOUT => DispatchError::Unavailable(message),
        _ => DispatchError::Rejected(message),
    }
}

const IDEMPOTENT_TRANSPORT_ATTEMPTS: usize = 3;
const IDEMPOTENT_TRANSPORT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// Private Worker-to-Coordinator wire command. Keeping the request-local retry
/// hint here prevents a scheduler mechanism from entering the public ingress
/// contract or the Anthropic Managed API surface.
#[derive(Serialize)]
struct RelinquishCommand<'a> {
    claim: &'a RunClaim,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
    identity: &'a WorkerIdentity,
}

fn reject_worker_supplied_execution_binding(
    binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
) -> Result<(), DispatchError> {
    if binding.is_some() {
        return Err(DispatchError::Rejected("execution domain binding requires a trusted Coordinator provider; Worker JSON cannot establish it".into()));
    }
    Ok(())
}

fn installed_worker_credential_capabilities(
    worker: &WorkerSnapshot,
) -> Result<awaken_credential_contract::CredentialRealizationCapabilities, DispatchError> {
    worker_credential_realization_capabilities(worker)
        .map_err(|error| DispatchError::Rejected(error.to_string()))
}

/// A `Dispatch` store whose worker verbs are HTTP calls to a cell server.
/// Worker HTTP transport supplies execution and claim-bound RPC roles only.
/// Local authority is rejected by the type checker before any request.
/// ```compile_fail,E0599
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::DispatchAdministration;
/// use awaken_agent_contract::agent::run::Id;
/// async fn cancel_peer(worker: &HttpDispatchQueue) {
///     worker.cancel(&Id("no-such-run".into())).await;
/// }
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::DispatchAdministration;
/// fn required<T: DispatchAdministration>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::Inbox;
/// fn required<T: Inbox>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::Outbox;
/// fn required<T: Outbox>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::DispatchMaintenance;
/// fn required<T: DispatchMaintenance>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::DispatchReads;
/// fn required<T: DispatchReads>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::DispatchCommitAuthority;
/// fn required<T: DispatchCommitAuthority>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::DispatchControlReads;
/// fn required<T: DispatchControlReads>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::DispatchControlClaims;
/// fn required<T: DispatchControlClaims>() {}
/// required::<HttpDispatchQueue>();
/// ```
/// ```compile_fail
/// use awaken_coordinator_client::HttpDispatchQueue;
/// use awaken_dispatch_contract::SessionReservationAuthority;
/// fn required<T: SessionReservationAuthority>() {}
/// required::<HttpDispatchQueue>();
/// ```
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
    Arc<dyn awaken_dispatch_execution::RemoteDispatch>,
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
            client: awaken_outbound_http::rustls_client_builder(
                awaken_outbound_http::TransportDeadline::total(std::time::Duration::from_secs(30)),
            )
            .no_proxy()
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
            return Err(dispatch_response_error(resp).await);
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
                // Reuse the one bounded response decoder. Retrying must not
                // erase the Coordinator's precise failure evidence and leave
                // operators with only an unhelpful status code.
                last_retryable_error = Some(dispatch_response_error(response).await.to_string());
                if attempt < IDEMPOTENT_TRANSPORT_ATTEMPTS {
                    tokio::time::sleep(IDEMPOTENT_TRANSPORT_RETRY_DELAY).await;
                    continue;
                }
                break;
            }
            if !response.status().is_success() {
                return Err(dispatch_response_error(response).await);
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
        Err(DispatchError::Unavailable(
            last_retryable_error.unwrap_or_else(|| {
                "idempotent dispatch transport exhausted without a response".to_string()
            }),
        ))
    }

    fn worker_id(&self) -> &str {
        &self.worker_identity.worker_id
    }

    async fn post_stream_observation(
        &self,
        claim: &RunClaim,
        observation: StreamObservation,
    ) -> Result<(), StreamError> {
        // `classify` is the single routing truth. Complete content/lifecycle Facts
        // are committed through the durable path and must never be posted to the
        // Coordinator's live-only observation endpoint.
        if !awaken_agent_contract::event::classify(&observation.event.kind).live {
            return Ok(());
        }
        let result = self
            .post(
                "/v1/worker/dispatch/stream",
                &StreamObservationRequest {
                    request: StreamEventRequest {
                        claim: claim.clone(),
                        identity: self.worker_identity.clone(),
                        event: observation.event,
                    },
                    assistant_response: observation.assistant_response,
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
impl ClaimedStreamPublisher for HttpDispatchQueue {
    async fn publish(&self, claim: &RunClaim, event: StreamEvent) -> Result<(), StreamError> {
        self.post_stream_observation(claim, event.into()).await
    }

    async fn publish_observation(
        &self,
        claim: &RunClaim,
        observation: StreamObservation,
    ) -> Result<(), StreamError> {
        self.post_stream_observation(claim, observation).await
    }
}

#[async_trait]
impl ClaimedCredentialDeliveryTransport for HttpDispatchQueue {
    async fn refresh_credential_delivery(
        &self,
        claim: &RunClaim,
        delivery_request: awaken_credential_contract::AttemptCredentialDeliveryRequest,
    ) -> Result<Option<awaken_credential_contract::AttemptCredentialDelivery>, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/credential_delivery/refresh",
                &CredentialDeliveryRefreshRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                    delivery_request,
                },
                self.worker_id(),
            )
            .await?;
        serde_json::from_value(value.get("delivery").cloned().unwrap_or_default()).map_err(
            |error| DispatchError::Rejected(format!("decode credential delivery: {error}")),
        )
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaims for HttpDispatchQueue {
    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let value = self
            .post_idempotent(
                "/v1/worker/dispatch/reservation/resolve",
                &SessionRunReservationResolutionRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                    resolution,
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

    async fn claim_new_run(
        &self,
        request: RunDispatch,
        _owner: &str,
        lease_ms: u64,
        _capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
        let _ = lease_ms;
        Ok(claimed)
    }

    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.claim_new_run(
            request,
            &worker.identity.lease_owner(),
            lease_ms,
            &capabilities,
        )
        .await
    }

    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        _owner: &str,
        lease_ms: u64,
        _capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
        let _ = lease_ms;
        Ok(claimed)
    }

    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.deliver_and_claim(
            input,
            &worker.identity.lease_owner(),
            lease_ms,
            &capabilities,
        )
        .await
    }

    async fn claim(
        &self,
        _owner: &str,
        lease_ms: u64,
        _capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
        let _ = lease_ms;
        Ok(claimed)
    }

    async fn claim_retry_exhausted(
        &self,
        _owner: &str,
        lease_ms: u64,
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
        let _ = lease_ms;
        Ok(claimed)
    }

    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.claim(&self.worker_identity.lease_owner(), lease_ms, &capabilities)
            .await
    }

    async fn claim_run(
        &self,
        run_id: &RunId,
        _owner: &str,
        lease_ms: u64,
        _capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
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
        let _ = lease_ms;
        Ok(claimed)
    }

    async fn claim_run_compatible(
        &self,
        run_id: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let capabilities = installed_worker_credential_capabilities(worker)?;
        self.claim_run(
            run_id,
            &worker.identity.lease_owner(),
            lease_ms,
            &capabilities,
        )
        .await
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchClaimOwnership for HttpDispatchQueue {
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        let value = self
            .post(
                "/v1/worker/dispatch/claim_is_current",
                &RecoveryRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                    session_work_authority: None,
                },
                self.worker_id(),
            )
            .await?;
        Ok(value
            .get("current")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }
}

#[async_trait]
impl awaken_dispatch_contract::DispatchExecution for HttpDispatchQueue {
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

    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        let v = self
            .post(
                "/v1/worker/dispatch/renew",
                &RenewRequest {
                    run_id: claim.run_id.0.clone(),
                    lease_epoch: claim.epoch,
                    identity: Some(self.worker_identity.clone()),
                },
                self.worker_id(),
            )
            .await?;
        let _ = lease_ms;
        serde_json::from_value(
            v.get("outcome")
                .cloned()
                .ok_or_else(|| DispatchError::Rejected("renewal response has no outcome".into()))?,
        )
        .map_err(|error| DispatchError::Rejected(format!("invalid renewal outcome: {error}")))
    }

    async fn begin_attempt(
        &self,
        claim: &RunClaim,
        binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        reject_worker_supplied_execution_binding(binding)?;
        let value = self.attempt_command(claim, "begin").await?;
        serde_json::from_value(
            value
                .get("admission")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode attempt admission: {error}")))
    }

    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        reject_worker_supplied_execution_binding(binding)?;
        let value = self.attempt_command(claim, "handoff").await?;
        serde_json::from_value(
            value
                .get("admission")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|error| DispatchError::Rejected(format!("decode attempt admission: {error}")))
    }

    async fn close_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let value = self.attempt_command(claim, "close").await?;
        Ok(
            if value.get("closed").and_then(serde_json::Value::as_bool) == Some(true) {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    async fn finish_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let value = self.attempt_command(claim, "finish").await?;
        Ok(
            if value
                .get("finished")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                SettleOutcome::Applied
            } else {
                SettleOutcome::Fenced
            },
        )
    }

    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let v = self
            .post_idempotent(
                "/v1/worker/dispatch/relinquish",
                &RelinquishCommand {
                    claim,
                    retry_after_ms: None,
                    identity: &self.worker_identity,
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

    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        let v = self
            .post_idempotent(
                "/v1/worker/dispatch/relinquish",
                &RelinquishCommand {
                    claim,
                    retry_after_ms: Some(retry_after_ms),
                    identity: &self.worker_identity,
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
}

#[async_trait]
impl awaken_dispatch_contract::DispatchEnqueue for HttpDispatchQueue {
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
}

#[async_trait]
impl awaken_dispatch_contract::DispatchSettlement for HttpDispatchQueue {
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
}

#[async_trait]
impl awaken_dispatch_contract::ClaimedCheckpointTransport for HttpDispatchQueue {
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
        session_work_authority: Option<&awaken_worker_contract::SessionWorkAuthorityReceipt>,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, DispatchError>
    {
        let value = self
            .post_idempotent(
                "/v1/worker/recovery/snapshot",
                &RecoveryRequest {
                    claim: claim.clone(),
                    identity: Some(self.worker_identity.clone()),
                    session_work_authority: session_work_authority.cloned(),
                },
                self.worker_id(),
            )
            .await?;
        serde_json::from_value(value.get("snapshot").cloned().unwrap_or_default())
            .map_err(|error| DispatchError::Rejected(format!("decode recovery snapshot: {error}")))
    }
}

#[async_trait]
impl awaken_dispatch_contract::InboxRead for HttpDispatchQueue {
    // The one legitimate read no-op (NOT fail-closed): the worker's own drive calls
    // `Inbox::list` to drain a thread's unbound input, but a db-less worker already
    // receives its run's pending input in `Claimed.pending`, and the unbound-inbox
    // drain is a server-local concern — so an empty list is the *correct* answer
    // here, not a pretended one. Failing this closed would break every db-less
    // drive (`worker.rs` consumes it with `?`).
    async fn list(&self, _thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        Ok(Vec::new())
    }
}

impl HttpDispatchQueue {
    async fn attempt_command(
        &self,
        claim: &RunClaim,
        action: &str,
    ) -> Result<serde_json::Value, DispatchError> {
        self.post_idempotent(
            &format!("/v1/worker/dispatch/attempt/{action}"),
            &AttemptExecutionRequest {
                claim: claim.clone(),
                identity: Some(self.worker_identity.clone()),
            },
            self.worker_id(),
        )
        .await
    }
}
