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

use crate::executor_from_materialized_endpoint_for_provider;

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
    access_token_source: Arc<awaken_agent_contract::RedactedStringSource>,
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
        access_token_source: Arc<awaken_agent_contract::RedactedStringSource>,
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
        resolve_access_token(access_token_source.as_ref())?;
        Ok(Self {
            http: reqwest::Client::new(),
            base_url,
            access_token_source,
            client_instance_id,
        })
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        access_token: &RedactedString,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(access_token.expose_secret())
    }

    /// Send with the current interactive credential. One authentication
    /// rejection may be retried only when the identity source reports a
    /// different successor; an unchanged rejected credential is terminal.
    async fn send_with_access_token<F>(
        &self,
        build: F,
    ) -> Result<reqwest::Response, BrokeredInferenceError>
    where
        F: Fn(&RedactedString) -> reqwest::RequestBuilder,
    {
        let attempted = resolve_access_token(self.access_token_source.as_ref())
            .map_err(|_| BrokeredInferenceError::AuthenticationRequired)?;
        let response = build(&attempted)
            .send()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let successor = resolve_access_token(self.access_token_source.as_ref())
            .map_err(|_| BrokeredInferenceError::AuthenticationRequired)?;
        if successor.expose_secret() == attempted.expose_secret() {
            return Ok(response);
        }
        build(&successor)
            .send()
            .await
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)
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

fn resolve_access_token(
    source: &awaken_agent_contract::RedactedStringSource,
) -> Result<RedactedString, String> {
    let token = source()?;
    if token.is_empty() {
        return Err("Cloud access token source returned an empty credential".into());
    }
    Ok(token)
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

#[derive(Debug, Clone, serde::Deserialize)]
struct BrokeredToolRoute {
    tool_id: String,
    provider_id: String,
    provider_label: String,
    route_ref: String,
    options_schema: serde_json::Value,
}

#[derive(serde::Serialize)]
struct CreateToolGrantBody<'a> {
    client_instance_id: &'a str,
    local_run_correlation: Option<&'a str>,
    operation_id: &'a str,
    tool_id: &'a str,
    route_ref: &'a str,
}

#[derive(serde::Deserialize)]
struct ToolGrantBody {
    tool_id: String,
    provider_id: String,
    route_ref: String,
    gateway_base_url: String,
    capability: String,
}

impl HttpBrokeredInferenceClient {
    async fn list_tool_routes(&self) -> Result<Vec<BrokeredToolRoute>, BrokeredInferenceError> {
        #[derive(serde::Deserialize)]
        struct Page {
            data: Vec<BrokeredToolRoute>,
        }
        let response = self
            .send_with_access_token(|token| {
                self.request(reqwest::Method::GET, "/v1/inference/tools", token)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::classify(response).await);
        }
        response
            .json::<Page>()
            .await
            .map(|page| page.data)
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)
    }

    /// Add only currently discoverable Cloud routes to the shared builtin
    /// provider catalog. Missing tool kinds remain absent rather than exposing
    /// a provider that cannot obtain a grant.
    pub async fn install_managed_web_routes(
        &self,
        registry: &mut awaken_ext_builtin_tools::WebSearchProviderRegistry,
    ) -> Result<usize, BrokeredInferenceError> {
        let routes = self.list_tool_routes().await?;
        let search = routes
            .iter()
            .filter(|route| {
                route.provider_id == awaken_ext_builtin_tools::AWAKEN_CLOUD_PROVIDER_ID
                    && route.tool_id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID
            })
            .cloned()
            .collect::<Vec<_>>();
        let fetch = routes
            .iter()
            .filter(|route| {
                route.provider_id == awaken_ext_builtin_tools::AWAKEN_CLOUD_PROVIDER_ID
                    && route.tool_id == awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID
            })
            .cloned()
            .collect::<Vec<_>>();
        if search.is_empty() && fetch.is_empty() {
            return Ok(0);
        }
        let search_schema = discovered_tool_options_schema(&search);
        let fetch_schema = discovered_tool_options_schema(&fetch);
        let provider = Arc::new(
            awaken_ext_builtin_tools::ManagedGatewayWebProvider::new(Arc::new(self.clone()))
                .with_descriptors(
                    "Awaken Cloud · Web Search",
                    search_schema,
                    search.iter().map(|route| route.route_ref.clone()),
                    "Awaken Cloud · Web Fetch",
                    fetch_schema,
                    fetch.iter().map(|route| route.route_ref.clone()),
                ),
        );
        if !search.is_empty() {
            registry
                .register(provider.clone())
                .map_err(|_| BrokeredInferenceError::InvalidRequest)?;
        }
        if !fetch.is_empty() {
            registry
                .register_fetch(provider)
                .map_err(|_| BrokeredInferenceError::InvalidRequest)?;
        }
        Ok(search.len() + fetch.len())
    }
}

