//! Network tools. `web_search` has one provider registry shared by native and
//! mediated ACP execution; providers own HTTP details while the platform owns
//! configuration, exact credential resolution, and tool exposure.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, IdBound, Plugin, PluginConfigError, PluginManifest,
};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{
    RawTool, Tool, ToolError, ToolExecutionTarget, ToolExecutor, ToolOutput, ToolRecoveryCapability,
};
use awaken_runtime_contract::{CredentialMaterial, CredentialRef, CredentialUsage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::erasure::erase_for;

pub const WEB_SEARCH_PLUGIN_ID: &str = "web_search";
pub const WEB_SEARCH_TOOL_ID: &str = "web_search";
pub const DUCKDUCKGO_PROVIDER_ID: &str = "duckduckgo";
pub const BRAVE_PROVIDER_ID: &str = "brave";

/// Extension-owned execution settings decoded from the neutral policy's one
/// opaque configuration value. The serde shape is the durable shape previously
/// stored by the neutral contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WebExecutionConfiguration {
    WebFetch(WebFetchExecutionConfiguration),
    WebSearch(WebSearchExecutionConfiguration),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "domains",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WebDomainFilter {
    Allow(Vec<String>),
    Block(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchExecutionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<WebDomainFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchExecutionConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<WebDomainFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<WebSearchUserLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchUserLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Cap on fetched bytes so a huge response cannot blow up the transcript.
const MAX_BODY: u64 = 1 << 20;

fn execution_configuration<'a>(
    toolsets: &'a [awaken_runtime_contract::agent_bindings::ToolsetPolicy],
    name: &str,
) -> Option<&'a Value> {
    toolsets
        .iter()
        .find(|policy| {
            matches!(
                policy.source,
                awaken_runtime_contract::agent_bindings::ToolsetSource::Agent
            )
        })
        .and_then(|policy| policy.configuration_for(name))
}

/// Read the normalized `web_fetch` settings from the executable toolset.
/// Authoring and runtime never maintain a parallel settings map.
pub fn web_fetch_execution_configuration(
    toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
) -> Result<Option<WebFetchExecutionConfiguration>, String> {
    let Some(value) = execution_configuration(toolsets, "web_fetch") else {
        return Ok(None);
    };
    match serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid web_fetch execution configuration: {error}"))?
    {
        WebExecutionConfiguration::WebFetch(configuration) => Ok(Some(configuration)),
        WebExecutionConfiguration::WebSearch(_) => {
            Err("web_fetch policy carries web_search execution configuration".to_string())
        }
    }
}

/// Read the normalized `web_search` settings from the executable toolset. The
/// same value configures Native dispatch and ACP export.
pub fn web_search_execution_configuration(
    toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
) -> Result<Option<WebSearchExecutionConfiguration>, String> {
    let Some(value) = execution_configuration(toolsets, "web_search") else {
        return Ok(None);
    };
    match serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid web_search execution configuration: {error}"))?
    {
        WebExecutionConfiguration::WebSearch(configuration) => Ok(Some(configuration)),
        WebExecutionConfiguration::WebFetch(_) => {
            Err("web_search policy carries web_fetch execution configuration".to_string())
        }
    }
}

async fn blocking<F, T>(work: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| ToolError::Execution(format!("blocking task: {err}")))?
}

/// HTTP GET a URL and return the response body as text (UTF-8 lossy, capped).
pub struct WebFetchTool;

fn url_matches_filter(url: &url::Url, filter: &WebDomainFilter) -> bool {
    let matches = |configured: &str| {
        let (domain, path) = configured.split_once('/').unwrap_or((configured, ""));
        let host_matches = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case(domain)
                || host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
        });
        host_matches
            && (path.is_empty()
                || url.path() == format!("/{path}")
                || url.path().starts_with(&format!("/{path}/")))
    };
    match filter {
        WebDomainFilter::Allow(domains) => domains.iter().any(|domain| matches(domain)),
        WebDomainFilter::Block(domains) => !domains.iter().any(|domain| matches(domain)),
    }
}

/// Existing Session Hand executor narrowed by the Agent's WebFetch settings.
/// The wrapper is placement-neutral: Workdir, Namespace, Container, Native,
/// and ACP all keep their current executor while sharing one pre/post policy.
pub struct ConfiguredWebToolExecutor {
    inner: Arc<dyn ToolExecutor>,
    web_fetch: WebFetchExecutionConfiguration,
}

