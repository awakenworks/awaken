//! Control-side HTTP adapter for the Managed Deployment Session launch port.

use std::time::Duration;

use awaken_protocol_managed::{
    DEPLOYMENT_SESSION_LAUNCH_PATH, DeploymentLaunch, DeploymentLaunchOutcome,
    DeploymentSessionLauncher,
};
use axum::http::StatusCode;

const IDEMPOTENT_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
pub struct HttpDeploymentSessionLauncher {
    base_url: String,
    bearer_token: String,
    client: reqwest::Client,
}

impl HttpDeploymentSessionLauncher {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let bearer_token = bearer_token.into();
        if base_url.is_empty() || bearer_token.trim().is_empty() {
            return Err("Coordinator URL and Deployment Session launch token are required".into());
        }
        let parsed = reqwest::Url::parse(&base_url)
            .map_err(|error| format!("invalid Coordinator launch URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(
                "Coordinator launch URL must be an http(s) base URL without query or fragment"
                    .into(),
            );
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|error| format!("construct Deployment Session HTTP client: {error}"))?;
        Ok(Self {
            base_url,
            bearer_token,
            client,
        })
    }

    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }
}

#[async_trait::async_trait]
impl DeploymentSessionLauncher for HttpDeploymentSessionLauncher {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
        let mut last_unavailable = None;
        for attempt in 1..=IDEMPOTENT_ATTEMPTS {
            let response = self
                .client
                .post(format!(
                    "{}{}",
                    self.base_url, DEPLOYMENT_SESSION_LAUNCH_PATH
                ))
                .bearer_auth(&self.bearer_token)
                .json(&request)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    last_unavailable = Some(error.to_string());
                    if attempt < IDEMPOTENT_ATTEMPTS {
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    break;
                }
            };
            if response.status() == StatusCode::UNAUTHORIZED {
                return DeploymentLaunchOutcome::Unavailable {
                    message: "Coordinator rejected Deployment Session launch credentials".into(),
                };
            }
            let status = response.status();
            let decoded = response.json::<DeploymentLaunchOutcome>().await;
            if status.is_success() {
                return decoded.unwrap_or_else(|error| DeploymentLaunchOutcome::Unavailable {
                    message: format!("Coordinator launch response decode failed: {error}"),
                });
            }
            if status == StatusCode::SERVICE_UNAVAILABLE && attempt < IDEMPOTENT_ATTEMPTS {
                last_unavailable = decoded
                    .ok()
                    .and_then(|outcome| match outcome {
                        DeploymentLaunchOutcome::Unavailable { message } => Some(message),
                        _ => None,
                    })
                    .or_else(|| Some(format!("Coordinator returned {status}")));
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            return decoded.unwrap_or_else(|_| DeploymentLaunchOutcome::Unavailable {
                message: format!("Coordinator launch returned {status}"),
            });
        }
        DeploymentLaunchOutcome::Unavailable {
            message: last_unavailable
                .unwrap_or_else(|| "Coordinator Deployment Session launch unavailable".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_protocol_managed::deployment_session_launch_router;
    use serde_json::json;

    use super::*;
    use awaken_protocol_managed::DeploymentRunError;

    fn request() -> DeploymentLaunch {
        serde_json::from_value(json!({
            "deployment_id": "depl_http",
            "deployment_run_id": "drun_http",
            "workspace_id": "workspace-a",
            "agent": {"id": "agent-a", "type": "agent", "version": 3},
            "environment_id": "env_local",
            "metadata": {},
            "initial_events": [],
            "resources": [],
            "vault_ids": []
        }))
        .unwrap()
    }

    struct RecordingLauncher {
        calls: AtomicUsize,
        unavailable_before: usize,
        run_ids: Mutex<Vec<String>>,
        semantic_failure: bool,
    }

    #[async_trait::async_trait]
    impl DeploymentSessionLauncher for RecordingLauncher {
        async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.run_ids
                .lock()
                .unwrap()
                .push(request.deployment_run_id.clone());
            if self.semantic_failure {
                return DeploymentLaunchOutcome::Failed {
                    error: DeploymentRunError::SessionCreationRejectedError {
                        message: "invalid launch".into(),
                    },
                };
            }
            if call < self.unavailable_before {
                return DeploymentLaunchOutcome::Unavailable {
                    message: "injected availability failure".into(),
                };
            }
            DeploymentLaunchOutcome::Created {
                session_id: "sesn_http".into(),
            }
        }
    }

    async fn server_for(
        launcher: Arc<dyn DeploymentSessionLauncher>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = deployment_session_launch_router(launcher, "secret-token").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn retries_only_indeterminate_launches_with_the_same_run_id() {
        // Cause/effect decision table: R1 two 503 outcomes before success ->
        // exactly three attempts carrying the same durable DeploymentRun id;
        // R2 a semantic Session rejection -> one attempt and the typed failure;
        // R3 bad bearer token -> no application call and an indeterminate result.
        let transient = Arc::new(RecordingLauncher {
            calls: AtomicUsize::new(0),
            unavailable_before: 2,
            run_ids: Mutex::new(Vec::new()),
            semantic_failure: false,
        });
        let (url, task) = server_for(transient.clone()).await;
        let client = HttpDeploymentSessionLauncher::new(&url, "secret-token").unwrap();
        assert!(
            matches!(
                client.launch(request()).await,
                DeploymentLaunchOutcome::Created { ref session_id } if session_id == "sesn_http"
            ),
            "R1"
        );
        assert_eq!(transient.calls.load(Ordering::SeqCst), 3, "R1");
        assert_eq!(
            transient.run_ids.lock().unwrap().as_slice(),
            ["drun_http", "drun_http", "drun_http"],
            "R1"
        );
        task.abort();

        let semantic = Arc::new(RecordingLauncher {
            calls: AtomicUsize::new(0),
            unavailable_before: 0,
            run_ids: Mutex::new(Vec::new()),
            semantic_failure: true,
        });
        let (url, task) = server_for(semantic.clone()).await;
        let client = HttpDeploymentSessionLauncher::new(&url, "secret-token").unwrap();
        assert!(
            matches!(
                client.launch(request()).await,
                DeploymentLaunchOutcome::Failed { .. }
            ),
            "R2"
        );
        assert_eq!(semantic.calls.load(Ordering::SeqCst), 1, "R2");

        let unauthorized = HttpDeploymentSessionLauncher::new(&url, "wrong-token").unwrap();
        assert!(
            matches!(
                unauthorized.launch(request()).await,
                DeploymentLaunchOutcome::Unavailable { .. }
            ),
            "R3"
        );
        assert_eq!(semantic.calls.load(Ordering::SeqCst), 1, "R3");
        task.abort();
    }

    #[tokio::test]
    async fn response_loss_after_creation_retries_the_same_business_command() {
        // Cause/effect rule A1: the Coordinator applies the launch but the TCP
        // connection closes before its response -> Control retries the identical
        // DeploymentRun command and receives the original Session identity. The
        // local launcher's separate test owns the one-Session/one-initial-batch
        // effect; this test owns the ambiguous network window.
        let launcher = Arc::new(RecordingLauncher {
            calls: AtomicUsize::new(0),
            unavailable_before: 0,
            run_ids: Mutex::new(Vec::new()),
            semantic_failure: false,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_launcher = launcher.clone();
        let task = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            let _applied = server_launcher.launch(request()).await;
            drop(connection);
            let app = deployment_session_launch_router(server_launcher, "secret-token").unwrap();
            axum::serve(listener, app).await.unwrap();
        });

        let client =
            HttpDeploymentSessionLauncher::new(format!("http://{address}"), "secret-token")
                .unwrap();
        assert!(matches!(
            client.launch(request()).await,
            DeploymentLaunchOutcome::Created { ref session_id } if session_id == "sesn_http"
        ));
        assert_eq!(launcher.calls.load(Ordering::SeqCst), 2, "A1");
        assert_eq!(
            launcher.run_ids.lock().unwrap().as_slice(),
            ["drun_http", "drun_http"],
            "A1"
        );
        task.abort();
    }

    #[test]
    fn requires_complete_private_endpoint_configuration() {
        // Causes: malformed/missing URL or missing token. Effect: fail before a
        // private request can be attempted.
        assert!(HttpDeploymentSessionLauncher::new("", "secret-token").is_err());
        assert!(HttpDeploymentSessionLauncher::new("http://", "secret-token").is_err());
        assert!(
            HttpDeploymentSessionLauncher::new("http://coordinator?mode=launch", "secret-token")
                .is_err()
        );
        assert!(HttpDeploymentSessionLauncher::new("http://coordinator", " ").is_err());
    }
}
