// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: This source implements the repository-controlled builtin product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Network tools. `web_search` has one provider registry shared by native and
//! mediated ACP execution; providers own HTTP details while the platform owns
//! configuration, exact credential resolution, and tool exposure.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::credential::Credential;
use awaken_runtime_contract::credential::CredentialPurpose;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, IdBound, Plugin, PluginConfigError, PluginManifest,
};
use awaken_runtime_contract::resolved::{
    OpenRouterWebFetchParameters, OpenRouterWebSearchParameters, ProviderServerTool, ToolDescriptor,
};
use awaken_runtime_contract::{
    CredentialAccess, CredentialRef, CredentialTarget, CredentialUsage, ModelExposurePolicy,
    PlaintextBoundary,
};
use awaken_tool_contract::tool::{RawTool, Tool, ToolError, ToolExecutionTarget};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::erasure::erase_for;

mod brave;
pub use brave::BraveSearchProvider;
mod configuration;
#[cfg(test)]
use configuration::WebExecutionConfiguration;
use configuration::{
    EffectiveWebFetchConfiguration, EffectiveWebSearchConfiguration, domains_permit_any_url,
    url_matches_filters,
};
pub use configuration::{
    WebDomainFilter, WebFetchExecutionConfiguration, WebSearchExecutionConfiguration,
    WebSearchUserLocation, web_fetch_execution_configuration, web_search_execution_configuration,
};
mod duckduckgo;
pub use duckduckgo::DuckDuckGoProvider;
mod managed;
#[allow(unused_imports)]
pub use managed::managed_web_route_ref;
pub use managed::{
    ManagedGatewayWebProvider, ManagedWebGatewayEndpoint, ManagedWebRouteError,
    ManagedWebRouteResolver,
};
mod configured_fetch;
use configured_fetch::{ConfiguredFetchTarget, RoutedWebFetchTool};
mod providers;
mod sources;
pub use providers::AwakenDirectFetchProvider;
use providers::{
    ProviderServerWebFetchTool, blocking, native_server_search_descriptors,
    openrouter_server_fetch_descriptor, openrouter_server_search_descriptor,
};

pub const WEB_SEARCH_PLUGIN_ID: &str = "web_search";
pub const WEB_SEARCH_TOOL_ID: &str = "web_search";
pub const WEB_FETCH_PLUGIN_ID: &str = "web_fetch";
pub const WEB_FETCH_TOOL_ID: &str = "web_fetch";
pub const DUCKDUCKGO_PROVIDER_ID: &str = "duckduckgo";
pub const BRAVE_PROVIDER_ID: &str = "brave";
pub const AWAKEN_DIRECT_PROVIDER_ID: &str = "awaken-direct";
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const DEEPSEEK_RESPONSES_PROVIDER_ID: &str = "deepseek-responses";
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub const DEEPSEEK_ANTHROPIC_PROVIDER_ID: &str = "deepseek-anthropic";
pub const GEMINI_PROVIDER_ID: &str = "gemini";
pub const VERTEX_PROVIDER_ID: &str = "vertex";
pub const AWAKEN_CLOUD_PROVIDER_ID: &str = "awaken-cloud";

/// Cap on fetched bytes so a huge response cannot blow up the transcript.
const MAX_BODY: u64 = 1 << 20;

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
        credential: Option<&Credential>,
        domain_filters: &[WebDomainFilter],
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
    /// Authoring-only exact source selection. The publication resolver consumes
    /// this field and replaces it with `credential_access`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
    /// Secret-free, publication-pinned execution authorization. Runtime accepts
    /// this field only and never reconstructs policy from a source reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_access: Option<CredentialAccess>,
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
    /// Resolve one exact outbound HTTP header using the common credential
    /// contract. Other usage shapes belong to their own last-mile adapters and
    /// are rejected when a Web provider is registered.
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
        credential: Option<&Credential>,
    ) -> Result<Vec<WebSearchResult>, ToolError>;
}

fn validate_object_options(options: &Value) -> Result<(), String> {
    if options.is_object() || options.is_null() {
        Ok(())
    } else {
        Err("provider options must be an object".into())
    }
}

