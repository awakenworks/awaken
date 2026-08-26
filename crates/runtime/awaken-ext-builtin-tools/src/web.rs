//! Network tools. `web_search` has one provider registry shared by native and
//! mediated ACP execution; providers own HTTP details while the platform owns
//! configuration, exact credential resolution, and tool exposure.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, IdBound, Plugin, PluginConfigError, PluginManifest,
};
use awaken_runtime_contract::resolved::{
    OpenRouterWebFetchParameters, OpenRouterWebSearchParameters, ProviderServerTool, ToolDescriptor,
};
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError, ToolExecutionTarget};
use awaken_runtime_contract::{CredentialMaterial, CredentialRef, CredentialUsage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::erasure::erase_for;

mod configuration;
#[cfg(test)]
use configuration::WebExecutionConfiguration;
use configuration::url_matches_filter;
pub use configuration::{
    WebDomainFilter, WebFetchExecutionConfiguration, WebSearchExecutionConfiguration,
    WebSearchUserLocation, web_fetch_execution_configuration, web_search_execution_configuration,
};
mod managed;
#[allow(unused_imports)]
pub use managed::managed_web_route_ref;
pub use managed::{
    ManagedGatewayWebProvider, ManagedWebGatewayEndpoint, ManagedWebRouteError,
    ManagedWebRouteResolver,
};
mod configured_fetch;
mod providers;
pub use providers::AwakenDirectFetchProvider;
use providers::{
    ProviderServerWebFetchTool, openrouter_server_fetch_descriptor,
    openrouter_server_search_descriptor,
};

pub const WEB_SEARCH_PLUGIN_ID: &str = "web_search";
pub const WEB_SEARCH_TOOL_ID: &str = "web_search";
pub const WEB_FETCH_PLUGIN_ID: &str = "web_fetch";
pub const WEB_FETCH_TOOL_ID: &str = "web_fetch";
pub const DUCKDUCKGO_PROVIDER_ID: &str = "duckduckgo";
pub const BRAVE_PROVIDER_ID: &str = "brave";
pub const AWAKEN_DIRECT_PROVIDER_ID: &str = "awaken-direct";
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
pub const AWAKEN_CLOUD_PROVIDER_ID: &str = "awaken-cloud";

/// Cap on fetched bytes so a huge response cannot blow up the transcript.
const MAX_BODY: u64 = 1 << 20;

async fn blocking<F, T>(work: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| ToolError::Execution(format!("blocking task: {err}")))?
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebFetchArgs {
    /// URL to fetch.
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct WebFetchRequest {
    pub url: String,
    pub options: Value,
}

#[async_trait]
pub trait WebFetchProvider: Send + Sync {
    fn descriptor(&self) -> WebFetchProviderDescriptor;

    fn validate_options(&self, options: &Value) -> Result<(), String> {
        validate_object_options(options)
    }

    /// Whether this provider can preserve the configured domain filter across
    /// every HTTP redirect. Implementations returning `true` must either apply
    /// the same filter before each redirected request or reject redirects
    /// before opening the next connection.
    fn enforces_domain_filter(&self) -> bool {
        false
    }

    /// The configured plugin validates the initial URL before passing a filter.
    /// A supporting provider then checks every redirect or rejects redirects
    /// before the next connection; unsupported providers fail configuration.
    async fn fetch(
        &self,
        request: WebFetchRequest,
        credential: Option<&CredentialMaterial>,
        domain_filter: Option<&WebDomainFilter>,
    ) -> Result<String, ToolError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct WebFetchProviderDescriptor {
    pub id: String,
    pub label: String,
    pub credential: WebSearchCredentialRequirement,
    pub options_schema: Value,
}

/// One provider account/route target. Repeating a provider id with a different
/// exact credential pin is how one provider exposes multiple accounts without
/// duplicating provider capability metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebProviderTarget {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
    #[serde(default)]
    pub options: Value,
}

#[derive(Debug, Clone, PartialEq)]
struct WebServerToolProviderDescriptor {
    id: String,
    label: String,
    options_schema: Value,
}

/// How one provider authenticates. The provider receives already-resolved
/// material and never learns about Vault or repository APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSearchCredentialRequirement {
    None,
    /// Resolve an exact pin using the common credential application contract.
    /// This covers built-ins (header/query/basic/certificate/file) and the open
    /// `Extension` consumer shape without another WebSearch auth enum.
    Exact(CredentialUsage),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchProviderDescriptor {
    pub id: String,
    pub label: String,
    pub credential: WebSearchCredentialRequirement,
    pub options_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct WebSearchRequest {
    pub query: String,
    pub count: usize,
    pub options: Value,
    pub user_location: Option<WebSearchUserLocation>,
}

#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn descriptor(&self) -> WebSearchProviderDescriptor;

    /// Validate provider-owned options without opening credentials or making a
    /// network request.
    fn validate_options(&self, options: &Value) -> Result<(), String> {
        validate_object_options(options)
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        credential: Option<&CredentialMaterial>,
    ) -> Result<Vec<WebSearchResult>, ToolError>;
}

fn validate_object_options(options: &Value) -> Result<(), String> {
    if options.is_object() || options.is_null() {
        Ok(())
    } else {
        Err("provider options must be an object".into())
    }
}

/// Runtime-host adapter for exact credential material. Implementations must
/// validate Workspace, revision, usage, and plaintext boundary before returning.
#[async_trait]
pub trait WebSearchCredentialResolver: Send + Sync {
    async fn resolve(
        &self,
        credential: &CredentialRef,
        provider_id: &str,
        usage: &CredentialUsage,
    ) -> Result<CredentialMaterial, String>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum WebSearchRegistryError {
    InvalidDescriptor,
    DuplicateProvider(String),
}

impl std::fmt::Display for WebSearchRegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDescriptor => {
                formatter.write_str("web-search provider descriptor is invalid")
            }
            Self::DuplicateProvider(id) => {
                write!(
                    formatter,
                    "web-search provider `{id}` is already registered"
                )
            }
        }
    }
}

impl std::error::Error for WebSearchRegistryError {}

/// One authoritative source for provider discovery and dispatch.
#[derive(Clone, Default)]
pub struct WebSearchProviderRegistry {
    providers: BTreeMap<String, RegisteredWebSearchProvider>,
    fetch_providers: BTreeMap<String, RegisteredWebFetchProvider>,
    server_search_providers: BTreeMap<String, WebServerToolProviderDescriptor>,
    server_fetch_providers: BTreeMap<String, WebServerToolProviderDescriptor>,
}

#[derive(Clone)]
struct RegisteredWebSearchProvider {
    descriptor: WebSearchProviderDescriptor,
    provider: Arc<dyn WebSearchProvider>,
}

#[derive(Clone)]
struct RegisteredWebFetchProvider {
    descriptor: WebFetchProviderDescriptor,
    provider: Arc<dyn WebFetchProvider>,
}

