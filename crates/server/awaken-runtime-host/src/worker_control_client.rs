//! Worker lifecycle client over the same authenticated upstream used for
//! dispatch and commits. It carries no store and trusts no client-side clock.

use awaken_run_ingress::{
    RegisteredWorker, RegistryMutation, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerRegistration,
};
use serde_json::{Value, json};

use crate::worker_security::{WORKER_ID_HEADER, WorkerUpstream};

#[derive(Clone)]
pub struct WorkerControlClient {
    upstream: WorkerUpstream,
}

impl WorkerControlClient {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let response = self
            .upstream
            .client()
            .post(format!("{}{}", self.upstream.base_url(), path))
            .header(WORKER_ID_HEADER, self.upstream.worker_id())
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("worker control transport: {error}"))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| format!("worker control response decode: {error}"))?;
        if !status.is_success() {
            return Err(body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("worker control request rejected")
                .to_string());
        }
        Ok(body)
    }

    pub async fn register(
        &self,
        incarnation_id: impl Into<String>,
        manifest: WorkerManifest,
    ) -> Result<RegisteredWorker, String> {
        let body = self
            .post(
                "/v1/worker/register",
                json!({
                    "registration": WorkerRegistration {
                        worker_id: self.upstream.worker_id().to_string(),
                        incarnation_id: incarnation_id.into(),
                        manifest,
                    }
                }),
            )
            .await?;
        serde_json::from_value(body.get("worker").cloned().unwrap_or(Value::Null))
            .map_err(|error| format!("worker registration decode: {error}"))
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

    async fn mutation(&self, path: &str, body: Value) -> Result<RegistryMutation, String> {
        let body = self.post(path, body).await?;
        serde_json::from_value(body.get("mutation").cloned().unwrap_or(Value::Null))
            .map_err(|error| format!("worker mutation decode: {error}"))
    }
}
