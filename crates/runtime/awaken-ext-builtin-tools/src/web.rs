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
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError, ToolExecutionTarget};
use awaken_runtime_contract::{CredentialMaterial, CredentialRef, CredentialUsage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::erasure::erase_for;

pub const WEB_SEARCH_PLUGIN_ID: &str = "web_search";
pub const WEB_SEARCH_TOOL_ID: &str = "web_search";
pub const DUCKDUCKGO_PROVIDER_ID: &str = "duckduckgo";
pub const BRAVE_PROVIDER_ID: &str = "brave";

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

/// HTTP GET a URL and return the response body as text (UTF-8 lossy, capped).
pub struct WebFetchTool;

#[derive(Deserialize)]
pub struct WebFetchArgs {
    pub url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    type Args = WebFetchArgs;
    type Output = String;

    fn id(&self) -> &str {
        "web_fetch"
    }

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

#[derive(Debug, Clone, Deserialize)]
pub struct WebSearchArgs {
    pub query: String,
    #[serde(default)]
    pub count: Option<usize>,
}

/// Configured model-callable tool. Native Runtime and ACP MCP export both use
/// this exact instance type.
pub struct WebSearchTool {
    provider: Arc<dyn WebSearchProvider>,
    descriptor: WebSearchProviderDescriptor,
    config: WebSearchConfig,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
}

impl WebSearchTool {
    fn configured(
        provider: RegisteredWebSearchProvider,
        config: WebSearchConfig,
        credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    ) -> Self {
        Self {
            descriptor: provider.descriptor,
            provider: provider.provider,
            config,
            credentials,
        }
    }

    pub fn duckduckgo() -> Self {
        let registry = WebSearchProviderRegistry::builtins();
        Self::configured(
            registry
                .provider(DUCKDUCKGO_PROVIDER_ID)
                .expect("DuckDuckGo is built in"),
            WebSearchConfig {
                provider_id: DUCKDUCKGO_PROVIDER_ID.into(),
                credential: None,
                options: json!({}),
            },
            None,
        )
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

    fn id(&self) -> &str {
        WEB_SEARCH_TOOL_ID
    }

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
                },
                credential.as_ref(),
            )
            .await?;
        Ok(render_results(&results))
    }
}

pub struct WebSearchPlugin {
    registry: WebSearchProviderRegistry,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
}

impl WebSearchPlugin {
    pub fn new(
        registry: WebSearchProviderRegistry,
        credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    ) -> Self {
        Self {
            registry,
            credentials,
        }
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
            WebSearchTool::configured(provider, config, self.credentials.clone()),
            ToolExecutionTarget::Sandbox,
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
        contributions.register_dynamic_tool(awaken_runtime_contract::plugin::DynamicTool {
            descriptor,
            tool,
        });
        Ok(contributions)
    }
}

pub fn web_search_descriptor() -> ToolDescriptor {
    ToolDescriptor::pinned(
        "builtin",
        WEB_SEARCH_TOOL_ID,
        "Search the web through the configured platform provider",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "search query" },
                "count": { "type": "integer", "minimum": 1, "maximum": 20 },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    )
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
                for key in ["country", "search_lang", "safesearch"] {
                    if let Some(value) = options.get(key).and_then(Value::as_str) {
                        call = call.query(key, value);
                    }
                }
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
        })
    }

    /// Cause/effect table: C1 free provider/no pin -> executable; C2 paid
    /// provider/exact pin/resolver -> credential consumed and normalized output;
    /// C3 paid/no pin -> config error before tool/network; C4 duplicate id ->
    /// composition error. These are the minimal independent provider decisions.
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
        let output = free_tool
            .invoke(ToolCall {
                call_id: "free-call".into(),
                tool_id: WEB_SEARCH_TOOL_ID.into(),
                arguments: json!({ "query": "rust" }),
            })
            .await
            .unwrap();
        assert!(output.content.contains("https://result.test"));
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
