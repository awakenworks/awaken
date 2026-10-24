// @formal evidence model_binding.constructor_and_wire_share_one_admission
// @formal evidence model_operation.ordinal_never_wraps_or_reuses_a_coordinate
//! Portable client-side seam for exact, short-lived managed inference access.
//!
//! The Cloud implementation owns identity, entitlement, pricing, quota, route
//! selection, Provider credential custody, usage and charging. This module owns
//! only the local consumer contract: request one exact public candidate, use the
//! returned native Gateway endpoint/capability, then close that grant. It never
//! receives or persists Cloud-internal target coordinates.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};
use awaken_runtime_contract::{InferenceEndpoint, UnspecifiedReasoning};

use crate::executor_from_materialized_endpoint_for_provider;

mod catalog;

pub const BROKERED_INFERENCE_ACCESS_CAPABILITY: &str = "inference.brokered_grant";

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
    /// The broker declared this identity's quota spent (`quota_exceeded`).
    /// A hard exhaustion: `retry_after` is only when it resets.
    #[error("managed inference quota is exhausted")]
    QuotaExceeded {
        retry_after: Option<std::time::Duration>,
    },
    /// The broker throttled the request (a bare 429) without declaring its
    /// quota spent, so it may clear after `retry_after`. Kept apart from
    /// `QuotaExceeded` so an exhausted quota is never retried in place and a
    /// throttle still is.
    #[error("managed inference is rate limited")]
    RateLimited {
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
            // An exhausted quota cannot clear inside the retry loop.
            Self::QuotaExceeded { retry_after } => LlmError::UsageLimit {
                message: self.to_string(),
                reset_after: retry_after,
            },
            Self::RateLimited { retry_after } => LlmError::RateLimited {
                message: self.to_string(),
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

/// A Platform endpoint that never answers must not pin a brokered call open.
const BROKERED_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The Platform inference plane is Awaken's own control traffic, and its
/// `http://127.0.0.1` emulator form is not reachable through an ambient egress
/// proxy; a routable Platform endpoint keeps a deliberately configured one.
fn brokered_http_client(base_url: &str) -> reqwest::Client {
    let mut builder = awaken_outbound_http::rustls_client_builder(
        awaken_outbound_http::TransportDeadline::total(BROKERED_REQUEST_TIMEOUT),
    );
    if awaken_outbound_http::target_is_loopback(base_url) {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .expect("static brokered inference client configuration is valid")
}

const BROKERED_CATALOG_MAX_ATTEMPTS: usize = 3;
const BROKERED_CATALOG_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// Read one authoritative Cloud catalog snapshot. Both operations are GETs, so a
/// transient transport/5xx failure can be retried without duplicating grants,
/// usage, or billing side effects. Stable identity/entitlement failures remain
/// fail-closed on the first attempt. Consumer: Control's catalog projection
/// adapter, which alone maps these neutral Cloud rows into its catalog types.
pub async fn fetch_brokered_catalog(
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
    workspace_id: String,
    client_instance_id: String,
}

impl std::fmt::Debug for HttpBrokeredInferenceClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpBrokeredInferenceClient")
            .field("base_url", &self.base_url)
            .field("workspace_id", &self.workspace_id)
            .field("client_instance_id", &self.client_instance_id)
            .finish_non_exhaustive()
    }
}

impl HttpBrokeredInferenceClient {
    /// The validated Cloud endpoint used by Control's catalog projection.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn new(
        base_url: impl Into<String>,
        access_token_source: Arc<awaken_agent_contract::RedactedStringSource>,
        workspace_id: impl Into<String>,
        client_instance_id: impl Into<String>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let client_instance_id = client_instance_id.into();
        let workspace_id = workspace_id.into();
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
        if workspace_id.trim().is_empty() {
            return Err("Cloud Workspace id must not be empty".into());
        }
        resolve_access_token(access_token_source.as_ref())?;
        Ok(Self {
            http: brokered_http_client(&base_url),
            base_url,
            access_token_source,
            workspace_id,
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
                BrokeredInferenceError::RateLimited { retry_after }
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
    workspace_id: &'a str,
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
    workspace_id: &'a str,
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
        context: &awaken_tool_contract::tool::ToolOperationContext,
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
                        workspace_id: &self.workspace_id,
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
        | BrokeredInferenceError::RateLimited { .. }
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
                        workspace_id: &self.workspace_id,
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

/// One brokered candidate as an [`LlmExecutor`]: each request leases an
/// exact grant, runs on the returned Gateway endpoint and closes the grant.
pub struct BrokeredCandidateExecutor {
    client: Arc<dyn BrokeredInferenceClient>,
    provider: String,
    model: String,
    api_dialect: String,
    adapter_kind: String,
    unspecified_reasoning: UnspecifiedReasoning,
    native_protocol: String,
    local_run_correlation: Option<String>,
    ownership: Option<Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>>,
}

impl BrokeredCandidateExecutor {
    pub fn new(
        client: Arc<dyn BrokeredInferenceClient>,
        provider_ref: &str,
        model: &str,
        endpoint: &InferenceEndpoint,
        unspecified_reasoning: UnspecifiedReasoning,
        local_run_correlation: Option<String>,
        ownership: Option<Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>>,
    ) -> Result<Self, String> {
        // Brokered supply asks its Cloud client for a provider routing label,
        // not a Vault source authorization coordinate. The catalog may give
        // either a versioned or bare label; direct credential materialization
        // instead uses candidate_provider_expectation and requires both exact
        // versioned Provider and endpoint pins before any Vault material read.
        let provider = provider_ref
            .split_once('@')
            .map_or(provider_ref, |(provider, _)| provider)
            .trim();
        let native_protocol = match endpoint.api_dialect.as_str() {
            "anthropic_messages" => "anthropic_messages",
            "open_ai_chat" => "openai_chat_completions",
            "open_ai_responses" => "openai_responses",
            _ => {
                return Err(format!(
                    "unsupported brokered API dialect `{}`",
                    endpoint.api_dialect
                ));
            }
        };
        if provider.is_empty() || model.trim().is_empty() {
            return Err("brokered publication has an incomplete public target".into());
        }
        Ok(Self {
            client,
            provider: provider.into(),
            model: model.into(),
            api_dialect: endpoint.api_dialect.clone(),
            adapter_kind: endpoint.adapter_kind.clone(),
            unspecified_reasoning,
            native_protocol: native_protocol.into(),
            local_run_correlation,
            ownership,
        })
    }

    async fn lease(
        &self,
        binding: &awaken_runtime_contract::resolved::ModelBinding,
    ) -> Result<BrokeredInferenceLease, LlmError> {
        let operation = awaken_runtime_contract::model_operation::current_model_operation_context()
            .filter(|operation| {
                self.local_run_correlation.as_deref() == Some(operation.run_id().0.as_str())
                    && operation.binding() == binding
            })
            .ok_or_else(|| {
                LlmError::Binding(
                    "brokered inference requires the exact model operation context".into(),
                )
            })?;
        self.verify_ownership().await?;
        let idempotency_fingerprint = awaken_agent_contract::content_fingerprint(&(
            operation.operation_id(),
            &self.provider,
            &self.model,
            &self.native_protocol,
        ))
        .map_err(|error| LlmError::Binding(error.to_string()))?;
        let lease = self
            .client
            .create_grant(BrokeredInferenceRequest {
                provider: self.provider.clone(),
                original_model_id: self.model.clone(),
                native_protocol: self.native_protocol.clone(),
                local_run_correlation: self.local_run_correlation.clone(),
                idempotency_key: format!("awaken-{idempotency_fingerprint}"),
            })
            .await
            .map_err(BrokeredInferenceError::into_llm)?;
        // Grant issuance is asynchronous control I/O, not an execution permit.
        // A current pre-grant observation cannot authorize the later native
        // send if authority was lost while waiting for the grant response.
        if let Err(error) = self.verify_ownership().await {
            self.close(&lease).await;
            return Err(error);
        }
        Ok(lease)
    }

    async fn verify_ownership(&self) -> Result<(), LlmError> {
        let ownership = self.ownership.as_deref().ok_or_else(|| {
            LlmError::Binding("brokered inference has no attempt ownership fence".into())
        })?;
        awaken_runtime_contract::execution::verify_attempt_ownership(Some(ownership))
            .await
            .map_err(|error| LlmError::Binding(error.to_string()))
    }

    fn executor(&self, lease: &BrokeredInferenceLease) -> Result<Arc<dyn LlmExecutor>, LlmError> {
        executor_from_materialized_endpoint_for_provider(
            &self.provider,
            &self.api_dialect,
            &self.adapter_kind,
            Some(&lease.gateway_base_url),
            Some(&lease.capability),
            self.unspecified_reasoning,
        )
        .map_err(|error| LlmError::Binding(error.to_string()))
    }

    async fn close(&self, lease: &BrokeredInferenceLease) {
        let _ = self.client.close_grant(&lease.grant_id).await;
    }

    async fn execute(
        &self,
        mut request: ChatRequest,
        sink: Option<&dyn DeltaSink>,
    ) -> Result<ChatResponse, LlmError> {
        // Validate the wire-only copy before acquiring a grant. Authorization
        // still names the original publication binding, and a projection error
        // creates neither a remote model request nor a grant needing closure.
        let upstream_binding = request.project_upstream_model(&self.model)?;
        let lease = self.lease(&request.model_binding).await.map_err(|error| {
            // The egress lease could not be acquired, so nothing was sent.
            error.with_execution_evidence(
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(
                    awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution(
                    ),
                ),
            )
        })?;
        request.model_binding = upstream_binding;
        let result = match self.executor(&lease) {
            Ok(executor) => match sink {
                Some(sink) => executor.infer_streaming(request, sink).await,
                None => executor.infer(request).await,
            },
            // No executor could be built for the lease: never entered.
            Err(error) => Err(error.with_execution_evidence(
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(
                    awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution(
                    ),
                ),
            )),
        };
        self.close(&lease).await;
        result
    }
}

#[async_trait]
impl LlmExecutor for BrokeredCandidateExecutor {
    fn effect_scope(
        &self,
        request: &ChatRequest,
    ) -> awaken_runtime_contract::model_operation::ModelEffectScope {
        awaken_provider_genai::provider_model_effect_scope(
            &self.provider,
            &self.api_dialect,
            &self.adapter_kind,
            request,
        )
    }

    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.execute(request, None).await
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        self.execute(request, Some(sink)).await
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

    fn bearer_test_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut bearers = Vec::new();
            for (status, body) in responses {
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

                let reason = if status == 401 { "Unauthorized" } else { "OK" };
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
        grant_gate: Option<Arc<tokio::sync::Barrier>>,
        gateway_base_url: Option<String>,
        closed: Mutex<Vec<String>>,
        fail_close: bool,
    }

    mod effect_scope;
    mod sent_effect_unknown;

    #[tokio::test]
    async fn brokered_catalog_fetch_reuses_the_authenticated_cloud_client() {
        // Causes: C1 readiness and models are successful authenticated GETs;
        // C2 the same Cloud catalog is read again. Effects: R1 and R2 retain
        // the exact model and route revision and each request carries the
        // client's bearer. Control owns timestamp and dialect projection.
        const MODELS: &str = r#"{"data":[{"provider":"awaken-cloud","original_model_id":"model-a","native_protocol":"openai_responses","route_publication_revision":7}]}"#;
        let (base_url, server) =
            bearer_test_server(vec![(200, "{}"), (200, MODELS), (200, "{}"), (200, MODELS)]);
        let client = HttpBrokeredInferenceClient::new(
            base_url,
            static_access_token("catalog-access"),
            "workspace-a",
            "desktop-a",
        )
        .unwrap();
        let first = fetch_brokered_catalog(&client).await.unwrap();
        assert_eq!(first.len(), 1, "R1 exact catalog");
        let second = fetch_brokered_catalog(&client).await.unwrap();
        assert_eq!(second, first, "R2 model authority unchanged");
        assert_eq!(second[0].route_publication_revision, 7, "R2 route revision");
        assert_eq!(server.join().unwrap(), vec!["catalog-access"; 4]);
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
            "workspace-tools",
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
            &awaken_tool_contract::tool::ToolOperationContext::for_run("run-7", "op-7"),
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
            requests[1].contains("\"workspace_id\":\"workspace-tools\""),
            "W2"
        );
        assert!(
            requests[1]
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Bearer cloud-access")),
            "W2: HTTP field names are case-insensitive: {}",
            requests[1]
        );
    }

    #[async_trait]
    impl BrokeredInferenceClient for RecordingClient {
        async fn create_grant(
            &self,
            request: BrokeredInferenceRequest,
        ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
            self.requests.lock().unwrap().push(request);
            if let Some(gate) = &self.grant_gate {
                gate.wait().await;
                gate.wait().await;
            }
            Ok(BrokeredInferenceLease {
                grant_id: "grant-1".into(),
                gateway_base_url: self
                    .gateway_base_url
                    .clone()
                    .unwrap_or_else(|| "https://gateway.invalid/v1".into()),
                capability: RedactedString::new("lease-token"),
                grant_expires_at: 1_800_000_000,
            })
        }

        async fn close_grant(&self, grant_id: &str) -> Result<(), BrokeredInferenceError> {
            self.closed.lock().unwrap().push(grant_id.to_owned());
            if self.fail_close {
                Err(BrokeredInferenceError::TemporarilyUnavailable)
            } else {
                Ok(())
            }
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
            &wire_endpoint("open_ai_responses", "openai"),
            UnspecifiedReasoning::ProviderDefault,
            Some("run-7".into()),
            ownership,
        )
        .unwrap()
    }

    fn wire_endpoint(api_dialect: &str, adapter_kind: &str) -> InferenceEndpoint {
        InferenceEndpoint {
            adapter_kind: adapter_kind.into(),
            api_dialect: api_dialect.into(),
            base_url: "https://gateway.invalid".into(),
            upstream_model: "gpt-5".into(),
            processing_placement: None,
        }
    }

    #[test]
    fn brokered_provider_label_accepts_bare_or_versioned_catalog_names() {
        // Causes: B1 Cloud catalog sends a versioned provider label; B2 it
        // sends a bare label. Effect E1 both select the same brokered client
        // provider without opening local credential material. Rules
        // R1=B1=>E1; R2=B2=>E1. The direct Vault path's required endpoint
        // coordinate is deliberately a different, stricter authority check.
        for label in ["openai@3", "openai"] {
            let executor = BrokeredCandidateExecutor::new(
                Arc::new(RecordingClient::default()),
                label,
                "gpt-5",
                &wire_endpoint("open_ai_responses", "openai"),
                UnspecifiedReasoning::ProviderDefault,
                None,
                None,
            )
            .expect("R1-R2 brokered label");
            assert_eq!(executor.provider, "openai", "R1-R2/E1 {label}");
        }
    }

    fn model_operation(
        ordinal: u64,
    ) -> awaken_runtime_contract::model_operation::ModelOperationContext {
        use awaken_runtime_contract::model_operation::{ModelOperation, ModelOperationContext};
        let mut record = ModelOperation::next(
            None,
            awaken_runtime_contract::resolved::ModelBinding::new("openai@3", "gpt-5", "native"),
        )
        .unwrap();
        record.ordinal = std::num::NonZeroU64::new(ordinal).unwrap();
        ModelOperationContext::from_operation(
            &awaken_agent_contract::agent::thread::Id("thread-7".into()),
            &awaken_agent_contract::agent::run::Id("run-7".into()),
            &record,
        )
    }

    async fn lease_operation(
        executor: &BrokeredCandidateExecutor,
        ordinal: u64,
    ) -> Result<BrokeredInferenceLease, LlmError> {
        let operation = model_operation(ordinal);
        let binding = operation.binding().clone();
        awaken_runtime_contract::model_operation::with_model_operation_context(
            Some(operation),
            executor.lease(&binding),
        )
        .await
    }

    #[tokio::test]
    async fn invalid_wire_model_projection_never_acquires_a_grant() {
        // B1 current exact operation/ownership + C1 empty/padded pinned wire
        // model + C2 streaming/nonstreaming => Binding with RequestProjection
        // proof, zero create_grant, zero close_grant, original request intact.
        // The existing valid wire/grant tests below own B2 (admitted copy ->
        // authorized original binding -> one grant -> one exact close). A local
        // projection failure must not leak a grant or relabel it as a send.
        use awaken_runtime_contract::model_operation::{
            ModelExecutionEvidence, NoDispatchProof, with_model_operation_context,
        };
        for alias in ["", " gpt-5", "gpt-5 "] {
            for streaming in [false, true] {
                let client = Arc::new(RecordingClient::default());
                let mut model = executor(client.clone(), Some(Arc::new(CurrentOwnership)));
                model.model = alias.into(); // Inject a malformed endpoint pin, not a binding.
                let operation = model_operation(7);
                let request = ChatRequest {
                    model_binding: operation.binding().clone(),
                    inference: Default::default(),
                    messages: vec![],
                    tools: vec![],
                };
                let original = request.clone();
                let error = with_model_operation_context(Some(operation), async {
                    if streaming {
                        model
                            .infer_streaming(request.clone(), &crate::NoopDeltaSink)
                            .await
                    } else {
                        model.infer(request.clone()).await
                    }
                })
                .await
                .unwrap_err();
                assert!(matches!(error.classification(), LlmError::Binding(_)), "B1");
                assert_eq!(
                    error.execution_evidence(),
                    ModelExecutionEvidence::NotDispatched(NoDispatchProof::request_projection(),),
                    "B1 exact pre-effect stage"
                );
                assert!(client.requests.lock().unwrap().is_empty(), "B1 zero grants");
                assert!(
                    client.closed.lock().unwrap().is_empty(),
                    "B1 no resource to close"
                );
                assert_eq!(request, original, "B1 original authorization input");
            }
        }
    }

    #[tokio::test]
    async fn rematerialized_candidates_must_not_reuse_another_invocations_grant_key() {
        // G1 the pinned wrapper materializes a fresh candidate per invocation;
        // G2 both invocations belong to the same Run and target. Distinct durable
        // model invocations must request distinct grants despite G1+G2. A local
        // counter reset must not impersonate a retry of the preceding operation.
        let client = Arc::new(RecordingClient::default());
        for ordinal in 1..=2 {
            lease_operation(
                &executor(client.clone(), Some(Arc::new(CurrentOwnership))),
                ordinal,
            )
            .await
            .unwrap();
        }
        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_ne!(requests[0].idempotency_key, requests[1].idempotency_key);
    }

    #[tokio::test]
    async fn t1_t2_exact_request_and_unique_idempotency_are_derived_locally() {
        let client = Arc::new(RecordingClient::default());
        let executor = executor(client.clone(), Some(Arc::new(CurrentOwnership)));

        lease_operation(&executor, 1).await.unwrap();
        lease_operation(&executor, 2).await.unwrap();

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
        let error = lease_operation(&executor(client.clone(), None), 1)
            .await
            .unwrap_err();

        assert_eq!(error.code(), "binding_rejected");
        assert!(client.requests.lock().unwrap().is_empty());
        // T3 at both model-port entries: missing authority => no model send,
        // hence NotDispatched (the broker control operation is a separate effect).
        let request = ChatRequest {
            model_binding: awaken_runtime_contract::resolved::ModelBinding::new("p", "m", "b"),
            inference: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let model = executor(client.clone(), None);
        for streaming in [false, true] {
            let error = if streaming {
                model
                    .infer_streaming(request.clone(), &crate::NoopDeltaSink)
                    .await
            } else {
                model.infer(request.clone()).await
            }
            .unwrap_err();
            assert_eq!(error.code(), "binding_rejected");
            assert_eq!(
                error.execution_evidence(),
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(
                    awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution(
                    )
                )
            );
        }
        assert!(client.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ownership_change_during_grant_wait_cannot_start_native_inference() {
        // Cause graph: C1 authority is current before grant I/O; C2 changes
        // while the grant is blocked; C3 grant close succeeds/fails; C4 either
        // model port. C1 -> grant acquired; C2 -> fresh ownership observation
        // denies model send; C3 cannot replace that failure or upgrade evidence.
        // Decision table (each row x streaming/nonstreaming):
        // | rule | after grant | close | effects |
        // | F1 | Lost | success | zero native sends, exact close, NotDispatched |
        // | F2 | Unavailable | success | same, no fallback |
        // | F3 | Lost | failure | same, original binding error retained |
        // | F4 | Unavailable | failure | same |
        // | F5 | Current | success | one native send, exact close, complete HTTP error => Terminated |
        // F5's upstream returns a complete 400 response body. That exact remote
        // invocation has ended; TCP close alone would remain Unknown and is
        // covered by the provider transport-failure matrix.
        // The barrier fixes the crash cut without timing sleeps. This tests a
        // fresh observation, not atomic fencing of an arbitrary remote server.
        use awaken_runtime_contract::model_operation::{
            ModelExecutionEvidence, ResponseEnvelope, TerminalReceipt, with_model_operation_context,
        };
        use awaken_runtime_contract::{AttemptOwnershipError, AttemptOwnershipVerifier};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

        struct MutableOwnership(Mutex<Result<(), AttemptOwnershipError>>);
        #[async_trait]
        impl AttemptOwnershipVerifier for MutableOwnership {
            async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
                self.0.lock().unwrap().clone()
            }
        }

        for (rule, after_grant, fail_close) in [
            ("F1", Err(AttemptOwnershipError::Lost), false),
            (
                "F2",
                Err(AttemptOwnershipError::Unavailable("offline".into())),
                false,
            ),
            ("F3", Err(AttemptOwnershipError::Lost), true),
            (
                "F4",
                Err(AttemptOwnershipError::Unavailable("offline".into())),
                true,
            ),
            ("F5", Ok(()), false),
        ] {
            for streaming in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
                let sends = Arc::new(AtomicUsize::new(0));
                let observed = sends.clone();
                let server = tokio::spawn(async move {
                    loop {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        // Read the full request before closing the response.
                        // Dropping a socket with unread input can reset TCP on
                        // Windows, which correctly yields Unknown evidence.
                        {
                            let mut reader = tokio::io::BufReader::new(&mut socket);
                            let mut line = String::new();
                            let mut content_length = None;
                            loop {
                                line.clear();
                                assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                                if line == "\r\n" {
                                    break;
                                }
                                if let Some((name, value)) = line.split_once(':')
                                    && name.eq_ignore_ascii_case("content-length")
                                {
                                    content_length = Some(value.trim().parse::<u64>().unwrap());
                                }
                            }
                            let length =
                                content_length.expect("native JSON request has a body length");
                            assert_eq!(
                                tokio::io::copy(&mut reader.take(length), &mut tokio::io::sink())
                                    .await
                                    .unwrap(),
                                length,
                                "native request body must be complete"
                            );
                        }
                        observed.fetch_add(1, Ordering::SeqCst);
                        socket.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await.unwrap();
                        socket.shutdown().await.unwrap();
                    }
                });
                let gate = Arc::new(tokio::sync::Barrier::new(2));
                let client = Arc::new(RecordingClient {
                    grant_gate: Some(gate.clone()),
                    gateway_base_url: Some(endpoint),
                    fail_close,
                    ..Default::default()
                });
                let ownership = Arc::new(MutableOwnership(Mutex::new(Ok(()))));
                let model = executor(client.clone(), Some(ownership.clone()));
                let operation = model_operation(1);
                let request = ChatRequest {
                    model_binding: operation.binding().clone(),
                    inference: Default::default(),
                    messages: Vec::new(),
                    tools: Vec::new(),
                };
                let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::join!(
                        with_model_operation_context(Some(operation), async {
                            if streaming {
                                model.infer_streaming(request, &crate::NoopDeltaSink).await
                            } else {
                                model.infer(request).await
                            }
                        }),
                        async {
                            gate.wait().await;
                            *ownership.0.lock().unwrap() = after_grant.clone();
                            gate.wait().await;
                        }
                    )
                    .0
                })
                .await;
                server.abort();
                let _ = server.await;
                let error = outcome.expect(rule).unwrap_err();
                if after_grant.is_err() {
                    assert_eq!(
                        sends.load(Ordering::SeqCst),
                        0,
                        "{rule}: stale native entry"
                    );
                    assert_eq!(error.code(), "binding_rejected", "{rule}");
                    assert_eq!(
                        error.execution_evidence(),
                        ModelExecutionEvidence::NotDispatched(awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution()),
                        "{rule}"
                    );
                } else {
                    assert_eq!(sends.load(Ordering::SeqCst), 1, "{rule}: healthy path");
                    assert_eq!(
                        error.execution_evidence(),
                        ModelExecutionEvidence::Terminated(
                            TerminalReceipt::from_response_envelope(
                                ResponseEnvelope::CompleteErrorBody,
                            )
                        ),
                        "{rule}: a complete HTTP error body proves that invocation terminated"
                    );
                }
                assert_eq!(client.requests.lock().unwrap().len(), 1, "{rule}");
                assert_eq!(*client.closed.lock().unwrap(), ["grant-1"], "{rule}");
            }
        }
    }

    #[tokio::test]
    async fn grant_retry_identity_survives_reconstruction_and_rejects_foreign_context() {
        use awaken_runtime_contract::model_operation::with_model_operation_context;
        // G3 same durable invocation through new adapter instances => same key,
        // including a lost grant response retry. G4 ordinal advances => distinct
        // (adjacent reproduction). G5 absent context, foreign Run or binding =>
        // zero broker IO. Ownership is necessary but cannot supply missing identity.
        let client = Arc::new(RecordingClient::default());
        for _ in 0..2 {
            lease_operation(
                &executor(client.clone(), Some(Arc::new(CurrentOwnership))),
                7,
            )
            .await
            .unwrap();
        }
        {
            let requests = client.requests.lock().unwrap();
            assert_eq!(requests[0].idempotency_key, requests[1].idempotency_key);
        }
        let model = executor(client.clone(), Some(Arc::new(CurrentOwnership)));
        let operation = model_operation(7);
        let binding = operation.binding().clone();
        assert!(model.lease(&binding).await.is_err());
        let (provider, _, backend) = binding.identity();
        let foreign_binding = awaken_runtime_contract::resolved::ModelBinding::parse(
            provider,
            "foreign-model",
            backend,
        )
        .unwrap();
        assert!(
            with_model_operation_context(Some(operation.clone()), model.lease(&foreign_binding))
                .await
                .is_err()
        );
        let mut foreign_run = executor(client.clone(), Some(Arc::new(CurrentOwnership)));
        foreign_run.local_run_correlation = Some("foreign-run".into());
        assert!(
            with_model_operation_context(Some(operation), foreign_run.lease(&binding))
                .await
                .is_err()
        );
        assert_eq!(client.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn t2_unsupported_protocol_fails_before_executor_creation() {
        let client = Arc::new(RecordingClient::default());
        assert!(
            BrokeredCandidateExecutor::new(
                client,
                "gemini@1",
                "gemini-pro",
                &wire_endpoint("gemini", "gemini"),
                UnspecifiedReasoning::ProviderDefault,
                None,
                Some(Arc::new(CurrentOwnership)),
            )
            .is_err()
        );
    }

    #[test]
    fn t4_broker_failures_keep_stable_runtime_categories() {
        use awaken_runtime_contract::model_operation::{
            ModelEffectScope, ModelExecutionEvidence, ModelOperationPhase, ModelReentry,
            NoDispatchProof,
        };
        use awaken_runtime_contract::resilience::{
            FailureClass, RetryPolicy, RetryStop, RetryVerdict,
        };
        // Cause: each broker refusal of grant creation (never dispatched, as
        // `lease` records). Effects: E1 the runtime code; E2 the failure
        // class; E3 the retry loop's verdict for the first failed attempt.
        // Regression row R6: an exhausted quota is a hard usage limit — not
        // retried in place — while a bare throttle (R7) still is.
        // Reentry uses the production projection with Opaque scope: a broker
        // refusal proves pre-dispatch, never a response-only adapter capability.
        let hour = std::time::Duration::from_secs(3_600);
        let reset = Some(std::time::Duration::from_secs(90));
        let not_retryable = RetryVerdict::Stop(RetryStop::NotRetryable);
        let rows = [
            (
                "R1",
                BrokeredInferenceError::AuthenticationRequired,
                "login_required",
                FailureClass::Permanent,
                Some(not_retryable),
            ),
            (
                "R2",
                BrokeredInferenceError::AccountSelectionRequired,
                "login_required",
                FailureClass::Permanent,
                Some(not_retryable),
            ),
            (
                "R3",
                BrokeredInferenceError::SubscriptionRequired,
                "unauthorized",
                FailureClass::Permanent,
                Some(not_retryable),
            ),
            (
                "R4",
                BrokeredInferenceError::ModelNotEntitled,
                "unauthorized",
                FailureClass::Permanent,
                Some(not_retryable),
            ),
            (
                "R5",
                BrokeredInferenceError::InsufficientBalance,
                "usage_limit",
                FailureClass::UsageLimit { reset_after: None },
                Some(not_retryable),
            ),
            (
                "R6",
                BrokeredInferenceError::QuotaExceeded { retry_after: reset },
                "usage_limit",
                FailureClass::UsageLimit { reset_after: reset },
                Some(not_retryable),
            ),
            (
                "R7",
                BrokeredInferenceError::RateLimited { retry_after: reset },
                "rate_limited",
                FailureClass::RateLimited { retry_after: reset },
                None,
            ),
            (
                "R8",
                BrokeredInferenceError::ModelUnavailable,
                "model_not_found",
                FailureClass::Permanent,
                Some(not_retryable),
            ),
            (
                "R9",
                BrokeredInferenceError::TemporarilyUnavailable,
                "provider_error",
                FailureClass::Transient,
                None,
            ),
            (
                "R10",
                BrokeredInferenceError::InvalidRequest,
                "binding_rejected",
                FailureClass::Permanent,
                Some(not_retryable),
            ),
        ];
        for (row, error, code, class, stop) in rows {
            let error =
                error
                    .into_llm()
                    .with_execution_evidence(ModelExecutionEvidence::NotDispatched(
                        NoDispatchProof::executor_resolution(),
                    ));
            assert_eq!(error.code(), code, "{row} E1");
            assert_eq!(error.class(), class, "{row} E2");
            let verdict = RetryPolicy::default().decide(
                error.class(),
                ModelReentry::observed(
                    ModelOperationPhase::Returned,
                    ModelEffectScope::Opaque,
                    error.execution_evidence(),
                ),
                0,
                hour,
                0,
            );
            match stop {
                Some(stop) => assert_eq!(verdict, stop, "{row} E3"),
                None => assert!(matches!(verdict, RetryVerdict::Retry { .. }), "{row} E3"),
            }
        }
    }

    #[tokio::test]
    async fn broker_429_decodes_an_exhausted_quota_apart_from_a_throttle() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Cause: the broker's 429 body code (quota_exceeded / none) and its
        // Retry-After. Effects: E1 the decoded refusal; E2 the reset/retry
        // hint survives. Rules: D1 quota_exceeded => QuotaExceeded (a hard
        // usage limit downstream); D2 a bare 429 => RateLimited (a throttle).
        async fn respond(body: &'static str) -> reqwest::Response {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 90\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let response = reqwest::get(format!("http://{address}/grant"))
                .await
                .unwrap();
            server.await.unwrap();
            response
        }
        let hint = Some(std::time::Duration::from_secs(90));
        for (row, body, expected) in [
            (
                "D1",
                r#"{"error":"quota_exceeded"}"#,
                BrokeredInferenceError::QuotaExceeded { retry_after: hint },
            ),
            (
                "D2",
                "{}",
                BrokeredInferenceError::RateLimited { retry_after: hint },
            ),
        ] {
            let decoded = HttpBrokeredInferenceClient::classify(respond(body).await).await;
            assert_eq!(decoded, expected, "{row}");
        }
    }

    #[test]
    fn cloud_http_client_requires_https_and_never_debugs_the_bearer() {
        assert!(
            HttpBrokeredInferenceClient::new(
                "http://api.awakenworks.com",
                static_access_token("secret-token"),
                "workspace-1",
                "client-1",
            )
            .is_err()
        );
        let client = HttpBrokeredInferenceClient::new(
            "https://api.awakenworks.com/",
            static_access_token("secret-token"),
            "workspace-1",
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
                "workspace-1",
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
        let (current_url, current_server) = bearer_test_server(vec![(200, "{}")]);
        let current = HttpBrokeredInferenceClient::new(
            current_url,
            static_access_token("current"),
            "workspace-1",
            "client-1",
        )
        .unwrap();
        let response = current
            .send_with_access_token(|token| current.request(reqwest::Method::GET, "/probe", token))
            .await
            .expect("A1");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "A1");
        assert_eq!(current_server.join().unwrap(), ["current"], "A1");

        let (rotating_url, rotating_server) = bearer_test_server(vec![
            (401, r#"{"error":"authentication_required"}"#),
            (200, "{}"),
        ]);
        let rotating = HttpBrokeredInferenceClient::new(
            rotating_url,
            scripted_access_tokens([Ok("old"), Ok("old"), Ok("new")]),
            "workspace-1",
            "client-1",
        )
        .unwrap();
        let response = rotating
            .send_with_access_token(|token| rotating.request(reqwest::Method::GET, "/probe", token))
            .await
            .expect("A2");
        assert_eq!(response.status(), reqwest::StatusCode::OK, "A2");
        assert_eq!(rotating_server.join().unwrap(), ["old", "new"], "A2");

        let (unchanged_url, unchanged_server) =
            bearer_test_server(vec![(401, r#"{"error":"authentication_required"}"#)]);
        let unchanged = HttpBrokeredInferenceClient::new(
            unchanged_url,
            static_access_token("same"),
            "workspace-1",
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
            "workspace-1",
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
