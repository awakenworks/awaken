//! Portable client-side seam for exact, short-lived managed inference access.
//!
//! The Cloud implementation owns identity, entitlement, pricing, quota, route
//! selection, Provider credential custody, usage and charging. This module owns
//! only the local consumer contract: request one exact public candidate, use the
//! returned native Gateway endpoint/capability, then close that grant. It never
//! receives or persists Cloud-internal target coordinates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};

use crate::executor_from_materialized_endpoint;

pub const BROKERED_INFERENCE_ACCESS_CAPABILITY: &str = "inference.brokered_grant";
pub const BROKERED_ROUTE_PREFIX: &str = "brokered:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokeredInferenceRequest {
    pub provider: String,
    pub original_model_id: String,
    pub native_protocol: String,
    pub local_run_correlation: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone)]
pub struct BrokeredInferenceLease {
    pub grant_id: String,
    pub gateway_base_url: String,
    pub capability: RedactedString,
    pub grant_expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokeredInferenceError {
    #[error("Cloud authentication is required")]
    AuthenticationRequired,
    #[error("Cloud account selection is required")]
    AccountSelectionRequired,
    #[error("an active subscription is required")]
    SubscriptionRequired,
    #[error("the model is not included in the current entitlement")]
    ModelNotEntitled,
    #[error("the selected managed model is unavailable")]
    ModelUnavailable,
    #[error("the Cloud balance or hard cap is exhausted")]
    InsufficientBalance,
    #[error("managed inference quota is exhausted")]
    QuotaExceeded {
        retry_after: Option<std::time::Duration>,
    },
    #[error("managed inference is temporarily unavailable")]
    TemporarilyUnavailable,
    #[error("the brokered inference request is invalid")]
    InvalidRequest,
}

impl BrokeredInferenceError {
    fn into_llm(self) -> LlmError {
        match self {
            Self::AuthenticationRequired | Self::AccountSelectionRequired => {
                LlmError::LoginRequired(self.to_string())
            }
            Self::SubscriptionRequired | Self::ModelNotEntitled => {
                LlmError::Unauthorized(self.to_string())
            }
            Self::ModelUnavailable => LlmError::ModelNotFound(self.to_string()),
            Self::InsufficientBalance => LlmError::UsageLimit {
                message: self.to_string(),
                reset_after: None,
            },
            Self::QuotaExceeded { retry_after } => LlmError::RateLimited {
                message: "managed inference quota is exhausted".into(),
                retry_after,
            },
            Self::TemporarilyUnavailable => LlmError::Provider(self.to_string()),
            Self::InvalidRequest => LlmError::Binding(self.to_string()),
        }
    }
}

#[async_trait]
pub trait BrokeredInferenceClient: Send + Sync {
    async fn create_grant(
        &self,
        request: BrokeredInferenceRequest,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError>;

    /// Renew an issued grant for callers that need to perform another native
    /// request without changing its exact target.
    async fn renew_grant(
        &self,
        grant_id: &str,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError>;

    /// Best-effort release after one native request. A failed close never changes
    /// an already-returned model response; the short server TTL remains the
    /// bounded cleanup authority.
    async fn close_grant(&self, grant_id: &str) -> Result<(), BrokeredInferenceError>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct BrokeredModel {
    pub provider: String,
    pub original_model_id: String,
    pub native_protocol: String,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub capabilities: std::collections::BTreeSet<String>,
    pub route_publication_revision: u64,
}

#[async_trait]
pub trait BrokeredModelCatalogClient: Send + Sync {
    async fn readiness(&self) -> Result<(), BrokeredInferenceError>;
    async fn list_models(&self) -> Result<Vec<BrokeredModel>, BrokeredInferenceError>;
}

const BROKERED_CATALOG_MAX_ATTEMPTS: usize = 3;
const BROKERED_CATALOG_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// Read one authoritative Cloud catalog snapshot. Both operations are GETs, so a
/// transient transport/5xx failure can be retried without duplicating grants,
/// usage, or billing side effects. Stable identity/entitlement failures remain
/// fail-closed on the first attempt.
async fn fetch_brokered_catalog(
    client: &dyn BrokeredModelCatalogClient,
) -> Result<Vec<BrokeredModel>, BrokeredInferenceError> {
    for attempt in 1..=BROKERED_CATALOG_MAX_ATTEMPTS {
        let result = async {
            client.readiness().await?;
            client.list_models().await
        }
        .await;
        match result {
            Ok(models) => return Ok(models),
            Err(BrokeredInferenceError::TemporarilyUnavailable)
                if attempt < BROKERED_CATALOG_MAX_ATTEMPTS =>
            {
                tokio::time::sleep(BROKERED_CATALOG_RETRY_DELAY).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the bounded catalog retry loop always returns")
}

#[derive(Clone)]
pub struct HttpBrokeredInferenceClient {
    http: reqwest::Client,
    base_url: String,
    access_token: RedactedString,
    client_instance_id: String,
}

impl std::fmt::Debug for HttpBrokeredInferenceClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpBrokeredInferenceClient")
            .field("base_url", &self.base_url)
            .field("client_instance_id", &self.client_instance_id)
            .finish_non_exhaustive()
    }
}

impl HttpBrokeredInferenceClient {
    pub fn new(
        base_url: impl Into<String>,
        access_token: RedactedString,
        client_instance_id: impl Into<String>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let client_instance_id = client_instance_id.into();
        let parsed = reqwest::Url::parse(&base_url)
            .map_err(|error| format!("invalid Cloud inference URL: {error}"))?;
        let loopback_http = parsed.scheme() == "http"
            && parsed
                .host_str()
                .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                .is_some_and(|address| address.is_loopback());
        if (parsed.scheme() != "https" && !loopback_http) || parsed.cannot_be_a_base() {
            return Err(
                "Cloud inference URL must use https (plain HTTP is limited to an IP loopback emulator)"
                    .into(),
            );
        }
        if client_instance_id.trim().is_empty() {
            return Err("Cloud client instance id must not be empty".into());
        }
        Ok(Self {
            http: reqwest::Client::new(),
            base_url,
            access_token,
            client_instance_id,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(self.access_token.expose_secret())
    }

    async fn classify(response: reqwest::Response) -> BrokeredInferenceError {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        #[derive(serde::Deserialize)]
        struct ErrorBody {
            error: Option<String>,
        }
        let status = response.status();
        let code = response
            .json::<ErrorBody>()
            .await
            .ok()
            .and_then(|body| body.error)
            .unwrap_or_default();
        match code.as_str() {
            "authentication_required" => BrokeredInferenceError::AuthenticationRequired,
            "account_selection_required" => BrokeredInferenceError::AccountSelectionRequired,
            "subscription_required" => BrokeredInferenceError::SubscriptionRequired,
            "model_not_entitled" => BrokeredInferenceError::ModelNotEntitled,
            "model_unavailable" => BrokeredInferenceError::ModelUnavailable,
            "insufficient_balance" => BrokeredInferenceError::InsufficientBalance,
            "quota_exceeded" => BrokeredInferenceError::QuotaExceeded { retry_after },
            "invalid_request" | "idempotency_key_required" => {
                BrokeredInferenceError::InvalidRequest
            }
            _ if status == reqwest::StatusCode::UNAUTHORIZED => {
                BrokeredInferenceError::AuthenticationRequired
            }
            _ if status == reqwest::StatusCode::FORBIDDEN => {
                BrokeredInferenceError::ModelNotEntitled
            }
            _ if status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                BrokeredInferenceError::QuotaExceeded { retry_after }
            }
            _ => BrokeredInferenceError::TemporarilyUnavailable,
        }
    }
}

#[derive(serde::Serialize)]
struct CreateGrantBody<'a> {
    client_instance_id: &'a str,
    local_run_correlation: Option<&'a str>,
    provider: &'a str,
    original_model_id: &'a str,
    native_protocol: &'a str,
}

#[derive(serde::Deserialize)]
struct GrantBody {
    grant_id: String,
    gateway_base_url: String,
    capability: String,
    grant_expires_at: i64,
}

#[async_trait]
impl BrokeredInferenceClient for HttpBrokeredInferenceClient {
    async fn create_grant(
        &self,
        request: BrokeredInferenceRequest,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
        let response = self
            .request(reqwest::Method::POST, "/v1/inference/grants")
            .header("Idempotency-Key", &request.idempotency_key)
            .json(&CreateGrantBody {
                client_instance_id: &self.client_instance_id,
                local_run_correlation: request.local_run_correlation.as_deref(),
                provider: &request.provider,
                original_model_id: &request.original_model_id,
                native_protocol: &request.native_protocol,
            })
            .send()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if !response.status().is_success() {
            return Err(Self::classify(response).await);
        }
        let body = response
            .json::<GrantBody>()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if body.grant_id.trim().is_empty()
            || body.gateway_base_url.trim().is_empty()
            || body.capability.trim().is_empty()
        {
            return Err(BrokeredInferenceError::TemporarilyUnavailable);
        }
        Ok(BrokeredInferenceLease {
            grant_id: body.grant_id,
            gateway_base_url: body.gateway_base_url,
            capability: RedactedString::new(body.capability),
            grant_expires_at: body.grant_expires_at,
        })
    }

    async fn close_grant(&self, grant_id: &str) -> Result<(), BrokeredInferenceError> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/inference/grants/{grant_id}/close"),
            )
            .send()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::classify(response).await)
        }
    }

