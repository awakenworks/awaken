//! Worker lifecycle client over the same authenticated upstream used for
//! dispatch and commits. It carries no store and trusts no client-side clock.

use awaken_run_ingress_contract::{
    ClaimedSessionControlError, RegisteredWorker, RegistryMutation, RunClaim, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerRegistration,
};
use serde_json::{Value, json};

use awaken_worker_transport_security::WorkerUpstream;

#[derive(Debug, thiserror::Error)]
pub enum WorkerRegistrationError {
    #[error("{0}")]
    SlotOccupied(String),
    #[error("{0}")]
    Rejected(String),
}

#[derive(Clone)]
pub struct WorkerControlClient {
    upstream: WorkerUpstream,
}

impl WorkerControlClient {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }

    async fn post_response(
        &self,
        path: &str,
        body: Value,
    ) -> Result<(reqwest::StatusCode, Value), String> {
        let request = self
            .upstream
            .client()
            .post(format!("{}{}", self.upstream.base_url(), path));
        let response = self
            .upstream
            .authorize("POST", path, request)?
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("worker control transport: {error}"))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| format!("worker control response decode: {error}"))?;
        Ok((status, body))
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let (status, body) = self.post_response(path, body).await?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("worker control request rejected")
                .to_string())
        }
    }

    pub async fn register(
        &self,
        incarnation_id: impl Into<String>,
        manifest: WorkerManifest,
    ) -> Result<RegisteredWorker, String> {
        self.register_classified(incarnation_id, manifest)
            .await
            .map_err(|error| error.to_string())
    }

    /// Register once while preserving the one retryable registry conflict as a
    /// typed result. The caller owns retry timing; all other transport and
    /// validation failures remain terminal.
    pub async fn register_classified(
        &self,
        incarnation_id: impl Into<String>,
        manifest: WorkerManifest,
    ) -> Result<RegisteredWorker, WorkerRegistrationError> {
        let (status, body) = self
            .post_response(
                "/v1/worker/register",
                json!({
                    "registration": WorkerRegistration {
                        worker_id: self.upstream.worker_id().to_string(),
                        incarnation_id: incarnation_id.into(),
                        manifest,
                    }
                }),
            )
            .await
            .map_err(WorkerRegistrationError::Rejected)?;
        if !status.is_success() {
            let message = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("worker registration rejected")
                .to_string();
            return if status == reqwest::StatusCode::CONFLICT {
                Err(WorkerRegistrationError::SlotOccupied(message))
            } else {
                Err(WorkerRegistrationError::Rejected(message))
            };
        }
        serde_json::from_value(body.get("worker").cloned().unwrap_or(Value::Null)).map_err(
            |error| {
                WorkerRegistrationError::Rejected(format!("worker registration decode: {error}"))
            },
        )
    }

    pub async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
    ) -> Result<RegistryMutation, String> {
        self.mutation(
            "/v1/worker/heartbeat",
            json!({ "identity": identity, "heartbeat": heartbeat }),
        )
        .await
    }

    pub async fn current_environment_warmups(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Vec<awaken_session_contract::EnvironmentSnapshot>, String> {
        let body = self
            .post(
                "/v1/worker/environment/warmups",
                json!({ "identity": identity }),
            )
            .await?;
        serde_json::from_value(body.get("warmups").cloned().unwrap_or(Value::Null))
            .map_err(|error| format!("Environment warmup response decode: {error}"))
    }

    pub async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: Option<u64>,
    ) -> Result<RegistryMutation, String> {
        self.mutation(
            "/v1/worker/drain",
            json!({ "identity": identity, "deadline_ms": deadline_ms }),
        )
        .await
    }

    pub async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, String> {
        self.mutation("/v1/worker/quiesced", json!({ "identity": identity }))
            .await
    }

    pub async fn deregister(&self, identity: &WorkerIdentity) -> Result<RegistryMutation, String> {
        self.mutation("/v1/worker/deregister", json!({ "identity": identity }))
            .await
    }

    pub async fn resume_session(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionRealizationDirective>,
        ClaimedSessionControlError,
    > {
        let body = self
            .realization_response(
                "/v1/worker/session/resume",
                json!({
                    "identity": identity,
                    "claim": claim,
                    "session_id": session_id,
                }),
            )
            .await
            .map_err(ClaimedSessionControlError::from)?;
        body.get("realization")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                ClaimedSessionControlError::new(format!("Session resume directive decode: {error}"))
            })
    }

    /// Claim one cold terminal projection through the same identity-bound
    /// Control channel used by ordinary realization. Commands remain on the
    /// aggregate-owned polling method below.
    pub async fn claim_next_terminal_cleanup(
        &self,
        identity: &WorkerIdentity,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupAssignment>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let body = self
            .realization_response(
                "/v1/worker/session/cleanup/claim-next",
                json!({
                    "identity": identity,
                    "target": target,
                }),
            )
            .await?;
        body.get("assignment")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(format!(
                    "Session terminal cleanup assignment decode: {error}"
                ))
            })
    }

    /// Poll the Coordinator's existing durable Session cleanup operation for
    /// this exact realization generation. `Some([])` is a terminal fence that
    /// is not yet ready to execute and must retain the local projection.
    pub async fn terminal_cleanup_commands(
        &self,
        identity: &WorkerIdentity,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<Vec<awaken_session_contract::SessionCleanupCommand>>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let body = self
            .realization_response(
                "/v1/worker/session/cleanup/poll",
                json!({
                    "identity": identity,
                    "session_id": session_id,
                    "lease": lease,
                }),
            )
            .await?;
        serde_json::from_value(body.get("commands").cloned().unwrap_or(Value::Null)).map_err(
            |error| {
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(format!(
                    "Session terminal cleanup command decode: {error}"
                ))
            },
        )
    }

    /// Poll the aggregate-owned root publication command and its canonical
    /// Workspace through the same registered-Worker control channel as terminal
    /// cleanup.
    pub async fn terminal_repository_publication_command(
        &self,
        identity: &WorkerIdentity,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let body = self
            .realization_response(
                "/v1/worker/session/cleanup/repository-publication/poll",
                json!({
                    "identity": identity,
                    "session_id": session_id,
                    "lease": lease,
                }),
            )
            .await?;
        body.get("projection")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(format!(
                    "Session Repository publication projection decode: {error}"
                ))
            })
    }

    pub async fn record_terminal_repository_publication_receipt(
        &self,
        identity: &WorkerIdentity,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.realization_response(
            "/v1/worker/session/cleanup/repository-publication/complete",
            json!({
                "identity": identity,
                "session_id": session_id,
                "lease": lease,
                "receipt": receipt,
            }),
        )
        .await
        .map(|_| ())
    }

    pub async fn record_terminal_cleanup_completion(
        &self,
        identity: &WorkerIdentity,
        lease: &awaken_session_contract::SessionRealizationLease,
        completion: awaken_session_contract::SessionCleanupCompletion,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.realization_response(
            "/v1/worker/session/cleanup/complete",
            json!({
                "identity": identity,
                "lease": lease,
                "completion": completion,
            }),
        )
        .await
        .map(|_| ())
    }

    pub async fn list_session_agents(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::SessionAgentRosterEntry>, ClaimedSessionControlError>
    {
        let body = self
            .realization_response(
                "/v1/worker/session/agents/list",
                json!({
                    "identity": identity,
                    "claim": claim,
                    "session_id": session_id,
                }),
            )
            .await
            .map_err(ClaimedSessionControlError::from)?;
        serde_json::from_value(body.get("agents").cloned().unwrap_or(Value::Null)).map_err(
            |error| {
                ClaimedSessionControlError::new(format!("Session Agent roster decode: {error}"))
            },
        )
    }

    pub async fn admit_session_model_request(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, ClaimedSessionControlError> {
        let body = self
            .realization_response(
                "/v1/worker/session/model-request/admit",
                json!({
                    "identity": identity,
                    "claim": claim,
                    "session_id": session_id,
                    "thread_id": thread_id,
                    "run_id": run_id,
                }),
            )
            .await
            .map_err(ClaimedSessionControlError::from)?;
        body.get("admitted")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                ClaimedSessionControlError::new(
                    "Session model-request admission response omitted its decision",
                )
            })
    }

    pub async fn admit_session_run_activity(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        session_id: &str,
        agent_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        mode: awaken_session_contract::SessionRunActivityAdmissionMode,
    ) -> Result<awaken_session_contract::SessionRunActivityAdmission, ClaimedSessionControlError>
    {
        let body = self
            .realization_response(
                "/v1/worker/session/run-activity/admit",
                json!({
                    "identity": identity,
                    "claim": claim,
                    "session_id": session_id,
                    "agent_id": agent_id,
                    "run_id": run_id,
                    "mode": mode,
                }),
            )
            .await
            .map_err(ClaimedSessionControlError::from)?;
        serde_json::from_value(body.get("admission").cloned().unwrap_or(Value::Null)).map_err(
            |error| {
                ClaimedSessionControlError::new(format!(
                    "Session Run activity admission response decode: {error}"
                ))
            },
        )
    }

    pub async fn send_session_agent_message(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        command: awaken_session_contract::SessionAgentMessageCommand,
    ) -> Result<awaken_session_contract::SessionAgentMessageReceipt, ClaimedSessionControlError>
    {
        let body = self
            .realization_response(
                "/v1/worker/session/agents/send",
                json!({ "identity": identity, "claim": claim, "command": command }),
            )
            .await
            .map_err(ClaimedSessionControlError::from)?;
        serde_json::from_value(body.get("receipt").cloned().unwrap_or(Value::Null)).map_err(
            |error| {
                ClaimedSessionControlError::new(format!("Session Agent receipt decode: {error}"))
            },
        )
    }

    pub async fn settle_session_agent_boundary(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        command: awaken_session_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), ClaimedSessionControlError> {
        self.realization_response(
            "/v1/worker/session/agents/settle",
            json!({ "identity": identity, "claim": claim, "command": command }),
        )
        .await
        .map(|_| ())
        .map_err(ClaimedSessionControlError::from)
    }

    pub async fn activate_session_realization(
        &self,
        identity: &WorkerIdentity,
        command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.realization_phase("/v1/worker/session/realization/activate", identity, command)
            .await
    }

    pub async fn begin_session_realization(
        &self,
        identity: &WorkerIdentity,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.realization_phase("/v1/worker/session/realization/begin", identity, command)
            .await
    }

    pub async fn acknowledge_session_realization(
        &self,
        identity: &WorkerIdentity,
        command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.realization_phase(
            "/v1/worker/session/realization/acknowledge",
            identity,
            command,
        )
        .await
    }

    pub async fn fail_session_realization(
        &self,
        identity: &WorkerIdentity,
        command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.realization_response(
            "/v1/worker/session/realization/fail",
            json!({ "identity": identity, "command": command }),
        )
        .await
        .map(|_| ())
    }

    async fn realization_phase<T: serde::Serialize>(
        &self,
        path: &str,
        identity: &WorkerIdentity,
        command: T,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let body = self
            .realization_response(path, json!({ "identity": identity, "command": command }))
            .await?;
        serde_json::from_value(body.get("realization").cloned().unwrap_or(Value::Null)).map_err(
            |error| {
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(format!(
                    "Session realization directive decode: {error}"
                ))
            },
        )
    }

    async fn realization_response(
        &self,
        path: &str,
        body: Value,
    ) -> Result<Value, awaken_session_contract::SessionRealizationControlFailure> {
        let (status, body) = self
            .post_response(path, body)
            .await
            .map_err(awaken_session_contract::SessionRealizationControlFailure::Unavailable)?;
        if status.is_success() {
            return Ok(body);
        }
        if let Some(error) = body.get("realization_error") {
            return serde_json::from_value(error.clone()).map_or_else(
                |decode| {
                    Err(
                        awaken_session_contract::SessionRealizationControlFailure::Unavailable(
                            format!("Session realization failure decode: {decode}"),
                        ),
                    )
                },
                Err,
            );
        }
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Unavailable(
                body.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("worker control request rejected")
                    .to_string(),
            ),
        )
    }

    async fn mutation(&self, path: &str, body: Value) -> Result<RegistryMutation, String> {
        let body = self.post(path, body).await?;
        serde_json::from_value(body.get("mutation").cloned().unwrap_or(Value::Null))
            .map_err(|error| format!("worker mutation decode: {error}"))
    }
}
