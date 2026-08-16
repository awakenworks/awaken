//! Database-less Worker's atomic claimed-commit HTTP client.

use std::sync::Arc;

use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::CommitReceipt;
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_run_ingress_contract::{
    ClaimedCommitCommand, ClaimedCommitRequest, ClaimedRunCommit, RunClaim, WorkerIdentity,
};
use awaken_worker_transport_security::{WORKER_ID_HEADER, WorkerRequestAuthorizer, WorkerUpstream};

const TRANSPORT_ATTEMPTS: usize = 3;
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

fn server_error_detail(body: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.trim().to_string());
    let mut chars = detail.chars();
    let bounded = chars.by_ref().take(512).collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else if bounded.is_empty() {
        "response body was empty".to_string()
    } else {
        bounded
    }
}

/// Atomic claimed-run commit used by a database-less Worker.
pub struct RemoteClaimedRunCommit {
    base_url: String,
    client: reqwest::Client,
    identity: WorkerIdentity,
    request_authorizer: Option<Arc<dyn WorkerRequestAuthorizer>>,
}

impl RemoteClaimedRunCommit {
    pub fn new(base_url: impl Into<String>, identity: WorkerIdentity) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("the default claimed-commit HTTP client should build"),
            identity,
            request_authorizer: None,
        }
    }

    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn with_request_authorizer(mut self, authorizer: Arc<dyn WorkerRequestAuthorizer>) -> Self {
        self.request_authorizer = Some(authorizer.bind_worker_identity(&self.identity));
        self
    }

    fn authorize(
        &self,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, CommitError> {
        match &self.request_authorizer {
            Some(authorizer) => authorizer
                .authorize("POST", path, &self.identity.worker_id, request)
                .map_err(CommitError::Rejected),
            None => Ok(request.header(WORKER_ID_HEADER, &self.identity.worker_id)),
        }
    }
}

#[async_trait::async_trait]
impl ClaimedRunCommit for RemoteClaimedRunCommit {
    async fn commit(
        &self,
        _claim: &RunClaim,
        _commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        Err(CommitError::Rejected(
            "remote Worker commits require a versioned CommitOperation".to_string(),
        ))
    }

    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        let path = "/v1/worker/commit-claimed";
        let request_body = ClaimedCommitRequest::new(command, self.identity.clone());
        let mut last_transport_error = None;
        for attempt in 1..=TRANSPORT_ATTEMPTS {
            let request = self.client.post(format!("{}{path}", self.base_url));
            let response = match self
                .authorize(path, request)?
                .json(&request_body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    last_transport_error = Some(format!("claimed commit transport: {error}"));
                    if attempt < TRANSPORT_ATTEMPTS {
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    break;
                }
            };
            let status = response.status();
            if !status.is_success() {
                let detail = match response.text().await {
                    Ok(body) => server_error_detail(&body),
                    Err(error) => format!("response body could not be read: {error}"),
                };
                let message = format!("claimed commit server returned {status}: {detail}");
                if !status.is_server_error() {
                    return Err(CommitError::Rejected(message));
                }
                last_transport_error = Some(message);
                if attempt < TRANSPORT_ATTEMPTS {
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                break;
            }
            match response.json().await {
                Ok(receipt) => return Ok(receipt),
                Err(error) => {
                    last_transport_error =
                        Some(format!("commit receipt transport/decode: {error}"));
                    if attempt < TRANSPORT_ATTEMPTS {
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                }
            }
        }
        Err(CommitError::Rejected(last_transport_error.unwrap_or_else(
            || "claimed commit transport exhausted without a response".to_string(),
        )))
    }
}

pub fn remote_claimed_commit(
    upstream: &WorkerUpstream,
) -> Result<Arc<dyn ClaimedRunCommit>, String> {
    let identity = upstream.worker_identity().cloned().ok_or_else(|| {
        "remote Worker commit transport requires a registered identity".to_string()
    })?;
    Ok(Arc::new(
        RemoteClaimedRunCommit::new(upstream.base_url(), identity)
            .with_client(upstream.client().clone())
            .with_request_authorizer(upstream.request_authorizer()),
    ))
}