fn discovered_tool_options_schema(routes: &[BrokeredToolRoute]) -> serde_json::Value {
    let variants = routes
        .iter()
        .map(|route| {
            let mut schema = route.options_schema.clone();
            if let Some(object) = schema.as_object_mut() {
                object.insert(
                    "title".into(),
                    serde_json::Value::String(route.provider_label.clone()),
                );
            }
            schema
        })
        .collect::<Vec<_>>();
    serde_json::json!({"oneOf": variants})
}

#[async_trait]
impl awaken_ext_builtin_tools::ManagedWebRouteResolver for HttpBrokeredInferenceClient {
    async fn resolve(
        &self,
        context: &awaken_runtime_contract::tool::ToolOperationContext,
        tool_id: &str,
        route_ref: &str,
    ) -> Result<
        awaken_ext_builtin_tools::ManagedWebGatewayEndpoint,
        awaken_ext_builtin_tools::ManagedWebRouteError,
    > {
        let run = context
            .run_id
            .as_ref()
            .ok_or(awaken_ext_builtin_tools::ManagedWebRouteError::Invalid)?;
        let response = self
            .send_with_access_token(|token| {
                self.request(reqwest::Method::POST, "/v1/inference/tools/grants", token)
                    .json(&CreateToolGrantBody {
                        client_instance_id: &self.client_instance_id,
                        local_run_correlation: Some(&run.0),
                        operation_id: &context.operation_id,
                        tool_id,
                        route_ref,
                    })
            })
            .await
            .map_err(map_managed_web_error)?;
        if !response.status().is_success() {
            return Err(map_managed_web_error(Self::classify(response).await));
        }
        let body = response
            .json::<ToolGrantBody>()
            .await
            .map_err(|_| awaken_ext_builtin_tools::ManagedWebRouteError::Unavailable)?;
        if body.tool_id != tool_id
            || body.provider_id != awaken_ext_builtin_tools::AWAKEN_CLOUD_PROVIDER_ID
            || body.route_ref != route_ref
            || body.gateway_base_url.trim().is_empty()
            || body.capability.trim().is_empty()
        {
            return Err(awaken_ext_builtin_tools::ManagedWebRouteError::Invalid);
        }
        Ok(awaken_ext_builtin_tools::ManagedWebGatewayEndpoint {
            gateway_base_url: body.gateway_base_url,
            route_ref: body.route_ref,
            lease_token: RedactedString::new(body.capability),
        })
    }
}

fn map_managed_web_error(
    error: BrokeredInferenceError,
) -> awaken_ext_builtin_tools::ManagedWebRouteError {
    match error {
        BrokeredInferenceError::AuthenticationRequired
        | BrokeredInferenceError::AccountSelectionRequired
        | BrokeredInferenceError::SubscriptionRequired
        | BrokeredInferenceError::ModelNotEntitled
        | BrokeredInferenceError::InsufficientBalance => {
            awaken_ext_builtin_tools::ManagedWebRouteError::Forbidden
        }
        BrokeredInferenceError::ModelUnavailable | BrokeredInferenceError::InvalidRequest => {
            awaken_ext_builtin_tools::ManagedWebRouteError::Invalid
        }
        BrokeredInferenceError::QuotaExceeded { .. }
        | BrokeredInferenceError::TemporarilyUnavailable => {
            awaken_ext_builtin_tools::ManagedWebRouteError::Unavailable
        }
    }
}