/// Runtime-host adapter for an already-published exact credential access.
/// Implementations validate Workspace, revision, target, usage, and plaintext
/// boundary, then return only the outbound wire credential shape. Providers
/// never receive Vault material or learn repository mechanics.
#[async_trait]
pub trait WebSearchCredentialResolver: Send + Sync {
    async fn resolve(
        &self,
        access: &CredentialAccess,
        provider_id: &str,
    ) -> Result<Credential, String>;
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

mod registry;
pub use registry::WebSearchProviderRegistry;
#[cfg(test)]
use registry::validate_web_credential_requirement;
use registry::{RegisteredWebFetchProvider, RegisteredWebSearchProvider};

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
    if let WebSearchCredentialRequirement::Exact(_) = credential {
        properties.insert(
            "credential".into(),
            json!({
                "type": "object",
                "title": "Vault credential",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_access: Option<CredentialAccess>,
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
            credential_access: self.credential_access.clone(),
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
    execution_configuration: Option<EffectiveWebSearchConfiguration>,
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
        execution_configuration: Option<EffectiveWebSearchConfiguration>,
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
        if self
            .execution_configuration
            .as_ref()
            .is_some_and(|policy| !domains_permit_any_url(&policy.domains))
        {
            return Err(ToolError::DefinitiveFailure(
                "url_not_allowed: no domain is permitted by the combined web_search policy".into(),
            ));
        }
        let mut last_unavailable = None;
        for target in &self.targets {
            let credential = resolve_credential(
                &target.descriptor.credential,
                target.config.credential_access.as_ref(),
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
                        .map(|configuration| &configuration.domains)
                        .filter(|domains| !domains.is_empty())
                    {
                        Some(filter) => results
                            .into_iter()
                            .filter(|result| {
                                url::Url::parse(&result.url)
                                    .is_ok_and(|url| url_matches_filters(&url, filter))
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
        Err(last_unavailable.unwrap_or_else(|| {
            ToolError::DefinitiveFailure("web-search route plan has no targets".into())
        }))
    }
}

async fn resolve_credential(
    requirement: &WebSearchCredentialRequirement,
    access: Option<&CredentialAccess>,
    resolver: Option<&Arc<dyn WebSearchCredentialResolver>>,
    provider_id: &str,
    tool: &str,
) -> Result<Option<Credential>, ToolError> {
    match requirement {
        WebSearchCredentialRequirement::None => Ok(None),
        WebSearchCredentialRequirement::Exact(_) => {
            let access = access.ok_or_else(|| {
                ToolError::DefinitiveFailure(format!(
                    "{tool} published credential access is missing"
                ))
            })?;
            resolver
                .ok_or_else(|| {
                    ToolError::DefinitiveFailure(format!(
                        "{tool} credential resolver is unavailable"
                    ))
                })?
                .resolve(access, provider_id)
                .await
                .map(Some)
                .map_err(ToolError::DefinitiveFailure)
        }
    }
}

#[derive(Clone)]
pub struct WebSearchPlugin {
    registry: WebSearchProviderRegistry,
    credentials: Option<Arc<dyn WebSearchCredentialResolver>>,
    execution_configuration: Option<EffectiveWebSearchConfiguration>,
}

enum ConfiguredWebRoute<T> {
    Host(Vec<T>),
    ProviderServer(ProviderServerTool),
}

#[derive(Clone, Copy)]
enum CredentialConfigForm {
    Authoring,
    Published,
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
        self.execution_configuration = configuration.map(Into::into);
        self
    }

    /// Configure the one WebSearch realization from the Run's frozen callers.
    pub fn with_inherited_execution_configuration(
        mut self,
        own: Option<WebSearchExecutionConfiguration>,
        inherited: &awaken_runtime_contract::activation::InheritedToolsets,
    ) -> Result<Self, String> {
        self.execution_configuration =
            configuration::effective_search_configuration(own, inherited)?;
        Ok(self)
    }

    fn configured_provider(
        &self,
        config: Option<&Value>,
        form: CredentialConfigForm,
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
                .is_some_and(|policy| !policy.domains.is_empty() || policy.user_location.is_some())
            {
                return Err(PluginConfigError::new(
                    WEB_SEARCH_PLUGIN_ID,
                    "provider-server realization cannot enforce the Agent WebSearch execution policy",
                ));
            }
            let projection = match config.provider_id.as_str() {
                OPENROUTER_PROVIDER_ID => {
                    let parameters: OpenRouterWebSearchParameters =
                        serde_json::from_value(config.options).map_err(|error| {
                            PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, error.to_string())
                        })?;
                    ProviderServerTool::openrouter_web_search(parameters)
                }
                OPENAI_PROVIDER_ID => empty_native_search_options(
                    config.options,
                    ProviderServerTool::openai_web_search(),
                )?,
                DEEPSEEK_RESPONSES_PROVIDER_ID => empty_native_search_options(
                    config.options,
                    ProviderServerTool::deepseek_responses_web_search(),
                )?,
                ANTHROPIC_PROVIDER_ID => empty_native_search_options(
                    config.options,
                    ProviderServerTool::anthropic_web_search(),
                )?,
                DEEPSEEK_ANTHROPIC_PROVIDER_ID => empty_native_search_options(
                    config.options,
                    ProviderServerTool::deepseek_anthropic_web_search(),
                )?,
                GEMINI_PROVIDER_ID => empty_native_search_options(
                    config.options,
                    ProviderServerTool::gemini_web_search(),
                )?,
                VERTEX_PROVIDER_ID => empty_native_search_options(
                    config.options,
                    ProviderServerTool::vertex_web_search(),
                )?,
                _ => unreachable!("registered server search providers are exhaustively projected"),
            };
            return Ok(ConfiguredWebRoute::ProviderServer(projection));
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
                &target,
                form,
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
        self.configured_provider(config, CredentialConfigForm::Authoring)
            .map(|_| ())
    }

    pub fn configured_tool(
        &self,
        config: Option<&Value>,
    ) -> Result<(ToolDescriptor, Arc<dyn RawTool>), PluginConfigError> {
        let route = self.configured_provider(config, CredentialConfigForm::Published)?;
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
        let mut descriptor = descriptor;
        if self
            .execution_configuration
            .as_ref()
            .is_some_and(|policy| !domains_permit_any_url(&policy.domains))
        {
            descriptor.description.push_str(" No domain is permitted by the combined policy; every call returns url_not_allowed.");
        }
        Ok((descriptor, tool))
    }
}

fn empty_native_search_options(
    options: Value,
    projection: ProviderServerTool,
) -> Result<ProviderServerTool, PluginConfigError> {
    let options = options.as_object().ok_or_else(|| {
        PluginConfigError::new(WEB_SEARCH_PLUGIN_ID, "provider options must be an object")
    })?;
    if !options.is_empty() {
        return Err(PluginConfigError::new(
            WEB_SEARCH_PLUGIN_ID,
            "this provider-native WebSearch route does not accept options",
        ));
    }
    Ok(projection)
}

fn validate_target(
    plugin_id: &str,
    requirement: &WebSearchCredentialRequirement,
    target: &WebProviderTarget,
    form: CredentialConfigForm,
    options: &Value,
    validate_options: impl FnOnce(&Value) -> Result<(), String>,
) -> Result<(), PluginConfigError> {
    validate_options(options).map_err(|error| PluginConfigError::new(plugin_id, error))?;
    match (
        requirement,
        form,
        &target.credential,
        &target.credential_access,
    ) {
        (WebSearchCredentialRequirement::None, _, None, None)
        | (
            WebSearchCredentialRequirement::Exact(_),
            CredentialConfigForm::Authoring,
            Some(_),
            None,
        ) => Ok(()),
        (
            WebSearchCredentialRequirement::Exact(usage),
            CredentialConfigForm::Published,
            None,
            Some(access),
        ) if access.usage == *usage
            && access.target
                == Some(CredentialTarget::new(
                    CredentialPurpose::WebProviderAuthorization,
                    target.provider_id.clone(),
                ))
            && access.policy.model_exposure == ModelExposurePolicy::Forbidden
            && access.policy.allowed_plaintext_holders.len() == 1
            && access
                .policy
                .allowed_plaintext_holders
                .iter()
                .all(|holder| {
                    holder.boundary == PlaintextBoundary::Worker
                        && holder.trust_domain.0
                            == awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN
                }) =>
        {
            Ok(())
        }
        (WebSearchCredentialRequirement::None, _, _, _) => Err(PluginConfigError::new(
            plugin_id,
            "the selected provider does not consume a credential",
        )),
        (WebSearchCredentialRequirement::Exact(_), CredentialConfigForm::Authoring, _, _) => {
            Err(PluginConfigError::new(
                plugin_id,
                "the selected provider requires one exact credential source pin",
            ))
        }
        (WebSearchCredentialRequirement::Exact(_), CredentialConfigForm::Published, _, _) => {
            Err(PluginConfigError::new(
                plugin_id,
                "the selected provider requires one valid published credential access",
            ))
        }
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
        Err(ToolError::DefinitiveFailure(
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
    execution_configuration: Option<EffectiveWebFetchConfiguration>,
    source_catalogue: Option<Arc<sources::SourceCatalogue>>,
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
            source_catalogue: None,
        }
    }

    #[must_use]
    pub fn with_execution_configuration(
        mut self,
        configuration: Option<WebFetchExecutionConfiguration>,
    ) -> Self {
        self.execution_configuration = configuration.map(Into::into);
        self
    }

    /// Project source ownership from the existing immutable descriptor catalog.
    /// No names authored by a model can select their source class.
    #[must_use]
    pub fn with_tool_source_catalogue(mut self, descriptors: &[ToolDescriptor]) -> Self {
        self.source_catalogue = Some(Arc::new(sources::SourceCatalogue::from_descriptors(
            descriptors,
        )));
        self
    }

    /// Configure the one WebFetch realization from the Run's frozen callers.
    pub fn with_inherited_execution_configuration(
        mut self,
        own: Option<WebFetchExecutionConfiguration>,
        inherited: &awaken_runtime_contract::activation::InheritedToolsets,
    ) -> Result<Self, String> {
        self.execution_configuration =
            configuration::effective_fetch_configuration(own, inherited)?;
        Ok(self)
    }

    pub fn default_config() -> Value {
        json!({ "provider_id": AWAKEN_DIRECT_PROVIDER_ID, "options": {} })
    }

    fn configured_route(
        &self,
        config: Option<&Value>,
        form: CredentialConfigForm,
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
                !policy.domains.is_empty()
                    || policy.max_content_tokens.is_some()
                    || !policy.url_sources.is_empty()
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
                .is_some_and(|configuration| !configuration.domains.is_empty())
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
                &target,
                form,
                &target.options,
                |options| provider.provider.validate_options(options),
            )?;
            targets.push((provider, target));
        }
        Ok(ConfiguredWebRoute::Host(targets))
    }

    pub fn validate_config(&self, config: Option<&Value>) -> Result<(), PluginConfigError> {
        self.configured_route(config, CredentialConfigForm::Authoring)
            .map(|_| ())
    }

    pub fn configured_tool(
        &self,
        config: Option<&Value>,
    ) -> Result<(ToolDescriptor, Arc<dyn RawTool>), PluginConfigError> {
        match self.configured_route(config, CredentialConfigForm::Published)? {
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
                    {
                        let mut descriptor = web_fetch_descriptor();
                        if self
                            .execution_configuration
                            .as_ref()
                            .is_some_and(|policy| !domains_permit_any_url(&policy.domains))
                        {
                            descriptor.description.push_str(" No domain is permitted by the combined policy; every call returns url_not_allowed.");
                        }
                        descriptor
                    },
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
                phase_hooks: vec![
                    awaken_runtime_contract::plugin::PhaseHookPoint::BeforeInference,
                    awaken_runtime_contract::plugin::PhaseHookPoint::AfterTool,
                ],
                state_keys: IdBound::Exact(vec![sources::STATE_KEY.into()]),
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
        if let Some(policy) = &self.execution_configuration
            && !policy.url_sources.is_empty()
        {
            contributions.declare_state_key(sources::STATE_KEY);
            contributions.register_hook(Arc::new(sources::UrlSourceObserver));
            contributions.register_hook(Arc::new(sources::UrlSourceRecorder(
                self.source_catalogue.clone(),
            )));
        }
        contributions.register_dynamic_tool(
            awaken_runtime_contract::plugin::DynamicTool::try_new(descriptor, tool)
                .map_err(|error| PluginConfigError::new(WEB_FETCH_PLUGIN_ID, error.to_string()))?,
        );
        Ok(contributions)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use awaken_tool_contract::tool::ToolCall;

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
            credential: Option<&Credential>,
        ) -> Result<Vec<WebSearchResult>, ToolError> {
            *self.seen_request.lock().unwrap() = Some(request.clone());
            *self.seen_secret.lock().unwrap() = credential
                .and_then(Credential::header)
                .map(|(_, value)| value);
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
            _access: &CredentialAccess,
            _provider_id: &str,
        ) -> Result<Credential, String> {
            Err("paid credential resolution reached".into())
        }
    }

    fn published_access(provider_id: &str, usage: CredentialUsage) -> CredentialAccess {
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        );
        CredentialAccess::new(
            CredentialRef {
                id: format!("cred:{provider_id}"),
                revision: 2,
            },
            awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            usage,
            awaken_runtime_contract::CredentialExecutionPolicy::exact(
                holder,
                ModelExposurePolicy::Forbidden,
            ),
        )
        .with_target(CredentialTarget::new(
            CredentialPurpose::WebProviderAuthorization,
            provider_id,
        ))
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
            _credential: Option<&Credential>,
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

    pub(super) struct FakeFetchProvider {
        pub(super) seen_requests: Mutex<Vec<WebFetchRequest>>,
        pub(super) seen_filters: Mutex<Vec<Vec<WebDomainFilter>>>,
    }

    #[tokio::test]
    async fn inherited_policy_controls_provider_dispatch_and_empty_domain_descriptions() {
        // Causes: C1 only the caller has a list; C2 own allow and caller block;
        // C3 own/caller allows are disjoint; C4 provider cannot enforce policy;
        // C5 a caller supplies location. Effects: I1 caller-only policy is active;
        // I2 allowed fetch dispatches once with every filter, blocked fetch has
        // zero additional dispatch; I3 empty intersections keep the tool with
        // an explanatory descriptor but return url_not_allowed without I/O;
        // I4 incompatible realization is refused; I5 location reaches provider
        // and blocked search results are omitted. Both allow and refusal are
        // witnessed, including an explicit unrestricted source.
        use awaken_runtime_contract::activation::InheritedToolsets;
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
        };
        let inherited = |name: &str, value| {
            InheritedToolsets::with_caller(
                vec![ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: ToolExecutionPolicy::default(),
                    overrides: vec![ToolPolicyOverride::with_optional_configuration(
                        name,
                        ToolExecutionPolicy::default(),
                        Some(value),
                    )],
                }],
                None,
            )
        };
        let provider = Arc::new(FakeFetchProvider {
            seen_requests: Mutex::new(Vec::new()),
            seen_filters: Mutex::new(Vec::new()),
        });
        let mut registry = WebSearchProviderRegistry::builtins();
        registry.register_fetch(provider.clone()).unwrap();
        let allow = inherited(
            "web_fetch",
            json!({"type":"web_fetch","domains":{"type":"allow","domains":["example.com"]}}),
        );
        let caller_only = WebFetchPlugin::new(registry.clone(), None)
            .with_inherited_execution_configuration(None, &allow)
            .unwrap();
        let (_, tool) = caller_only
            .configured_tool(Some(&json!({"provider_id":"fetch-probe","options":{}})))
            .unwrap();
        let call = |url: &str| ToolCall {
            call_id: "inherited-fetch".into(),
            tool_id: "web_fetch".into(),
            arguments: json!({"url":url}),
        };
        assert_eq!(
            tool.invoke(call("https://docs.example.com/"))
                .await
                .unwrap()
                .text(),
            "abcdef",
            "I1 allowed"
        );
        assert!(
            tool.invoke(call("https://example.net/"))
                .await
                .unwrap_err()
                .to_string()
                .contains("url_not_allowed"),
            "I1 refused"
        );
        assert_eq!(
            provider.seen_requests.lock().unwrap().len(),
            1,
            "I1 no refused dispatch"
        );
        let blocked = inherited(
            "web_fetch",
            json!({"type":"web_fetch","domains":{"type":"block","domains":["ads.example.com"]}}),
        );
        let plugin = WebFetchPlugin::new(registry.clone(), None)
            .with_inherited_execution_configuration(
                Some(WebFetchExecutionConfiguration {
                    url_sources: None,
                    domains: Some(WebDomainFilter::Allow(vec!["example.com".into()])),
                    max_content_tokens: None,
                }),
                &blocked,
            )
            .unwrap();
        let (_, tool) = plugin
            .configured_tool(Some(&json!({"provider_id":"fetch-probe","options":{}})))
            .unwrap();
        tool.invoke(call("https://docs.example.com/"))
            .await
            .unwrap();
        assert_eq!(
            provider.seen_filters.lock().unwrap().last().unwrap().len(),
            2,
            "I2 complete transport policy"
        );
        assert!(
            tool.invoke(call("https://ads.example.com/")).await.is_err(),
            "I2 blocked"
        );
        assert_eq!(
            provider.seen_requests.lock().unwrap().len(),
            2,
            "I2 no blocked I/O"
        );
        let plugin = WebFetchPlugin::new(registry.clone(), None)
            .with_inherited_execution_configuration(
                Some(WebFetchExecutionConfiguration {
                    url_sources: None,
                    domains: Some(WebDomainFilter::Allow(vec!["example.net".into()])),
                    max_content_tokens: None,
                }),
                &allow,
            )
            .unwrap();
        let (descriptor, tool) = plugin
            .configured_tool(Some(&json!({"provider_id":"fetch-probe","options":{}})))
            .unwrap();
        assert!(
            descriptor.description.contains("No domain is permitted"),
            "I3 model informed"
        );
        assert!(
            tool.invoke(call("https://example.net/"))
                .await
                .unwrap_err()
                .to_string()
                .contains("url_not_allowed"),
            "I3 error code"
        );
        assert_eq!(
            provider.seen_requests.lock().unwrap().len(),
            2,
            "I3 no dispatch"
        );
        assert!(
            caller_only
                .configured_tool(Some(&json!({"provider_id":"openrouter","options":{}})))
                .is_err(),
            "I4 caller-only restriction blocks unsupported realization"
        );
        let provider = fake_provider("inherited-search", false);
        let registry =
            WebSearchProviderRegistry::try_new([provider.clone() as Arc<dyn WebSearchProvider>])
                .unwrap();
        let search = inherited(
            "web_search",
            json!({"type":"web_search","domains":{"type":"block","domains":["result.test"]},"user_location":{"country":"SG"}}),
        );
        let plugin = WebSearchPlugin::new(registry.clone(), None)
            .with_inherited_execution_configuration(None, &search)
            .unwrap();
        let (_, tool) = plugin
            .configured_tool(Some(
                &json!({"provider_id":"inherited-search","options":{}}),
            ))
            .unwrap();
        let call = || ToolCall {
            call_id: "inherited-search".into(),
            tool_id: "web_search".into(),
            arguments: json!({"query":"policy"}),
        };
        assert_eq!(
            tool.invoke(call()).await.unwrap().text(),
            "no web-search results",
            "I5 result omitted"
        );
        assert_eq!(
            provider
                .seen_request
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .user_location
                .as_ref()
                .unwrap()
                .country
                .as_deref(),
            Some("SG"),
            "I5 inherited location"
        );
        *provider.seen_request.lock().unwrap() = None;
        let search = inherited(
            "web_search",
            json!({"type":"web_search","domains":{"type":"allow","domains":["example.com"]}}),
        );
        let plugin = WebSearchPlugin::new(registry, None)
            .with_inherited_execution_configuration(
                Some(WebSearchExecutionConfiguration {
                    domains: Some(WebDomainFilter::Allow(vec!["example.net".into()])),
                    user_location: None,
                }),
                &search,
            )
            .unwrap();
        let (descriptor, tool) = plugin
            .configured_tool(Some(
                &json!({"provider_id":"inherited-search","options":{}}),
            ))
            .unwrap();
        assert!(
            descriptor.description.contains("No domain is permitted"),
            "I3 search description"
        );
        assert!(
            tool.invoke(call())
                .await
                .unwrap_err()
                .to_string()
                .contains("url_not_allowed"),
            "I3 search error"
        );
        assert!(
            provider.seen_request.lock().unwrap().is_none(),
            "I3 no search dispatch"
        );
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

    #[test]
    fn model_provider_search_routes_are_closed_and_exact() {
        // Each advertised route resolves to one provider-bound server
        // projection. Options are deliberately closed until Awaken can prove
        // that the exact provider/dialect enforces them.
        let registry = WebSearchProviderRegistry::server_builtins();
        let plugin = WebSearchPlugin::new(registry, None);
        for (provider_id, expected) in [
            (OPENAI_PROVIDER_ID, ProviderServerTool::OpenAiWebSearch),
            (
                DEEPSEEK_RESPONSES_PROVIDER_ID,
                ProviderServerTool::DeepSeekResponsesWebSearch,
            ),
            (
                ANTHROPIC_PROVIDER_ID,
                ProviderServerTool::AnthropicWebSearch,
            ),
            (
                DEEPSEEK_ANTHROPIC_PROVIDER_ID,
                ProviderServerTool::DeepSeekAnthropicWebSearch,
            ),
            (GEMINI_PROVIDER_ID, ProviderServerTool::GeminiWebSearch),
            (VERTEX_PROVIDER_ID, ProviderServerTool::VertexWebSearch),
        ] {
            let (descriptor, _) = plugin
                .configured_tool(Some(&json!({"provider_id": provider_id, "options": {}})))
                .expect("documented native search route");
            assert_eq!(descriptor.provider_server_tool, Some(expected.clone()));
            assert!(
                plugin
                    .validate_config(Some(&json!({
                        "provider_id": provider_id,
                        "options": {"unverified": true}
                    })))
                    .is_err(),
                "unverified provider options must fail closed for {provider_id}"
            );
        }
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
            _credential: Option<&Credential>,
            domain_filters: &[WebDomainFilter],
        ) -> Result<String, ToolError> {
            self.seen_requests.lock().unwrap().push(request);
            self.seen_filters
                .lock()
                .unwrap()
                .push(domain_filters.to_vec());
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
                "credential_access": published_access(
                    "paid",
                    CredentialUsage::HttpHeader { name: "X-Key".into(), scheme: None },
                ),
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
        assert!(
            plugin
                .configured_tool(Some(&json!({
                    "provider_id": "paid",
                    "credential": { "id": "cred:paid", "revision": 2 },
                    "options": {}
                })))
                .is_err(),
            "runtime must not reconstruct policy from an authoring reference"
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
        assert!(
            !registry
                .config_schema()
                .to_string()
                .contains("x-awaken-credential-application"),
            "model-facing authoring schema must not expose injection mechanics"
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
            url_sources: None,
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
            seen_filters: Mutex::new(Vec::new()),
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
                url_sources: None,
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
    fn configured_provider_ids_are_unique() {
        // Cause/effect table: R1 none or a non-empty HTTP-header requirement is
        // one realizable provider contract; R2 any other usage or empty header
        // cannot be represented by the shared outbound wire adapter. R1 is
        // admitted and R2 fails registration before publication/runtime drift.
        assert!(
            validate_web_credential_requirement(&WebSearchCredentialRequirement::Exact(
                CredentialUsage::process_env("SECRET")
            ))
            .is_err(),
            "R2"
        );
        assert!(
            validate_web_credential_requirement(&WebSearchCredentialRequirement::Exact(
                CredentialUsage::HttpHeader {
                    name: String::new(),
                    scheme: None,
                }
            ))
            .is_err(),
            "R2"
        );
        let ids = WebSearchProviderRegistry::builtins()
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from(["brave".into(), "duckduckgo".into()]));
    }
}