    async fn renew_grant(
        &self,
        grant_id: &str,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/inference/grants/{grant_id}/renew"),
            )
            .send()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if !response.status().is_success() {
            return Err(Self::classify(response).await);
        }
        let body = response
            .json::<GrantBody>()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if body.grant_id.trim().is_empty()
            || body.gateway_base_url.trim().is_empty()
            || body.capability.trim().is_empty()
        {
            return Err(BrokeredInferenceError::TemporarilyUnavailable);
        }
        Ok(BrokeredInferenceLease {
            grant_id: body.grant_id,
            gateway_base_url: body.gateway_base_url,
            capability: RedactedString::new(body.capability),
            grant_expires_at: body.grant_expires_at,
        })
    }
}

#[async_trait]
impl BrokeredModelCatalogClient for HttpBrokeredInferenceClient {
    async fn readiness(&self) -> Result<(), BrokeredInferenceError> {
        let response = self
            .request(reqwest::Method::GET, "/v1/inference/readiness")
            .send()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::classify(response).await)
        }
    }

    async fn list_models(&self) -> Result<Vec<BrokeredModel>, BrokeredInferenceError> {
        #[derive(serde::Deserialize)]
        struct Page {
            data: Vec<BrokeredModel>,
        }
        let response = self
            .request(reqwest::Method::GET, "/v1/inference/models")
            .send()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if !response.status().is_success() {
            return Err(Self::classify(response).await);
        }
        response
            .json::<Page>()
            .await
            .map(|page| page.data)
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)
    }
}