#[async_trait]
impl BrokeredInferenceClient for HttpBrokeredInferenceClient {
    async fn create_grant(
        &self,
        request: BrokeredInferenceRequest,
    ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
        let response = self
            .send_with_access_token(|token| {
                self.request(reqwest::Method::POST, "/v1/inference/grants", token)
                    .header("Idempotency-Key", &request.idempotency_key)
                    .json(&CreateGrantBody {
                        client_instance_id: &self.client_instance_id,
                        local_run_correlation: request.local_run_correlation.as_deref(),
                        provider: &request.provider,
                        original_model_id: &request.original_model_id,
                        native_protocol: &request.native_protocol,
                    })
            })
            .await?;
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
        let path = format!("/v1/inference/grants/{grant_id}/close");
        let response = self
            .send_with_access_token(|token| self.request(reqwest::Method::POST, &path, token))
            .await?;
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
        let path = format!("/v1/inference/grants/{grant_id}/renew");
        let response = self
            .send_with_access_token(|token| self.request(reqwest::Method::POST, &path, token))
            .await?;
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
            .send_with_access_token(|token| {
                self.request(reqwest::Method::GET, "/v1/inference/readiness", token)
            })
            .await?;
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
            .send_with_access_token(|token| {
                self.request(reqwest::Method::GET, "/v1/inference/models", token)
            })
            .await?;
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
        executor_from_materialized_endpoint_for_provider(
            &self.provider,
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

    fn static_access_token(value: &str) -> Arc<awaken_agent_contract::RedactedStringSource> {
        let value = value.to_owned();
        Arc::new(move || Ok(RedactedString::new(value.clone())))
    }

    fn scripted_access_tokens(
        values: impl IntoIterator<Item = Result<&'static str, &'static str>>,
    ) -> Arc<awaken_agent_contract::RedactedStringSource> {
        let values = Arc::new(Mutex::new(
            values
                .into_iter()
                .map(|value| value.map(str::to_owned).map_err(str::to_owned))
                .collect::<VecDeque<_>>(),
        ));
        Arc::new(move || {
            values
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("token script exhausted".into()))
                .map(RedactedString::new)
        })
    }

    fn bearer_test_server(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut bearers = Vec::new();
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                let bearer = request
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("authorization: Bearer ")
                            .or_else(|| line.strip_prefix("Authorization: Bearer "))
                    })
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                bearers.push(bearer);