fn validate_provider_descriptor(
    id: &str,
    label: &str,
    options_schema: &Value,
) -> Result<(), WebSearchRegistryError> {
    if id.trim().is_empty()
        || label.trim().is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        || !options_schema.is_object()
    {
        return Err(WebSearchRegistryError::InvalidDescriptor);
    }
    Ok(())
}

impl WebSearchProviderRegistry {
    pub fn try_new(
        providers: impl IntoIterator<Item = Arc<dyn WebSearchProvider>>,
    ) -> Result<Self, WebSearchRegistryError> {
        let mut registry = Self::default();
        for provider in providers {
            registry.register(provider)?;
        }
        Ok(registry)
    }

    pub fn builtins() -> Self {
        let mut catalog = Self::try_new([
            Arc::new(DuckDuckGoProvider) as Arc<dyn WebSearchProvider>,
            Arc::new(BraveSearchProvider) as Arc<dyn WebSearchProvider>,
        ])
        .expect("built-in web-search provider descriptors are unique and valid");
        catalog
            .register_fetch(Arc::new(AwakenDirectFetchProvider))
            .expect("built-in web-fetch provider descriptor is valid");
        catalog
            .register_server_search(openrouter_server_search_descriptor())
            .expect("OpenRouter search descriptor is valid");
        catalog
            .register_server_fetch(openrouter_server_fetch_descriptor())
            .expect("OpenRouter fetch descriptor is valid");
        catalog
    }

    /// Provider-server capabilities usable by hosted compositions without
    /// installing the open direct/BYOK HTTP providers in that process.
    pub fn server_builtins() -> Self {
        let mut catalog = Self::default();
        catalog
            .register_server_search(openrouter_server_search_descriptor())
            .expect("OpenRouter search descriptor is valid");
        catalog
            .register_server_fetch(openrouter_server_fetch_descriptor())
            .expect("OpenRouter fetch descriptor is valid");
        catalog
    }

    pub fn register(
        &mut self,
        provider: Arc<dyn WebSearchProvider>,
    ) -> Result<(), WebSearchRegistryError> {
        let descriptor = provider.descriptor();
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        if self.providers.contains_key(&descriptor.id)
            || self.server_search_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.providers.insert(
            descriptor.id.clone(),
            RegisteredWebSearchProvider {
                descriptor,
                provider,
            },
        );
        Ok(())
    }

    pub fn register_fetch(
        &mut self,
        provider: Arc<dyn WebFetchProvider>,
    ) -> Result<(), WebSearchRegistryError> {
        let descriptor = provider.descriptor();
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        if self.fetch_providers.contains_key(&descriptor.id)
            || self.server_fetch_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.fetch_providers.insert(
            descriptor.id.clone(),
            RegisteredWebFetchProvider {
                descriptor,
                provider,
            },
        );
        Ok(())
    }

    fn register_server_search(
        &mut self,
        descriptor: WebServerToolProviderDescriptor,
    ) -> Result<(), WebSearchRegistryError> {
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        if self.providers.contains_key(&descriptor.id)
            || self.server_search_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.server_search_providers
            .insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    fn register_server_fetch(
        &mut self,
        descriptor: WebServerToolProviderDescriptor,
    ) -> Result<(), WebSearchRegistryError> {
        validate_provider_descriptor(
            &descriptor.id,
            &descriptor.label,
            &descriptor.options_schema,
        )?;
        if self.fetch_providers.contains_key(&descriptor.id)
            || self.server_fetch_providers.contains_key(&descriptor.id)
        {
            return Err(WebSearchRegistryError::DuplicateProvider(descriptor.id));
        }
        self.server_fetch_providers
            .insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    pub fn descriptors(&self) -> Vec<WebSearchProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor.clone())
            .collect()
    }

    fn provider(&self, id: &str) -> Option<RegisteredWebSearchProvider> {
        self.providers.get(id).cloned()
    }

    fn fetch_provider(&self, id: &str) -> Option<RegisteredWebFetchProvider> {
        self.fetch_providers.get(id).cloned()
    }

    /// JSON Schema is derived from the same descriptors dispatch uses. Each
    /// provider becomes one branch, so paid providers require an exact pin while
    /// free providers do not expose a meaningless credential field.
    pub fn config_schema(&self) -> Value {
        let mut descriptors = self.descriptors();
        descriptors.sort_by_key(|provider| {
            (
                !matches!(provider.credential, WebSearchCredentialRequirement::None),
                provider.id.clone(),
            )
        });
        let host_variants = descriptors
            .into_iter()
            .map(|provider| {
                host_target_schema(
                    &provider.id,
                    &provider.label,
                    &provider.credential,
                    provider.options_schema,
                )
            })
            .collect::<Vec<_>>();
        route_plan_schema(
            "Web search",
            host_variants,
            self.server_search_providers.values().cloned().collect(),
        )
    }

    pub fn fetch_config_schema(&self) -> Value {
        let mut descriptors = self
            .fetch_providers
            .values()
            .map(|provider| provider.descriptor.clone())
            .collect::<Vec<_>>();
        descriptors.sort_by_key(|provider| provider.id.clone());
        let host_variants = descriptors
            .into_iter()
            .map(|provider| {
                host_target_schema(
                    &provider.id,
                    &provider.label,
                    &provider.credential,
                    provider.options_schema,
                )
            })
            .collect();
        route_plan_schema(
            "Web fetch",
            host_variants,
            self.server_fetch_providers.values().cloned().collect(),
        )
    }
}