impl ConfiguredWebToolExecutor {
    #[must_use]
    pub fn new(inner: Arc<dyn ToolExecutor>, web_fetch: WebFetchExecutionConfiguration) -> Self {
        Self { inner, web_fetch }
    }
}

fn truncate_text_blocks(output: &mut ToolOutput, max_bytes: usize) {
    let mut remaining = max_bytes;
    for block in &mut output.content {
        let awaken_runtime_contract::ContentBlock::Text { text } = block else {
            continue;
        };
        if text.len() <= remaining {
            remaining -= text.len();
            continue;
        }
        let mut boundary = remaining.min(text.len());
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        remaining = 0;
    }
}

#[async_trait]
impl ToolExecutor for ConfiguredWebToolExecutor {
    fn recovery_capability(&self, tool_id: &str) -> ToolRecoveryCapability {
        self.inner.recovery_capability(tool_id)
    }

    async fn invoke(
        &self,
        call: &awaken_runtime_contract::tool::ToolCall,
    ) -> Result<ToolOutput, ToolError> {
        if call.tool_id != "web_fetch" {
            return self.inner.invoke(call).await;
        }
        let configuration = &self.web_fetch;
        let url = call
            .arguments
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("url is required".into()))?;
        let url = url::Url::parse(url)
            .map_err(|error| ToolError::InvalidArguments(format!("url: {error}")))?;
        if let Some(filter) = &configuration.domains
            && !url_matches_filter(&url, filter)
        {
            return Err(ToolError::Execution(
                "web_fetch URL is outside the configured domain policy".into(),
            ));
        }
        let mut output = self.inner.invoke(call).await?;
        if let Some(max_content_tokens) = configuration.max_content_tokens
            // Binary content is not part of the configured text-context cap.
            // Structured binary blocks are already ignored below; preserve the
            // explicit PDF URL case for legacy executors that return lossy text.
            && !url.path().to_ascii_lowercase().ends_with(".pdf")
        {
            truncate_text_blocks(
                &mut output,
                usize::try_from(max_content_tokens).unwrap_or(usize::MAX),
            );
        }
        Ok(output)
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebFetchArgs {
    /// URL to fetch.
    pub url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    type Args = WebFetchArgs;
    type Output = String;
    const ID: &'static str = "web_fetch";
    const DESCRIPTION: &'static str = "Fetch a URL";

    async fn call(&self, args: WebFetchArgs) -> Result<String, ToolError> {
        blocking(move || {
            let response = ureq::get(&args.url)
                .call()
                .map_err(|err| ToolError::Execution(format!("fetch {}: {err}", args.url)))?;
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(MAX_BODY)
                .read_to_end(&mut bytes)
                .map_err(|err| ToolError::Execution(format!("read body: {err}")))?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
        .await
    }
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
        if options.is_object() || options.is_null() {
            Ok(())
        } else {
            Err("provider options must be an object".into())
        }
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        credential: Option<&CredentialMaterial>,
    ) -> Result<Vec<WebSearchResult>, ToolError>;
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
}

#[derive(Clone)]
struct RegisteredWebSearchProvider {
    descriptor: WebSearchProviderDescriptor,
    provider: Arc<dyn WebSearchProvider>,
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
        Self::try_new([
            Arc::new(DuckDuckGoProvider) as Arc<dyn WebSearchProvider>,
            Arc::new(BraveSearchProvider) as Arc<dyn WebSearchProvider>,
        ])
        .expect("built-in web-search provider descriptors are unique and valid")
    }

    pub fn register(
        &mut self,
        provider: Arc<dyn WebSearchProvider>,
    ) -> Result<(), WebSearchRegistryError> {
        let descriptor = provider.descriptor();
        if descriptor.id.trim().is_empty()
            || descriptor.label.trim().is_empty()
            || !descriptor
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            || !descriptor.options_schema.is_object()
        {
            return Err(WebSearchRegistryError::InvalidDescriptor);
        }
        if self.providers.contains_key(&descriptor.id) {
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

    pub fn descriptors(&self) -> Vec<WebSearchProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor.clone())
            .collect()
    }

    fn provider(&self, id: &str) -> Option<RegisteredWebSearchProvider> {
        self.providers.get(id).cloned()
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
        let variants = descriptors
            .into_iter()
            .map(|provider| {
                let mut properties = serde_json::Map::from_iter([
                    (
                        "provider_id".into(),
                        json!({ "type": "string", "const": provider.id, "title": provider.label }),
                    ),
                    ("options".into(), provider.options_schema),
                ]);
                let mut required = vec!["provider_id"];
                if let WebSearchCredentialRequirement::Exact(usage) = provider.credential {
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
                    "title": provider.label,
                    "properties": properties,
                    "required": required,
                    "additionalProperties": false,
                })
            })
            .collect::<Vec<_>>();
        json!({ "title": "Web search", "oneOf": variants })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebSearchConfig {
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
    #[serde(default)]
    pub options: Value,
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
/// this exact instance type.
struct WebSearchTool {
    provider: Arc<dyn WebSearchProvider>,
    descriptor: WebSearchProviderDescriptor,
    config: WebSearchConfig,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    execution_configuration: Option<WebSearchExecutionConfiguration>,
}

impl WebSearchTool {
    fn configured(
        provider: RegisteredWebSearchProvider,
        config: WebSearchConfig,
        credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
        execution_configuration: Option<WebSearchExecutionConfiguration>,
    ) -> Self {
        Self {
            descriptor: provider.descriptor,
            provider: provider.provider,
            config,
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
        let credential = match &self.descriptor.credential {
            WebSearchCredentialRequirement::None => None,
            WebSearchCredentialRequirement::Exact(usage) => {
                let reference = self.config.credential.as_ref().ok_or_else(|| {
                    ToolError::Execution("web-search credential pin is missing".into())
                })?;
                Some(
                    self.credentials
                        .as_ref()
                        .ok_or_else(|| {
                            ToolError::Execution(
                                "web-search credential resolver is unavailable".into(),
                            )
                        })?
                        .resolve(reference, &self.descriptor.id, usage)
                        .await
                        .map_err(ToolError::Execution)?,
                )
            }
        };
        let results = self
            .provider
            .search(
                WebSearchRequest {
                    query: args.query,
                    count: args.count.unwrap_or(8).clamp(1, 20),
                    options: self.config.options.clone(),
                    user_location: self
                        .execution_configuration
                        .as_ref()
                        .and_then(|configuration| configuration.user_location.clone()),
                },
                credential.as_ref(),
            )
            .await?;
        let results = match self
            .execution_configuration
            .as_ref()
            .and_then(|configuration| configuration.domains.as_ref())
        {
            Some(filter) => results
                .into_iter()
                .filter(|result| {
                    url::Url::parse(&result.url).is_ok_and(|url| url_matches_filter(&url, filter))
                })
                .collect(),
            None => results,
        };
        Ok(render_results(&results))
    }
}

#[derive(Clone)]
pub struct WebSearchPlugin {
    registry: WebSearchProviderRegistry,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    execution_configuration: Option<WebSearchExecutionConfiguration>,
}

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
    ) -> Result<(WebSearchConfig, RegisteredWebSearchProvider), PluginConfigError> {
        let config: WebSearchConfig =
            serde_json::from_value(config.cloned().ok_or_else(|| {
                PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, "config is required")
            })?)
            .map_err(|error| PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, error.to_string()))?;
        let provider = self.registry.provider(&config.provider_id).ok_or_else(|| {
            PluginConfigError::new(
                WEB_SEARCH_PLUGIN_ID,
                format!("unknown provider `{}`", config.provider_id),
            )
        })?;
        provider
            .provider
            .validate_options(&config.options)
            .map_err(|error| PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, error))?;
        match (&provider.descriptor.credential, &config.credential) {
            (WebSearchCredentialRequirement::None, None)
            | (WebSearchCredentialRequirement::Exact(_), Some(_)) => {}
            (WebSearchCredentialRequirement::None, Some(_)) => {
                return Err(PluginConfigError::new(
                    WEB_SEARCH_PLUGIN_ID,
                    "the selected provider does not consume a credential",
                ));
            }
            (WebSearchCredentialRequirement::Exact(_), None) => {
                return Err(PluginConfigError::new(
                    WEB_SEARCH_PLUGIN_ID,
                    "the selected provider requires an exact credential pin",
                ));
            }
        }
        Ok((config, provider))
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
        let (config, provider) = self.configured_provider(config)?;
        if matches!(
            &provider.descriptor.credential,
            WebSearchCredentialRequirement::Exact(_)
        ) && self.credentials.is_none()
        {
            return Err(PluginConfigError::new(
                WEB_SEARCH_PLUGIN_ID,
                "the selected provider requires an installed credential materializer",
            ));
        }
        let descriptor = web_search_descriptor();
        let tool = erase_for(
            WebSearchTool::configured(
                provider,
                config,
                self.credentials.clone(),
                self.execution_configuration.clone(),
            ),
            // Search is a configured Worker plugin: provider selection and exact
            // credential materialization live at the Brain boundary. Sending it
            // to the static Environment Hand loses that configuration and yields
            // an `unknown tool` after permission was already granted.
            ToolExecutionTarget::Brain,
        );
        Ok((descriptor, tool))
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

/// The static network-tool bundle owns only `web_fetch`. Search is exposed
/// exclusively by [`WebSearchPlugin`], avoiding a second unconfigured path.
pub fn web_hand_tools() -> Vec<Arc<dyn RawTool>> {
    vec![erase_for(WebFetchTool, ToolExecutionTarget::Sandbox)]
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

    struct EchoExecutor;

    #[async_trait]
    impl ToolExecutor for EchoExecutor {
        async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(&call.call_id, "abcdef"))
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

    /// Web configuration cause/effect graph: one normalized ToolPolicyOverride
    /// selects the WebFetch executor policy or WebSearch provider policy. Fetch
    /// checks domains before invoking the existing placement executor and caps
    /// text afterward; Search sends location to the provider and filters results.
    ///
    /// Decision table:
    /// | Rule | tool | domain | setting | effect |
    /// | W1 | web_fetch | allowed | max=3 | inner invoked; text capped |
    /// | W2 | web_fetch | outside allowlist | any | reject before inner invoke |
    /// | W3 | non-web | n/a | fetch config present | unchanged delegation |
    /// | W4 | web_fetch | allowed `.PDF` URL | max=3 | legacy text projection is not policy-capped |
    /// | W5 | web_search | blocked result | location present | provider sees location; result removed |
    /// | W6 | web_fetch | unrestricted | max=0 | inner invoked; text capped to empty |
    /// | W7 | web_fetch | wrong config tag | any | reject before executor construction |
    /// | W8 | web_search | unknown config field | any | reject before provider construction |
    /// Constraints/invariants: policy is normalized once, domain rejection
    /// precedes I/O, the wrapper is the only Agent policy owner, `.pdf` legacy
    /// text bypasses its context cap while the raw fetch retains its 1 MiB safety
    /// ceiling, and malformed opaque configuration never widens into an
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
            "W7"
        );
        assert!(
            web_search_execution_configuration(&malformed(
                "web_search",
                json!({"type":"web_search", "unexpected":true})
            ))
            .is_err(),
            "W8"
        );