#[async_trait]
impl awaken_admin_config_api::BrokeredCatalogDiscovery for HttpBrokeredInferenceClient {
    async fn projection(&self) -> Result<awaken_model_catalog::BrokeredCatalogProjection, String> {
        let models = fetch_brokered_catalog(self)
            .await
            .map_err(|error| error.to_string())?;
        let models = models
            .into_iter()
            .map(|model| {
                let dialect = match model.native_protocol.as_str() {
                    "anthropic_messages" => awaken_model_catalog::ApiDialect::AnthropicMessages,
                    "openai_chat_completions" => awaken_model_catalog::ApiDialect::OpenAiChat,
                    "openai_responses" => awaken_model_catalog::ApiDialect::OpenAiResponses,
                    protocol => {
                        return Err(format!(
                            "Cloud returned unsupported native protocol `{protocol}`"
                        ));
                    }
                };
                Ok(awaken_model_catalog::BrokeredModelProjection {
                    provider_id: model.provider,
                    model_id: model.original_model_id,
                    dialect,
                    context_window: model.context_window,
                    max_output_tokens: model.max_output_tokens,
                    publication_revision: model.route_publication_revision,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let observed_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        Ok(awaken_model_catalog::BrokeredCatalogProjection {
            broker_id: "awaken-cloud".into(),
            control_base_url: self.base_url.clone(),
            models,
            observed_at_unix_ms,
        })
    }
}

pub(crate) struct BrokeredCandidateExecutor {
    client: Arc<dyn BrokeredInferenceClient>,
    provider: String,
    model: String,
    api_dialect: String,
    adapter_kind: String,
    native_protocol: String,
    local_run_correlation: Option<String>,
    ownership: Option<Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>>,
    sequence: AtomicU64,
}

impl BrokeredCandidateExecutor {
    pub(crate) fn new(
        client: Arc<dyn BrokeredInferenceClient>,
        provider_ref: &str,
        model: &str,
        api_dialect: &str,
        adapter_kind: &str,
        local_run_correlation: Option<String>,
        ownership: Option<Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>>,
    ) -> Result<Self, String> {
        let provider = provider_ref
            .split_once('@')
            .map_or(provider_ref, |(provider, _)| provider)
            .trim();
        let native_protocol = match api_dialect {
            "anthropic_messages" => "anthropic_messages",
            "open_ai_chat" => "openai_chat_completions",
            "open_ai_responses" => "openai_responses",
            _ => return Err(format!("unsupported brokered API dialect `{api_dialect}`")),
        };
        if provider.is_empty() || model.trim().is_empty() {
            return Err("brokered publication has an incomplete public target".into());
        }
        Ok(Self {
            client,
            provider: provider.into(),
            model: model.into(),
            api_dialect: api_dialect.into(),
            adapter_kind: adapter_kind.into(),
            native_protocol: native_protocol.into(),
            local_run_correlation,
            ownership,
            sequence: AtomicU64::new(0),
        })
    }

    async fn lease(&self) -> Result<BrokeredInferenceLease, LlmError> {
        self.ownership
            .as_ref()
            .ok_or_else(|| {
                LlmError::Binding("brokered inference has no attempt ownership fence".into())
            })?
            .verify_current()
            .await
            .map_err(|error| LlmError::Binding(error.to_string()))?;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let idempotency_fingerprint = awaken_runtime_contract::content_fingerprint(&(
            self.local_run_correlation.as_deref().unwrap_or("run"),
            &self.provider,
            &self.model,
            sequence,
        ))
        .map_err(|error| LlmError::Binding(error.to_string()))?;
        self.client
            .create_grant(BrokeredInferenceRequest {
                provider: self.provider.clone(),
                original_model_id: self.model.clone(),
                native_protocol: self.native_protocol.clone(),
                local_run_correlation: self.local_run_correlation.clone(),
                idempotency_key: format!("awaken-{idempotency_fingerprint}"),
            })
            .await
            .map_err(BrokeredInferenceError::into_llm)
    }

    fn executor(&self, lease: &BrokeredInferenceLease) -> Result<Arc<dyn LlmExecutor>, LlmError> {
        executor_from_materialized_endpoint(
            &self.api_dialect,
            &self.adapter_kind,
            Some(&lease.gateway_base_url),
            Some(&lease.capability),
        )
        .map_err(|error| LlmError::Binding(error.to_string()))
    }

    async fn close(&self, lease: &BrokeredInferenceLease) {
        let _ = self.client.close_grant(&lease.grant_id).await;
    }
}

#[async_trait]
impl LlmExecutor for BrokeredCandidateExecutor {
    async fn infer(&self, mut request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let lease = self.lease().await?;
        request.model_binding.model_ref.clone_from(&self.model);
        let result = match self.executor(&lease) {
            Ok(executor) => executor.infer(request).await,
            Err(error) => Err(error),
        };
        self.close(&lease).await;
        result
    }

    async fn infer_streaming(
        &self,
        mut request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        let lease = self.lease().await?;
        request.model_binding.model_ref.clone_from(&self.model);
        let result = match self.executor(&lease) {
            Ok(executor) => executor.infer_streaming(request, sink).await,
            Err(error) => Err(error),
        };
        self.close(&lease).await;
        result
    }
}

#[cfg(test)]
mod tests {
    //! Cause graph for one brokered candidate attempt:
    //! C1 candidate protocol is supported; C2 current attempt ownership exists
    //! and verifies; C3 broker grants the exact public target; C4 native request
    //! finishes. E1 execute with returned Gateway/capability; E2 issue a distinct
    //! idempotency key per Provider request; E3 close the grant; E4 fail before
    //! Cloud I/O or Provider I/O.
    //!
    //! Decision table:
    //! | Rule | C1 | C2 | C3 | C4 | Effect |
    //! | T1   | Y  | Y  | Y  | Y  | E1+E2+E3 |
    //! | T2   | N  | -  | -  | -  | E4 |
    //! | T3   | Y  | N  | -  | -  | E4, zero grant calls |
    //! | T4   | Y  | Y  | N  | -  | typed LLM failure |

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingClient {
        requests: Mutex<Vec<BrokeredInferenceRequest>>,
    }

    #[async_trait]
    impl BrokeredInferenceClient for RecordingClient {
        async fn create_grant(
            &self,
            request: BrokeredInferenceRequest,
        ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
            self.requests.lock().unwrap().push(request);
            Ok(BrokeredInferenceLease {
                grant_id: "grant-1".into(),
                gateway_base_url: "https://gateway.invalid/v1".into(),
                capability: RedactedString::new("lease-token"),
                grant_expires_at: 1_800_000_000,
            })
        }

        async fn close_grant(&self, _grant_id: &str) -> Result<(), BrokeredInferenceError> {
            Ok(())
        }

        async fn renew_grant(
            &self,
            _grant_id: &str,
        ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
            unreachable!("renew is not used by a single-request executor")
        }
    }

    struct CurrentOwnership;

    struct ScriptedCatalogClient {
        readiness: Mutex<VecDeque<Result<(), BrokeredInferenceError>>>,
        models: Mutex<VecDeque<Result<Vec<BrokeredModel>, BrokeredInferenceError>>>,
    }

    #[async_trait]
    impl BrokeredModelCatalogClient for ScriptedCatalogClient {
        async fn readiness(&self) -> Result<(), BrokeredInferenceError> {
            self.readiness
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted readiness result")
        }

        async fn list_models(&self) -> Result<Vec<BrokeredModel>, BrokeredInferenceError> {
            self.models
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted models result")
        }
    }

    #[async_trait]
    impl awaken_runtime_contract::AttemptOwnershipVerifier for CurrentOwnership {
        async fn verify_current(
            &self,
        ) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
            Ok(())
        }
    }

    fn executor(
        client: Arc<RecordingClient>,
        ownership: Option<Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>>,
    ) -> BrokeredCandidateExecutor {
        BrokeredCandidateExecutor::new(
            client,
            "openai@3",
            "gpt-5",
            "open_ai_responses",
            "openai",
            Some("run-7".into()),
            ownership,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn t1_t2_exact_request_and_unique_idempotency_are_derived_locally() {
        let client = Arc::new(RecordingClient::default());
        let executor = executor(client.clone(), Some(Arc::new(CurrentOwnership)));

        executor.lease().await.unwrap();
        executor.lease().await.unwrap();

        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].provider, "openai");
        assert_eq!(requests[0].original_model_id, "gpt-5");
        assert_eq!(requests[0].native_protocol, "openai_responses");
        assert_eq!(requests[0].local_run_correlation.as_deref(), Some("run-7"));
        assert_ne!(requests[0].idempotency_key, requests[1].idempotency_key);
    }

    #[tokio::test]
    async fn t3_missing_attempt_ownership_prevents_a_grant_call() {
        let client = Arc::new(RecordingClient::default());
        let error = executor(client.clone(), None).lease().await.unwrap_err();

        assert_eq!(error.code(), "binding_rejected");
        assert!(client.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn t2_unsupported_protocol_fails_before_executor_creation() {
        let client = Arc::new(RecordingClient::default());
        assert!(
            BrokeredCandidateExecutor::new(
                client,
                "gemini@1",
                "gemini-pro",
                "gemini",
                "gemini",
                None,
                Some(Arc::new(CurrentOwnership)),
            )
            .is_err()
        );
    }

    #[test]
    fn t4_broker_failures_keep_stable_runtime_categories() {
        assert_eq!(
            BrokeredInferenceError::AuthenticationRequired
                .into_llm()
                .code(),
            "login_required"
        );
        assert_eq!(
            BrokeredInferenceError::SubscriptionRequired
                .into_llm()
                .code(),
            "unauthorized"
        );
        assert_eq!(
            BrokeredInferenceError::QuotaExceeded { retry_after: None }
                .into_llm()
                .code(),
            "rate_limited"
        );
        assert_eq!(
            BrokeredInferenceError::TemporarilyUnavailable
                .into_llm()
                .code(),
            "provider_error"
        );
    }

    #[test]
    fn cloud_http_client_requires_https_and_never_debugs_the_bearer() {
        assert!(
            HttpBrokeredInferenceClient::new(
                "http://api.awakenworks.com",
                RedactedString::new("secret-token"),
                "client-1",
            )
            .is_err()
        );
        let client = HttpBrokeredInferenceClient::new(
            "https://api.awakenworks.com/",
            RedactedString::new("secret-token"),
            "client-1",
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("https://api.awakenworks.com"));
        assert!(
            HttpBrokeredInferenceClient::new(
                "http://127.0.0.1:8080",
                RedactedString::new("local-emulator-token"),
                "client-1",
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn catalog_retry_decision_table_is_bounded_and_fail_closed() {
        // Cause graph: C1=readiness result, C2=models result, C3=error is
        // temporary. Effects: E1=return snapshot, E2=retry the whole read,
        // E3=return the stable error immediately.
        //
        // | Rule | C1        | C2        | C3 | Effect |
        // | R1   | ok        | ok        | -  | E1     |
        // | R2   | temporary | -         | Y  | E2     |
        // | R3   | ok        | temporary | Y  | E2     |
        // | R4   | stable    | -         | N  | E3     |
        // | R5   | temporary on all three attempts | - | Y | E3 |
        let model = BrokeredModel {
            provider: "openai".into(),
            original_model_id: "gpt-5".into(),
            native_protocol: "openai_responses".into(),
            context_window: Some(400_000),
            max_output_tokens: Some(128_000),
            capabilities: Default::default(),
            route_publication_revision: 7,
        };
        let recovers = ScriptedCatalogClient {
            readiness: Mutex::new(VecDeque::from([
                Err(BrokeredInferenceError::TemporarilyUnavailable),
                Ok(()),
                Ok(()),
            ])),
            models: Mutex::new(VecDeque::from([
                Err(BrokeredInferenceError::TemporarilyUnavailable),
                Ok(vec![model.clone()]),
            ])),
        };
        assert_eq!(
            fetch_brokered_catalog(&recovers).await.unwrap(),
            vec![model]
        );
        assert!(recovers.readiness.lock().unwrap().is_empty());
        assert!(recovers.models.lock().unwrap().is_empty());

        let stable = ScriptedCatalogClient {
            readiness: Mutex::new(VecDeque::from([Err(
                BrokeredInferenceError::AuthenticationRequired,
            )])),
            models: Mutex::new(VecDeque::new()),
        };
        assert_eq!(
            fetch_brokered_catalog(&stable).await.unwrap_err(),
            BrokeredInferenceError::AuthenticationRequired
        );

        let exhausted = ScriptedCatalogClient {
            readiness: Mutex::new(VecDeque::from([
                Err(BrokeredInferenceError::TemporarilyUnavailable),
                Err(BrokeredInferenceError::TemporarilyUnavailable),
                Err(BrokeredInferenceError::TemporarilyUnavailable),
            ])),
            models: Mutex::new(VecDeque::new()),
        };
        assert_eq!(
            fetch_brokered_catalog(&exhausted).await.unwrap_err(),
            BrokeredInferenceError::TemporarilyUnavailable
        );
        assert!(exhausted.readiness.lock().unwrap().is_empty());
    }
}
