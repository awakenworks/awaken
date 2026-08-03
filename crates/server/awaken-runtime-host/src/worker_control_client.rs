//! Worker lifecycle client over the same authenticated upstream used for
//! dispatch and commits. It carries no store and trusts no client-side clock.

use awaken_run_ingress::{
    RegisteredWorker, RegistryMutation, RunClaim, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerRegistration,
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

    pub async fn contribute_application(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        contribution: awaken_session_contract::ApplicationSessionContribution,
    ) -> Result<crate::ApplicationSessionControlReceipt, String> {
        let body = self
            .post(
                "/v1/worker/session/application-contribution",
                json!({
                    "identity": identity,
                    "claim": claim,
                    "contribution": contribution,
                }),
            )
            .await?;
        let contribution =
            serde_json::from_value(body.get("receipt").cloned().unwrap_or(Value::Null))
                .map_err(|error| format!("application contribution receipt decode: {error}"))?;
        let realization =
            serde_json::from_value(body.get("realization").cloned().unwrap_or(Value::Null))
                .map_err(|error| format!("Session realization directive decode: {error}"))?;
        Ok(crate::ApplicationSessionControlReceipt {
            contribution,
            realization,
        })
    }

    pub async fn resume_application_session(
        &self,
        identity: &WorkerIdentity,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<Option<awaken_session_contract::SessionRealizationDirective>, String> {
        let body = self
            .post(
                "/v1/worker/session/application-resume",
                json!({
                    "identity": identity,
                    "claim": claim,
                    "session_id": session_id,
                }),
            )
            .await?;
        body.get("realization")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| format!("Session resume directive decode: {error}"))
    }

    pub async fn activate_session_realization(
        &self,
        identity: &WorkerIdentity,
        command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<awaken_session_contract::SessionRealizationDirective, String> {
        self.realization_phase("/v1/worker/session/realization/activate", identity, command)
            .await
    }

    pub async fn begin_session_realization(
        &self,
        identity: &WorkerIdentity,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<awaken_session_contract::SessionRealizationDirective, String> {
        self.realization_phase("/v1/worker/session/realization/begin", identity, command)
            .await
    }

    pub async fn acknowledge_session_realization(
        &self,
        identity: &WorkerIdentity,
        command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<awaken_session_contract::SessionRealizationDirective, String> {
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
    ) -> Result<(), String> {
        self.post(
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
    ) -> Result<awaken_session_contract::SessionRealizationDirective, String> {
        let body = self
            .post(path, json!({ "identity": identity, "command": command }))
            .await?;
        serde_json::from_value(body.get("realization").cloned().unwrap_or(Value::Null))
            .map_err(|error| format!("Session realization directive decode: {error}"))
    }

    async fn mutation(&self, path: &str, body: Value) -> Result<RegistryMutation, String> {
        let body = self.post(path, body).await?;
        serde_json::from_value(body.get("mutation").cloned().unwrap_or(Value::Null))
            .map_err(|error| format!("worker mutation decode: {error}"))
    }
}