        let executor = ConfiguredWebToolExecutor::new(Arc::new(EchoExecutor), fetch);
        let allowed = executor
            .invoke(&ToolCall {
                call_id: "fetch-allowed".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({ "url": "https://guide.docs.example.com/start" }),
            })
            .await
            .unwrap();
        assert_eq!(allowed.text(), "abc", "W1");
        assert!(
            executor
                .invoke(&ToolCall {
                    call_id: "fetch-blocked".into(),
                    tool_id: "web_fetch".into(),
                    arguments: json!({ "url": "https://example.net" }),
                })
                .await
                .is_err(),
            "W2"
        );
        let other = executor
            .invoke(&ToolCall {
                call_id: "read".into(),
                tool_id: "read".into(),
                arguments: json!({}),
            })
            .await
            .unwrap();
        assert_eq!(other.text(), "abcdef", "W3");
        let pdf = executor
            .invoke(&ToolCall {
                call_id: "fetch-pdf".into(),
                tool_id: "web_fetch".into(),
                arguments: json!({ "url": "https://docs.example.com/guide.PDF" }),
            })
            .await
            .unwrap();
        assert_eq!(pdf.text(), "abcdef", "W4");

        let zero_cap = ConfiguredWebToolExecutor::new(
            Arc::new(EchoExecutor),
            WebFetchExecutionConfiguration {
                domains: None,
                max_content_tokens: Some(0),
            },
        )
        .invoke(&ToolCall {
            call_id: "fetch-zero-cap".into(),
            tool_id: "web_fetch".into(),
            arguments: json!({ "url": "https://example.net" }),
        })
        .await
        .unwrap();
        assert_eq!(zero_cap.text(), "", "W6");

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
        assert_eq!(output.text(), "no web-search results", "W5");
        assert_eq!(
            provider
                .seen_request
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|request| request.user_location.clone()),
            Some(search_location),
            "W5"
        );
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