fn host_target_schema(
    id: &str,
    label: &str,
    credential: &WebSearchCredentialRequirement,
    options_schema: Value,
) -> Value {
    let mut properties = serde_json::Map::from_iter([
        (
            "provider_id".into(),
            json!({ "type": "string", "const": id, "title": label }),
        ),
        ("options".into(), options_schema),
    ]);
    let mut required = vec!["provider_id"];
    if let WebSearchCredentialRequirement::Exact(usage) = credential {
        properties.insert(
            "credential".into(),
            json!({
                "type": "object",
                "title": "Vault credential",
                "x-awaken-credential-application": usage,
                "properties": {
                    "id": { "type": "string", "title": "Credential source" },
                    "revision": { "type": "integer", "minimum": 1 },
                },
                "required": ["id", "revision"],
                "additionalProperties": false,
            }),
        );
        required.push("credential");
    }
    json!({
        "type": "object",
        "title": label,
        "x-awaken-realization": "host_executed",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn route_plan_schema(
    title: &str,
    host_variants: Vec<Value>,
    server_descriptors: Vec<WebServerToolProviderDescriptor>,
) -> Value {
    let fallback_items = json!({ "oneOf": host_variants });
    let mut variants = fallback_items["oneOf"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut branch| {
            if let Some(properties) = branch.get_mut("properties").and_then(Value::as_object_mut) {
                properties.insert(
                    "fallbacks".into(),
                    json!({
                        "type": "array",
                        "title": "Fallback accounts",
                        "items": fallback_items,
                        "default": [],
                    }),
                );
            }
            branch
        })
        .collect::<Vec<_>>();
    variants.extend(server_descriptors.into_iter().map(|provider| {
        json!({
            "type": "object",
            "title": provider.label,
            "x-awaken-realization": "provider_server",
            "properties": {
                "provider_id": { "type": "string", "const": provider.id },
                "options": provider.options_schema,
            },
            "required": ["provider_id"],
            "additionalProperties": false,
        })
    }));
    json!({ "title": title, "oneOf": variants })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebSearchConfig {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
    #[serde(default)]
    pub options: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<WebProviderTarget>,
}

pub type WebFetchConfig = WebSearchConfig;

impl WebSearchConfig {
    fn targets(&self) -> Vec<WebProviderTarget> {
        std::iter::once(WebProviderTarget {
            provider_id: self.provider_id.clone(),
            credential: self.credential.clone(),
            options: self.options.clone(),
        })
        .chain(self.fallbacks.iter().cloned())
        .collect()
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchArgs {
    /// Search query.
    pub query: String,
    /// Maximum number of results.
    #[serde(default)]
    #[schemars(range(min = 1, max = 20))]
    pub count: Option<usize>,
}

/// Configured model-callable tool. Native Runtime and ACP MCP export both use
/// this exact internal instance type through the configured plugin.
struct WebSearchTool {
    targets: Vec<ConfiguredSearchTarget>,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    execution_configuration: Option<WebSearchExecutionConfiguration>,
}

struct ConfiguredSearchTarget {
    provider: Arc<dyn WebSearchProvider>,
    descriptor: WebSearchProviderDescriptor,
    config: WebProviderTarget,
}

impl WebSearchTool {
    fn configured(
        targets: Vec<(RegisteredWebSearchProvider, WebProviderTarget)>,
        credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
        execution_configuration: Option<WebSearchExecutionConfiguration>,
    ) -> Self {
        Self {
            targets: targets
                .into_iter()
                .map(|(provider, config)| ConfiguredSearchTarget {
                    descriptor: provider.descriptor,
                    provider: provider.provider,
                    config,
                })
                .collect(),
            credentials,
            execution_configuration,
        }
    }
}

fn render_results(results: &[WebSearchResult]) -> String {
    if results.is_empty() {
        return "no web-search results".into();
    }
    results
        .iter()
        .map(|result| format!("- {}\n  {}\n  {}", result.title, result.url, result.snippet))
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for WebSearchTool {
    type Args = WebSearchArgs;
    type Output = String;
    const ID: &'static str = WEB_SEARCH_TOOL_ID;
    const DESCRIPTION: &'static str = "Search the web through the configured platform provider";

    async fn call(&self, args: WebSearchArgs) -> Result<String, ToolError> {
        let mut last_unavailable = None;
        for target in &self.targets {
            let credential = resolve_credential(
                &target.descriptor.credential,
                target.config.credential.as_ref(),
                self.credentials.as_ref(),
                &target.descriptor.id,
                "web-search",
            )
            .await?;
            match target
                .provider
                .search(
                    WebSearchRequest {
                        query: args.query.clone(),
                        count: args.count.unwrap_or(8).clamp(1, 20),
                        options: target.config.options.clone(),
                        user_location: self
                            .execution_configuration
                            .as_ref()
                            .and_then(|configuration| configuration.user_location.clone()),
                    },
                    credential.as_ref(),
                )
                .await
            {
                Ok(results) => {
                    let results = match self
                        .execution_configuration
                        .as_ref()
                        .and_then(|configuration| configuration.domains.as_ref())
                    {
                        Some(filter) => results
                            .into_iter()
                            .filter(|result| {
                                url::Url::parse(&result.url)
                                    .is_ok_and(|url| url_matches_filter(&url, filter))
                            })
                            .collect(),
                        None => results,
                    };
                    return Ok(render_results(&results));
                }
                Err(error @ ToolError::UnavailableBeforeDispatch(_)) => {
                    last_unavailable = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_unavailable
            .unwrap_or_else(|| ToolError::Execution("web-search route plan has no targets".into())))
    }
}

async fn resolve_credential(
    requirement: &WebSearchCredentialRequirement,
    reference: Option<&CredentialRef>,
    resolver: Option<&Arc<dyn WebSearchCredentialResolver>>,
    provider_id: &str,
    tool: &str,
) -> Result<Option<CredentialMaterial>, ToolError> {
    match requirement {
        WebSearchCredentialRequirement::None => Ok(None),
        WebSearchCredentialRequirement::Exact(usage) => {
            let reference = reference
                .ok_or_else(|| ToolError::Execution(format!("{tool} credential pin is missing")))?;
            resolver
                .ok_or_else(|| {
                    ToolError::Execution(format!("{tool} credential resolver is unavailable"))
                })?
                .resolve(reference, provider_id, usage)
                .await
                .map(Some)
                .map_err(ToolError::Execution)
        }
    }
}

#[derive(Clone)]
pub struct WebSearchPlugin {
    registry: WebSearchProviderRegistry,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    execution_configuration: Option<WebSearchExecutionConfiguration>,
}

enum ConfiguredWebRoute<T> {
    Host(Vec<T>),
    ProviderServer(ProviderServerTool),
}

type ConfiguredSearchRoute = ConfiguredWebRoute<(RegisteredWebSearchProvider, WebProviderTarget)>;
type ConfiguredFetchRoute = ConfiguredWebRoute<(RegisteredWebFetchProvider, WebProviderTarget)>;

impl WebSearchPlugin {
    pub fn new(
        registry: WebSearchProviderRegistry,
        credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    ) -> Self {
        Self {
            registry,
            credentials,
            execution_configuration: None,
        }
    }

    #[must_use]
    pub fn with_execution_configuration(
        mut self,
        configuration: Option<WebSearchExecutionConfiguration>,
    ) -> Self {
        self.execution_configuration = configuration;
        self
    }

    fn configured_provider(
        &self,
        config: Option<&Value>,
    ) -> Result<ConfiguredSearchRoute, PluginConfigError> {
        let config: WebSearchConfig =
            serde_json::from_value(config.cloned().ok_or_else(|| {
                PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, "config is required")
            })?)
            .map_err(|error| PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, error.to_string()))?;
        if self
            .registry
            .server_search_providers
            .contains_key(&config.provider_id)
        {
            if !config.fallbacks.is_empty() || config.credential.is_some() {
                return Err(PluginConfigError::new(
                    WEB_SEARCH_PLUGIN_ID,
                    "provider-server realization cannot mix host fallbacks or credentials",
                ));
            }
            if self
                .execution_configuration
                .as_ref()
                .is_some_and(|policy| policy.domains.is_some() || policy.user_location.is_some())
            {
                return Err(PluginConfigError::new(
                    WEB_SEARCH_PLUGIN_ID,
                    "provider-server realization cannot enforce the Agent WebSearch execution policy",
                ));
            }
            let parameters: OpenRouterWebSearchParameters = serde_json::from_value(config.options)
                .map_err(|error| PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, error.to_string()))?;
            return Ok(ConfiguredWebRoute::ProviderServer(
                ProviderServerTool::openrouter_web_search(parameters),
            ));
        }
        let mut targets = Vec::new();
        for target in config.targets() {
            let provider = self.registry.provider(&target.provider_id).ok_or_else(|| {
                PluginConfigError::new(
                    WEB_SEARCH_PLUGIN_ID,
                    format!("unknown host search provider `{}`", target.provider_id),
                )
            })?;
            validate_target(
                WEB_SEARCH_PLUGIN_ID,
                &provider.descriptor.credential,
                target.credential.as_ref(),
                &target.options,
                |options| provider.provider.validate_options(options),
            )?;
            targets.push((provider, target));
        }
        Ok(ConfiguredWebRoute::Host(targets))
    }

    /// Semantic validation used by both publication and runtime resolution.
    /// It parses through the same provider registry but performs no credential
    /// materialization and no network request.
    pub fn validate_config(&self, config: Option<&Value>) -> Result<(), PluginConfigError> {
        self.configured_provider(config).map(|_| ())
    }

    pub fn configured_tool(
        &self,
        config: Option<&Value>,
    ) -> Result<(ToolDescriptor, Arc<dyn RawTool>), PluginConfigError> {
        let route = self.configured_provider(config)?;
        let (descriptor, tool) = match route {
            ConfiguredWebRoute::Host(targets) => {
                if targets.iter().any(|(provider, _)| {
                    matches!(
                        provider.descriptor.credential,
                        WebSearchCredentialRequirement::Exact(_)
                    )
                }) && self.credentials.is_none()
                {
                    return Err(PluginConfigError::new(
                        WEB_SEARCH_PLUGIN_ID,
                        "the selected provider requires an installed credential materializer",
                    ));
                }
                (
                    web_search_descriptor(),
                    erase_for(
                        WebSearchTool::configured(
                            targets,
                            self.credentials.clone(),
                            self.execution_configuration.clone(),
                        ),
                        ToolExecutionTarget::Brain,
                    ),
                )
            }
            ConfiguredWebRoute::ProviderServer(projection) => (
                web_search_descriptor().with_provider_server_tool(projection),
                erase_for(ProviderServerWebSearchTool, ToolExecutionTarget::Brain),
            ),
        };
        Ok((descriptor, tool))
    }
}

fn validate_target(
    plugin_id: &str,
    requirement: &WebSearchCredentialRequirement,
    credential: Option<&CredentialRef>,
    options: &Value,
    validate_options: impl FnOnce(&Value) -> Result<(), String>,
) -> Result<(), PluginConfigError> {
    validate_options(options).map_err(|error| PluginConfigError::new(plugin_id, error))?;
    match (requirement, credential) {
        (WebSearchCredentialRequirement::None, None)
        | (WebSearchCredentialRequirement::Exact(_), Some(_)) => Ok(()),
        (WebSearchCredentialRequirement::None, Some(_)) => Err(PluginConfigError::new(
            plugin_id,
            "the selected provider does not consume a credential",
        )),
        (WebSearchCredentialRequirement::Exact(_), None) => Err(PluginConfigError::new(
            plugin_id,
            "the selected provider requires an exact credential pin",
        )),
    }
}

struct ProviderServerWebSearchTool;

#[async_trait]
impl Tool for ProviderServerWebSearchTool {
    type Args = WebSearchArgs;
    type Output = String;
    const ID: &'static str = WEB_SEARCH_TOOL_ID;
    const DESCRIPTION: &'static str = "Search the web through the selected model provider";

    async fn call(&self, _args: WebSearchArgs) -> Result<String, ToolError> {
        Err(ToolError::Execution(
            "provider-server WebSearch reached host execution".into(),
        ))
    }
}

impl Plugin for WebSearchPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: WEB_SEARCH_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: vec![WEB_SEARCH_PLUGIN_ID.into()],
            bound: CapabilityBound {
                tools: IdBound::Exact(vec![WEB_SEARCH_TOOL_ID.into()]),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        Contributions::new(WEB_SEARCH_PLUGIN_ID)
    }

    fn resolve_configured(
        &self,
        config: Option<&Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let (descriptor, tool) = self.configured_tool(config)?;
        let mut contributions = Contributions::new(WEB_SEARCH_PLUGIN_ID);
        contributions.register_dynamic_tool(
            awaken_runtime_contract::plugin::DynamicTool::try_new(descriptor, tool)
                .map_err(|error| PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, error.to_string()))?,
        );
        Ok(contributions)
    }
}

pub fn web_search_descriptor() -> ToolDescriptor {
    ToolDescriptor::for_tool::<WebSearchTool>("builtin")
}

pub fn web_fetch_descriptor() -> ToolDescriptor {
    ToolDescriptor::for_tool::<RoutedWebFetchTool>("builtin")
}

#[derive(Clone)]
pub struct WebFetchPlugin {
    registry: WebSearchProviderRegistry,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    execution_configuration: Option<WebFetchExecutionConfiguration>,
}

impl WebFetchPlugin {
    pub fn new(
        registry: WebSearchProviderRegistry,
        credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    ) -> Self {
        Self {
            registry,
            credentials,
            execution_configuration: None,
        }
    }

    #[must_use]
    pub fn with_execution_configuration(
        mut self,
        configuration: Option<WebFetchExecutionConfiguration>,
    ) -> Self {
        self.execution_configuration = configuration;
        self
    }

    pub fn default_config() -> Value {
        json!({ "provider_id": AWAKEN_DIRECT_PROVIDER_ID, "options": {} })
    }

    fn configured_route(
        &self,
        config: Option<&Value>,
    ) -> Result<ConfiguredFetchRoute, PluginConfigError> {
        let value = config.cloned().unwrap_or_else(Self::default_config);
        let config: WebFetchConfig = serde_json::from_value(value)
            .map_err(|error| PluginConfigError::new(WEB_FETCH_PLUGIN_ID, error.to_string()))?;
        if self
            .registry
            .server_fetch_providers
            .contains_key(&config.provider_id)
        {
            if !config.fallbacks.is_empty() || config.credential.is_some() {
                return Err(PluginConfigError::new(
                    WEB_FETCH_PLUGIN_ID,
                    "provider-server realization cannot mix host fallbacks or credentials",
                ));
            }
            if self.execution_configuration.as_ref().is_some_and(|policy| {
                policy.domains.is_some() || policy.max_content_tokens.is_some()
            }) {
                return Err(PluginConfigError::new(
                    WEB_FETCH_PLUGIN_ID,
                    "provider-server realization cannot enforce the Agent WebFetch execution policy",
                ));
            }
            let parameters: OpenRouterWebFetchParameters = serde_json::from_value(config.options)
                .map_err(|error| {
                PluginConfigError::new(WEB_FETCH_PLUGIN_ID, error.to_string())
            })?;
            return Ok(ConfiguredWebRoute::ProviderServer(
                ProviderServerTool::openrouter_web_fetch(parameters),
            ));
        }
        let mut targets = Vec::new();
        for target in config.targets() {
            let provider = self
                .registry
                .fetch_provider(&target.provider_id)
                .ok_or_else(|| {
                    PluginConfigError::new(
                        WEB_FETCH_PLUGIN_ID,
                        format!("unknown host fetch provider `{}`", target.provider_id),
                    )
                })?;
            if self
                .execution_configuration
                .as_ref()
                .and_then(|configuration| configuration.domains.as_ref())
                .is_some()
                && !provider.provider.enforces_domain_filter()
            {
                return Err(PluginConfigError::new(
                    WEB_FETCH_PLUGIN_ID,
                    format!(
                        "host fetch provider `{}` cannot enforce the Agent WebFetch domain policy",
                        target.provider_id
                    ),
                ));
            }
            validate_target(
                WEB_FETCH_PLUGIN_ID,
                &provider.descriptor.credential,
                target.credential.as_ref(),
                &target.options,
                |options| provider.provider.validate_options(options),
            )?;
            targets.push((provider, target));
        }
        Ok(ConfiguredWebRoute::Host(targets))
    }

    pub fn validate_config(&self, config: Option<&Value>) -> Result<(), PluginConfigError> {
        self.configured_route(config).map(|_| ())
    }

    pub fn configured_tool(
        &self,
        config: Option<&Value>,
    ) -> Result<(ToolDescriptor, Arc<dyn RawTool>), PluginConfigError> {
        match self.configured_route(config)? {
            ConfiguredWebRoute::Host(targets) => {
                if targets.iter().any(|(provider, _)| {
                    matches!(
                        provider.descriptor.credential,
                        WebSearchCredentialRequirement::Exact(_)
                    )
                }) && self.credentials.is_none()
                {
                    return Err(PluginConfigError::new(
                        WEB_FETCH_PLUGIN_ID,
                        "the selected provider requires an installed credential materializer",
                    ));
                }
                let targets = targets
                    .into_iter()
                    .map(|(provider, config)| ConfiguredFetchTarget {
                        descriptor: provider.descriptor,
                        provider: provider.provider,
                        config,
                    })
                    .collect();
                Ok((
                    web_fetch_descriptor(),
                    erase_for(
                        RoutedWebFetchTool {
                            targets,
                            credentials: self.credentials.clone(),
                            execution_configuration: self.execution_configuration.clone(),
                        },
                        ToolExecutionTarget::Brain,
                    ),
                ))
            }
            ConfiguredWebRoute::ProviderServer(projection) => Ok((
                web_fetch_descriptor().with_provider_server_tool(projection),
                erase_for(ProviderServerWebFetchTool, ToolExecutionTarget::Brain),
            )),
        }
    }
}

impl Plugin for WebFetchPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: WEB_FETCH_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: vec![WEB_FETCH_PLUGIN_ID.into()],
            bound: CapabilityBound {
                tools: IdBound::Exact(vec![WEB_FETCH_TOOL_ID.into()]),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        Contributions::new(WEB_FETCH_PLUGIN_ID)
    }

    fn resolve_configured(
        &self,
        config: Option<&Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let (descriptor, tool) = self.configured_tool(config)?;
        let mut contributions = Contributions::new(WEB_FETCH_PLUGIN_ID);
        contributions.register_dynamic_tool(
            awaken_runtime_contract::plugin::DynamicTool::try_new(descriptor, tool)
                .map_err(|error| PluginConfigError::new(WEB_FETCH_PLUGIN_ID, error.to_string()))?,
        );
        Ok(contributions)
    }
}

struct ConfiguredFetchTarget {
    provider: Arc<dyn WebFetchProvider>,
    descriptor: WebFetchProviderDescriptor,
    config: WebProviderTarget,
}

struct RoutedWebFetchTool {
    targets: Vec<ConfiguredFetchTarget>,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    execution_configuration: Option<WebFetchExecutionConfiguration>,
}

#[async_trait]
impl Tool for RoutedWebFetchTool {
    type Args = WebFetchArgs;
    type Output = String;
    const ID: &'static str = WEB_FETCH_TOOL_ID;
    const DESCRIPTION: &'static str = "Fetch a URL through the configured platform provider";

    async fn call(&self, args: WebFetchArgs) -> Result<String, ToolError> {
        let configured_url = self
            .execution_configuration
            .as_ref()
            .map(|configuration| {
                configured_fetch::configured_web_fetch_url(&args.url, configuration)
            })
            .transpose()?;
        let mut last_unavailable = None;
        for target in &self.targets {
            let credential = resolve_credential(
                &target.descriptor.credential,
                target.config.credential.as_ref(),
                self.credentials.as_ref(),
                &target.descriptor.id,
                "web-fetch",
            )
            .await?;
            let request = WebFetchRequest {
                url: args.url.clone(),
                options: target.config.options.clone(),
            };
            let domain_filter = self
                .execution_configuration
                .as_ref()
                .and_then(|configuration| configuration.domains.as_ref());
            let result = target
                .provider
                .fetch(request, credential.as_ref(), domain_filter)
                .await;
            match result {
                Ok(mut body) => {
                    if let (Some(url), Some(configuration)) = (
                        configured_url.as_ref(),
                        self.execution_configuration.as_ref(),
                    ) && let Some(max_bytes) =
                        configured_fetch::web_fetch_text_limit(url, configuration)
                    {
                        configured_fetch::truncate_text(&mut body, max_bytes);
                    }
                    return Ok(body);
                }
                Err(error @ ToolError::UnavailableBeforeDispatch(_)) => {
                    last_unavailable = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_unavailable
            .unwrap_or_else(|| ToolError::Execution("web-fetch route plan has no targets".into())))
    }
}

pub struct DuckDuckGoProvider;

#[derive(Deserialize, Default)]
struct DdgResponse {
    #[serde(default, rename = "Heading")]
    heading: String,
    #[serde(default, rename = "AbstractText")]
    abstract_text: String,
    #[serde(default, rename = "AbstractURL")]
    abstract_url: String,
    #[serde(default, rename = "RelatedTopics")]
    related_topics: Vec<DdgTopic>,
}

#[derive(Deserialize, Default)]
struct DdgTopic {
    #[serde(default, rename = "Text")]
    text: String,
    #[serde(default, rename = "FirstURL")]
    first_url: String,
}

fn ddg_results(response: DdgResponse, count: usize) -> Vec<WebSearchResult> {
    let mut results = Vec::new();
    if !response.abstract_text.is_empty() {
        results.push(WebSearchResult {
            title: response.heading,
            url: response.abstract_url,
            snippet: response.abstract_text,
        });
    }
    results.extend(
        response
            .related_topics
            .into_iter()
            .filter(|topic| !topic.text.is_empty())
            .map(|topic| WebSearchResult {
                title: topic.text.clone(),
                url: topic.first_url,
                snippet: topic.text,
            }),
    );
    results.truncate(count);
    results
}

#[async_trait]
impl WebSearchProvider for DuckDuckGoProvider {
    fn descriptor(&self) -> WebSearchProviderDescriptor {
        WebSearchProviderDescriptor {
            id: DUCKDUCKGO_PROVIDER_ID.into(),
            label: "DuckDuckGo (free)".into(),
            credential: WebSearchCredentialRequirement::None,
            options_schema: json!({ "type": "object", "additionalProperties": false }),
        }
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        _credential: Option<&CredentialMaterial>,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        blocking(move || {
            let response: DdgResponse = ureq::get("https://api.duckduckgo.com/")
                .query("q", &request.query)
                .query("format", "json")
                .query("no_html", "1")
                .query("no_redirect", "1")
                .call()
                .map_err(|err| ToolError::Execution(format!("DuckDuckGo search: {err}")))?
                .into_json()
                .map_err(|err| ToolError::Execution(format!("parse DuckDuckGo search: {err}")))?;
            Ok(ddg_results(response, request.count))
        })
        .await
    }
}

pub struct BraveSearchProvider;

#[derive(Deserialize, Default)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Deserialize, Default)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Deserialize, Default)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

#[async_trait]
impl WebSearchProvider for BraveSearchProvider {
    fn descriptor(&self) -> WebSearchProviderDescriptor {
        WebSearchProviderDescriptor {
            id: BRAVE_PROVIDER_ID.into(),
            label: "Brave Search API".into(),
            credential: WebSearchCredentialRequirement::Exact(CredentialUsage::HttpHeader {
                name: "X-Subscription-Token".into(),
                scheme: None,
            }),
            options_schema: json!({
                "type": "object",
                "properties": {
                    "country": { "type": "string" },
                    "search_lang": { "type": "string" },
                    "safesearch": { "type": "string", "enum": ["off", "moderate", "strict"] },
                },
                "additionalProperties": false,
            }),
        }
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        credential: Option<&CredentialMaterial>,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        let api_key = credential
            .ok_or_else(|| ToolError::Execution("Brave Search API key is missing".into()))?
            .single_secret()
            .map_err(|error| ToolError::Execution(error.to_string()))?
            .expose_secret()
            .to_string();
        blocking(move || {
            let mut call = ureq::get("https://api.search.brave.com/res/v1/web/search")
                .set("Accept", "application/json")
                .set("X-Subscription-Token", &api_key)
                .query("q", &request.query)
                .query("count", &request.count.to_string());
            if let Some(options) = request.options.as_object() {
                for key in ["search_lang", "safesearch"] {
                    if let Some(value) = options.get(key).and_then(Value::as_str) {
                        call = call.query(key, value);
                    }
                }
            }
            // Per-Run Agent configuration is more specific than the provider
            // publication's static default. Providers receive the complete
            // location; Brave can realize its country component directly.
            if let Some(country) = request
                .user_location
                .as_ref()
                .and_then(|location| location.country.as_deref())
                .or_else(|| {
                    request
                        .options
                        .get("country")
                        .and_then(serde_json::Value::as_str)
                })
            {
                call = call.query("country", country);
            }
            let response: BraveResponse = call
                .call()
                .map_err(|err| ToolError::Execution(format!("Brave search: {err}")))?
                .into_json()
                .map_err(|err| ToolError::Execution(format!("parse Brave search: {err}")))?;
            Ok(response
                .web
                .unwrap_or_default()
                .results
                .into_iter()
                .map(|result| WebSearchResult {
                    title: result.title,
                    url: result.url,
                    snippet: result.description,
                })
                .collect())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use awaken_runtime_contract::tool::ToolCall;

    use super::*;

    struct FakeProvider {
        descriptor: WebSearchProviderDescriptor,
        seen_secret: Mutex<Option<String>>,
        seen_request: Mutex<Option<WebSearchRequest>>,
    }

    #[async_trait]
    impl WebSearchProvider for FakeProvider {
        fn descriptor(&self) -> WebSearchProviderDescriptor {
            self.descriptor.clone()
        }

        async fn search(
            &self,
            request: WebSearchRequest,
            credential: Option<&CredentialMaterial>,
        ) -> Result<Vec<WebSearchResult>, ToolError> {
            *self.seen_request.lock().unwrap() = Some(request.clone());
            *self.seen_secret.lock().unwrap() = credential
                .and_then(|material| material.single_secret().ok())
                .map(|secret| secret.expose_secret().to_string());
            Ok(vec![WebSearchResult {
                title: request.query,
                url: "https://result.test".into(),
                snippet: "normalized".into(),
            }])
        }
    }

    struct FixedCredential;

    #[async_trait]
    impl WebSearchCredentialResolver for FixedCredential {
        async fn resolve(
            &self,
            _credential: &CredentialRef,
            _provider_id: &str,
            _usage: &CredentialUsage,
        ) -> Result<CredentialMaterial, String> {
            Err("paid credential resolution reached".into())
        }
    }

    struct UnavailableProvider;

    #[async_trait]
    impl WebSearchProvider for UnavailableProvider {
        fn descriptor(&self) -> WebSearchProviderDescriptor {
            WebSearchProviderDescriptor {
                id: "unavailable".into(),
                label: "Unavailable".into(),
                credential: WebSearchCredentialRequirement::None,
                options_schema: json!({ "type": "object" }),
            }
        }

        async fn search(
            &self,
            _request: WebSearchRequest,
            _credential: Option<&CredentialMaterial>,
        ) -> Result<Vec<WebSearchResult>, ToolError> {
            Err(ToolError::UnavailableBeforeDispatch(
                "provider admission unavailable".into(),
            ))
        }
    }

    fn fake_provider(id: &str, paid: bool) -> Arc<FakeProvider> {
        Arc::new(FakeProvider {
            descriptor: WebSearchProviderDescriptor {
                id: id.into(),
                label: id.into(),
                credential: if paid {
                    WebSearchCredentialRequirement::Exact(CredentialUsage::HttpHeader {
                        name: "X-Key".into(),
                        scheme: None,
                    })
                } else {
                    WebSearchCredentialRequirement::None
                },
                options_schema: json!({ "type": "object" }),
            },
            seen_secret: Mutex::new(None),
            seen_request: Mutex::new(None),
        })
    }

    struct FakeFetchProvider {
        seen_requests: Mutex<Vec<WebFetchRequest>>,
    }

    #[test]
    fn openrouter_server_configuration_is_typed_once_at_publication() {
        // Cause/effect table: S1/F1 known non-zero options -> the exact closed
        // provider projection; S2/F2 zero or unknown fields -> publication
        // rejection. No Runtime/provider layer receives an open options object.
        let registry = WebSearchProviderRegistry::server_builtins();
        let search = WebSearchPlugin::new(registry.clone(), None);
        let (descriptor, _) = search
            .configured_tool(Some(&json!({
                "provider_id": "openrouter",
                "options": {
                    "engine": "exa",
                    "max_results": 3,
                    "max_total_results": 9,
                    "search_context_size": "medium"
                }
            })))
            .expect("S1 typed search configuration");
        assert!(matches!(
            descriptor.provider_server_tool,
            Some(ProviderServerTool::OpenRouterWebSearch { .. })
        ));
        for invalid in [
            json!({"provider_id":"openrouter","options":{"max_results":0}}),
            json!({"provider_id":"openrouter","options":{"extra":true}}),
        ] {
            assert!(search.validate_config(Some(&invalid)).is_err(), "S2");
        }

        let fetch = WebFetchPlugin::new(registry, None);
        let (descriptor, _) = fetch
            .configured_tool(Some(&json!({
                "provider_id": "openrouter",
                "options": {"engine":"openrouter","max_content_tokens":2048}
            })))
            .expect("F1 typed fetch configuration");
        assert!(matches!(
            descriptor.provider_server_tool,
            Some(ProviderServerTool::OpenRouterWebFetch { .. })
        ));
        assert!(
            fetch
                .validate_config(Some(&json!({
                    "provider_id":"openrouter",
                    "options":{"max_content_tokens":0}
                })))
                .is_err(),
            "F2"
        );
    }

    #[async_trait]
    impl WebFetchProvider for FakeFetchProvider {
        fn descriptor(&self) -> WebFetchProviderDescriptor {
            WebFetchProviderDescriptor {
                id: "fetch-probe".into(),
                label: "Fetch probe".into(),
                credential: WebSearchCredentialRequirement::None,
                options_schema: json!({ "type": "object" }),
            }
        }

        fn enforces_domain_filter(&self) -> bool {
            true
        }

        async fn fetch(
            &self,
            request: WebFetchRequest,
            _credential: Option<&CredentialMaterial>,
            _domain_filter: Option<&WebDomainFilter>,
        ) -> Result<String, ToolError> {
            self.seen_requests.lock().unwrap().push(request);
            Ok("abcdef".into())
        }
    }

    /// Cause/effect table: C1 free provider/no pin -> executable; C2 paid
    /// provider/exact pin/resolver -> credential consumed and normalized output;
    /// C3 paid/no pin -> config error before tool/network; C4 duplicate id ->
    /// composition error; C5 configured search -> Brain execution so its
    /// provider and credential resolver remain attached. FMECA: routing C5 to
    /// the static Hand produces an authorized `unknown tool` and loses the
    /// Worker-held credential boundary.
    /// Constraints/invariants: the registry is the sole provider-id owner,
    /// credentials remain resolver-held, and configured search always runs in Brain.
    /// Decision rules W1..W5 correspond one-for-one to C1..C5 and their effects.
    #[tokio::test]
    async fn provider_registry_drives_validation_dispatch_and_credentials() {
        let free = fake_provider("free", false);
        let paid = fake_provider("paid", true);
        let registry = WebSearchProviderRegistry::try_new([
            free.clone() as Arc<dyn WebSearchProvider>,
            paid.clone() as Arc<dyn WebSearchProvider>,
        ])
        .unwrap();
        let plugin = WebSearchPlugin::new(registry.clone(), Some(Arc::new(FixedCredential)));

        let (_, free_tool) = plugin
            .configured_tool(Some(&json!({ "provider_id": "free", "options": {} })))
            .unwrap();
        assert_eq!(free_tool.execution_target(), ToolExecutionTarget::Brain);
        let output = free_tool
            .invoke(ToolCall {
                call_id: "free-call".into(),
                tool_id: WEB_SEARCH_TOOL_ID.into(),
                arguments: json!({ "query": "rust" }),
            })
            .await
            .unwrap();
        assert!(output.text().contains("https://result.test"));
        assert_eq!(*free.seen_secret.lock().unwrap(), None);

        let (_, paid_tool) = plugin
            .configured_tool(Some(&json!({
                "provider_id": "paid",
                "credential": { "id": "cred:paid", "revision": 2 },
                "options": {},
            })))
            .unwrap();
        let error = paid_tool
            .invoke(ToolCall {
                call_id: "paid-call".into(),
                tool_id: WEB_SEARCH_TOOL_ID.into(),
                arguments: json!({ "query": "domain driven design" }),
            })
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("paid credential resolution reached")
        );
        assert_eq!(*paid.seen_secret.lock().unwrap(), None);

        assert!(
            plugin
                .configured_tool(Some(&json!({ "provider_id": "paid", "options": {} })))
                .is_err()
        );
        assert_eq!(
            WebSearchProviderRegistry::try_new([
                fake_provider("same", false) as Arc<dyn WebSearchProvider>,
                fake_provider("same", false) as Arc<dyn WebSearchProvider>,
            ])
            .err(),
            Some(WebSearchRegistryError::DuplicateProvider("same".into()))
        );
        assert_eq!(
            registry.config_schema()["oneOf"].as_array().unwrap().len(),
            2
        );
    }

    /// Representation cause/effect table: R1 search and R2 fetch both combine a
    /// small host target vector with one feature-dependent provider-server
    /// descriptor/options payload; boxing that complete shared variant keeps
    /// either enum bounded without a parallel route or a second options owner.
    #[test]
    fn configured_web_routes_have_one_bounded_representation() {
        assert!(std::mem::size_of::<ConfiguredSearchRoute>() <= 64, "R1");
        assert!(std::mem::size_of::<ConfiguredFetchRoute>() <= 64, "R2");
    }

    /// Web configuration cause/effect graph: one normalized ToolPolicyOverride
    /// configures the selected WebFetch or WebSearch plugin. Fetch checks domains
    /// before provider I/O and caps text afterward; Search sends location to the
    /// provider and filters results. No static or executor-wrapper path exists.
    ///
    /// Decision table:
    /// | Rule | tool | domain | setting | effect |
    /// | W1 | web_fetch | allowed | max=3 | provider invoked once; text capped |
    /// | W2 | web_fetch | outside allowlist | any | reject before provider I/O |
    /// | W3 | web_fetch | allowed `.PDF` URL | max=3 | legacy text projection is not policy-capped |
    /// | W4 | web_search | blocked result | location present | provider sees location; result removed |
    /// | W5 | web_fetch | unrestricted | max=0 | provider invoked; text capped to empty |
    /// | W6 | web_fetch | wrong config tag | any | reject before executor construction |
    /// | W7 | web_search | unknown config field | any | reject before provider construction |
    /// Constraints/invariants: policy is normalized once, the configured plugin
    /// is the internal execution owner, domain rejection precedes I/O, `.pdf`
    /// legacy text bypasses its context cap while the raw fetch retains its 1 MiB
    /// safety ceiling, and malformed configuration never widens into an
    /// unconfigured Web tool.
    #[tokio::test]
    async fn normalized_web_configuration_controls_existing_execution_edges() {
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
        };

        let fetch = WebFetchExecutionConfiguration {
            domains: Some(WebDomainFilter::Allow(vec!["docs.example.com".into()])),
            max_content_tokens: Some(3),
        };
        let search_location = WebSearchUserLocation {
            city: Some("Shanghai".into()),
            country: Some("CN".into()),
            region: Some("Shanghai".into()),
            timezone: Some("Asia/Shanghai".into()),
        };
        let search = WebSearchExecutionConfiguration {
            domains: Some(WebDomainFilter::Block(vec!["result.test".into()])),
            user_location: Some(search_location.clone()),
        };
        let toolsets = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: vec![
                ToolPolicyOverride::with_optional_configuration(
                    "web_fetch",
                    ToolExecutionPolicy::default(),
                    Some(
                        serde_json::to_value(WebExecutionConfiguration::WebFetch(fetch.clone()))
                            .expect("WebFetch execution configuration serializes"),
                    ),
                ),
                ToolPolicyOverride::with_optional_configuration(
                    "web_search",
                    ToolExecutionPolicy::default(),
                    Some(
                        serde_json::to_value(WebExecutionConfiguration::WebSearch(search.clone()))
                            .expect("WebSearch execution configuration serializes"),
                    ),
                ),
            ],
        }];
        assert_eq!(
            web_fetch_execution_configuration(&toolsets).expect("valid WebFetch configuration"),
            Some(fetch.clone()),
        );
        assert_eq!(
            web_search_execution_configuration(&toolsets).expect("valid WebSearch configuration"),
            Some(search.clone()),
        );

        let malformed = |name: &str, configuration: Value| {
            vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::with_optional_configuration(
                    name,
                    ToolExecutionPolicy::default(),
                    Some(configuration),
                )],
            }]
        };
        assert!(
            web_fetch_execution_configuration(&malformed(
                "web_fetch",
                json!({"type":"web_search"})
            ))
            .is_err(),
            "W6"
        );
        assert!(
            web_search_execution_configuration(&malformed(
                "web_search",
                json!({"type":"web_search", "unexpected":true})
            ))
            .is_err(),
            "W7"
        );

        let fetch_provider = Arc::new(FakeFetchProvider {
            seen_requests: Mutex::new(Vec::new()),
        });
        let mut fetch_registry = WebSearchProviderRegistry::default();
        fetch_registry
            .register_fetch(fetch_provider.clone())
            .unwrap();
        let fetch_plugin = WebFetchPlugin::new(fetch_registry.clone(), None)
            .with_execution_configuration(Some(fetch.clone()));
        let (_, fetch_tool) = fetch_plugin
            .configured_tool(Some(&json!({
                "provider_id": "fetch-probe",
                "options": {}
            })))
            .unwrap();
        let allowed = fetch_tool
            .invoke(ToolCall {
                call_id: "fetch-allowed".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({ "url": "https://guide.docs.example.com/start" }),
            })
            .await
            .unwrap();
        assert_eq!(allowed.text(), "abc", "W1");
        assert_eq!(fetch_provider.seen_requests.lock().unwrap().len(), 1, "W1");
        assert!(
            fetch_tool
                .invoke(ToolCall {
                    call_id: "fetch-blocked".into(),
                    tool_id: "web_fetch".into(),
                    arguments: json!({ "url": "https://example.net" }),
                })
                .await
                .is_err(),
            "W2"
        );
        assert_eq!(fetch_provider.seen_requests.lock().unwrap().len(), 1, "W2");

        let pdf = fetch_tool
            .invoke(ToolCall {
                call_id: "fetch-pdf".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({ "url": "https://docs.example.com/guide.PDF" }),
            })
            .await
            .unwrap();
        assert_eq!(pdf.text(), "abcdef", "W3");
        assert_eq!(fetch_provider.seen_requests.lock().unwrap().len(), 2, "W3");

        let zero_plugin = WebFetchPlugin::new(fetch_registry, None).with_execution_configuration(
            Some(WebFetchExecutionConfiguration {
                domains: None,
                max_content_tokens: Some(0),
            }),
        );
        let (_, zero_tool) = zero_plugin
            .configured_tool(Some(&json!({
                "provider_id": "fetch-probe",
                "options": {}
            })))
            .unwrap();
        let zero_cap = zero_tool
            .invoke(ToolCall {
                call_id: "fetch-zero-cap".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({ "url": "https://example.net" }),
            })
            .await
            .unwrap();
        assert_eq!(zero_cap.text(), "", "W5");

        let provider = fake_provider("localized", false);
        let plugin = WebSearchPlugin::new(
            WebSearchProviderRegistry::try_new([provider.clone() as Arc<dyn WebSearchProvider>])
                .unwrap(),
            None,
        )
        .with_execution_configuration(Some(search));
        let (_, tool) = plugin
            .configured_tool(Some(&json!({ "provider_id": "localized", "options": {} })))
            .unwrap();
        let output = tool
            .invoke(ToolCall {
                call_id: "search".into(),
                tool_id: "web_search".into(),
                arguments: json!({ "query": "managed agents" }),
            })
            .await
            .unwrap();
        assert_eq!(output.text(), "no web-search results", "W4");
        assert_eq!(
            provider
                .seen_request
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|request| request.user_location.clone()),
            Some(search_location),
            "W4"
        );
    }

    #[tokio::test]
    async fn route_plan_falls_back_only_before_dispatch() {
        // Cause/effect decision table: R1 primary unavailable before dispatch +
        // fallback healthy -> fallback result; R2 primary execution error ->
        // terminal error and no replay. R2 is enforced by the match branch in
        // `WebSearchTool::call`; this test owns R1 and exact target ordering.
        let fallback = fake_provider("fallback", false);
        let registry = WebSearchProviderRegistry::try_new([
            Arc::new(UnavailableProvider) as Arc<dyn WebSearchProvider>,
            fallback as Arc<dyn WebSearchProvider>,
        ])
        .unwrap();
        let plugin = WebSearchPlugin::new(registry, None);
        let (_, tool) = plugin
            .configured_tool(Some(&json!({
                "provider_id": "unavailable",
                "options": {},
                "fallbacks": [{ "provider_id": "fallback", "options": {} }]
            })))
            .unwrap();
        let output = tool
            .invoke(ToolCall {
                call_id: "fallback-call".into(),
                tool_id: WEB_SEARCH_TOOL_ID.into(),
                arguments: json!({ "query": "bounded contexts" }),
            })
            .await
            .unwrap();
        assert!(output.text().contains("https://result.test"), "R1");
    }

    #[test]
    fn duckduckgo_response_normalizes_abstract_and_topics() {
        let results = ddg_results(
            DdgResponse {
                heading: "Rust".into(),
                abstract_text: "A language".into(),
                abstract_url: "https://rust-lang.org".into(),
                related_topics: vec![DdgTopic {
                    text: "Cargo".into(),
                    first_url: "https://doc.rust-lang.org/cargo".into(),
                }],
            },
            8,
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/cargo");
    }

    #[test]
    fn configured_provider_ids_are_unique() {
        let ids = WebSearchProviderRegistry::builtins()
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from(["brave".into(), "duckduckgo".into()]));
    }
}