                let (reason, body) = if status == 401 {
                    ("Unauthorized", r#"{"error":"authentication_required"}"#)
                } else {
                    ("OK", "{}")
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            bearers
        });
        (format!("http://{address}"), handle)
    }

    fn brokered_tool_test_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let responses = [
                serde_json::json!({"data":[
                    {
                        "tool_id":"web_search",
                        "provider_id":"awaken-cloud",
                        "provider_label":"Awaken Cloud · Brave",
                        "route_ref":"web-search:brave@7",
                        "funding":"platform",
                        "options_schema":{
                            "type":"object",
                            "properties":{"route_ref":{"type":"string","const":"web-search:brave@7"}},
                            "required":["route_ref"],
                            "additionalProperties":false
                        }
                    },
                    {
                        "tool_id":"web_fetch",
                        "provider_id":"awaken-cloud",
                        "provider_label":"Awaken Cloud · Reader",
                        "route_ref":"web-fetch:reader@2",
                        "funding":"platform",
                        "options_schema":{
                            "type":"object",
                            "properties":{"route_ref":{"type":"string","const":"web-fetch:reader@2"}},
                            "required":["route_ref"],
                            "additionalProperties":false
                        }
                    }
                ]})
                .to_string(),
                serde_json::json!({
                    "tool_id":"web_search",
                    "provider_id":"awaken-cloud",
                    "provider_label":"Awaken Cloud · Brave",
                    "route_ref":"web-search:brave@7",
                    "funding":"platform",
                    "gateway_base_url":"https://gateway.example",
                    "capability":"route-capability",
                    "capability_token_type":"Bearer",
                    "grant_expires_at":1800000000,
                    "local_run_correlation":"run-7",
                    "operation_id":"op-7"
                })
                .to_string(),
            ];
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 2048];
                let mut expected = None;
                loop {
                    let read = stream.read(&mut chunk).unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if expected.is_none()
                        && let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        expected = Some(end + 4 + length);
                    }
                    if expected.is_some_and(|size| bytes.len() >= size) {
                        break;
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    #[derive(Default)]
    struct RecordingClient {
        requests: Mutex<Vec<BrokeredInferenceRequest>>,
    }

    #[tokio::test]
    async fn brokered_web_discovery_and_resolution_share_the_cloud_client() {
        // Cause/effect graph: C1 authenticated tool discovery returns exact
        // search/fetch routes -> E1 both are installed in the existing registry
        // with discovery-derived schemas. C2 Runtime run+operation + selected
        // exact route -> E2 the same client requests a route grant and returns a
        // redacted Gateway endpoint. No Provider credential is represented.
        //
        // Decision table:
        // | Rule | discovery | operation context | effect |
        // | W1 | two valid Cloud routes | n/a | two provider capabilities |
        // | W2 | installed search route | run+operation | exact grant request |
        // Invalid route/provider responses are rejected by the exact equality
        // checks and Cloud endpoint tests own the denial combinations.
        let (base_url, server) = brokered_tool_test_server();
        let client = HttpBrokeredInferenceClient::new(
            base_url,
            static_access_token("cloud-access"),
            "desktop-tools",
        )
        .unwrap();
        let mut registry = awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins();
        assert_eq!(
            client
                .install_managed_web_routes(&mut registry)
                .await
                .unwrap(),
            2,
            "W1"
        );
        assert!(
            registry
                .config_schema()
                .to_string()
                .contains("web-search:brave@7"),
            "W1"
        );
        assert!(
            registry
                .fetch_config_schema()
                .to_string()
                .contains("web-fetch:reader@2"),
            "W1"
        );

        let endpoint = awaken_ext_builtin_tools::ManagedWebRouteResolver::resolve(
            &client,
            &awaken_runtime_contract::tool::ToolOperationContext::for_run("run-7", "op-7"),
            "web_search",
            "web-search:brave@7",
        )
        .await
        .unwrap();
        assert_eq!(endpoint.route_ref, "web-search:brave@7", "W2");
        assert_eq!(
            endpoint.lease_token.expose_secret(),
            "route-capability",
            "W2"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/inference/tools "), "W1");
        assert!(
            requests[1].contains("POST /v1/inference/tools/grants "),
            "W2"
        );
        assert!(requests[1].contains("\"operation_id\":\"op-7\""), "W2");
        assert!(
            requests[1].contains("authorization: Bearer cloud-access"),
            "W2"
        );
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
                static_access_token("secret-token"),
                "client-1",
            )
            .is_err()
        );
        let client = HttpBrokeredInferenceClient::new(
            "https://api.awakenworks.com/",
            static_access_token("secret-token"),
            "client-1",
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("https://api.awakenworks.com"));
        assert!(
            HttpBrokeredInferenceClient::new(
                "http://127.0.0.1:8080",
                static_access_token("local-emulator-token"),
                "client-1",
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn cloud_access_token_rotation_decision_table() {
        // Cause graph: C1=source resolves before I/O, C2=response is 401,
        // C3=successor differs. Effects: E1=send once, E2=retry once with the
        // successor, E3=return authentication failure without network fallback.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | A1   | Y  | N  | -  | E1     |
        // | A2   | Y  | Y  | Y  | E2     |
        // | A3   | Y  | Y  | N  | E1, terminal 401 |
        // | A4   | N  | -  | -  | E3, zero requests |
        let (current_url, current_server) = bearer_test_server(vec![200]);
        let current = HttpBrokeredInferenceClient::new(
            current_url,
            static_access_token("current"),
            "client-1",
        )
        .unwrap();
        let response = current
            .send_with_access_token(|token| current.request(reqwest::Method::GET, "/probe", token))
            .await
            .expect("A1");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "A1");
        assert_eq!(current_server.join().unwrap(), ["current"], "A1");

        let (rotating_url, rotating_server) = bearer_test_server(vec![401, 200]);
        let rotating = HttpBrokeredInferenceClient::new(
            rotating_url,
            scripted_access_tokens([Ok("old"), Ok("old"), Ok("new")]),
            "client-1",
        )
        .unwrap();
        let response = rotating
            .send_with_access_token(|token| rotating.request(reqwest::Method::GET, "/probe", token))
            .await
            .expect("A2");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "A2");
        assert_eq!(rotating_server.join().unwrap(), ["old", "new"], "A2");

        let (unchanged_url, unchanged_server) = bearer_test_server(vec![401]);
        let unchanged = HttpBrokeredInferenceClient::new(
            unchanged_url,
            static_access_token("same"),
            "client-1",
        )
        .unwrap();
        let response = unchanged
            .send_with_access_token(|token| {
                unchanged.request(reqwest::Method::GET, "/probe", token)
            })
            .await
            .expect("A3 returns the terminal HTTP response");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED, "A3");
        assert_eq!(unchanged_server.join().unwrap(), ["same"], "A3");

        let unavailable = HttpBrokeredInferenceClient::new(
            "http://127.0.0.1:9",
            scripted_access_tokens([Ok("initial"), Err("expired")]),
            "client-1",
        )
        .unwrap();
        assert_eq!(
            unavailable
                .send_with_access_token(|token| {
                    unavailable.request(reqwest::Method::GET, "/must-not-send", token)
                })
                .await
                .unwrap_err(),
            BrokeredInferenceError::AuthenticationRequired,
            "A4"
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
