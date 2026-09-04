//! One protocol adapter for Cloud-routed WebSearch and WebFetch.

use std::io::Read;

use super::*;

/// Secret-free exact Gateway route returned by a composition-owned authority.
#[derive(Debug, Clone)]
pub struct ManagedWebGatewayEndpoint {
    pub gateway_base_url: String,
    pub route_ref: String,
    pub lease_token: awaken_runtime_contract::RedactedString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedWebRouteError {
    Forbidden,
    Invalid,
    Unavailable,
}

impl std::fmt::Display for ManagedWebRouteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Forbidden => "managed Web route is forbidden",
            Self::Invalid => "managed Web route is invalid",
            Self::Unavailable => "managed Web route resolution is unavailable",
        })
    }
}

/// Composition-owned authorization edge for one routed builtin invocation.
/// Hosted Workers and signed-in local products implement this same port with
/// different principals; neither duplicates provider HTTP behavior.
#[async_trait]
pub trait ManagedWebRouteResolver: Send + Sync {
    async fn resolve(
        &self,
        context: &awaken_runtime_contract::tool::ToolOperationContext,
        tool_id: &str,
        route_ref: &str,
    ) -> Result<ManagedWebGatewayEndpoint, ManagedWebRouteError>;
}

/// The one Gateway-routed provider adapter shared by hosted and local Cloud
/// compositions. It owns protocol normalization, not Cloud authorization.
#[derive(Clone)]
pub struct ManagedGatewayWebProvider {
    resolver: Arc<dyn ManagedWebRouteResolver>,
    search_label: String,
    search_options_schema: Value,
    search_routes: Option<std::collections::BTreeSet<String>>,
    fetch_label: String,
    fetch_options_schema: Value,
    fetch_routes: Option<std::collections::BTreeSet<String>>,
}

impl ManagedGatewayWebProvider {
    #[must_use]
    pub fn new(resolver: Arc<dyn ManagedWebRouteResolver>) -> Self {
        Self {
            resolver,
            search_label: "Awaken Cloud · Web Search".into(),
            search_options_schema: managed_search_options_schema(),
            search_routes: None,
            fetch_label: "Awaken Cloud · Web Fetch".into(),
            fetch_options_schema: managed_fetch_options_schema(),
            fetch_routes: None,
        }
    }

    /// Bind discovery-derived route choices into the same descriptors used by
    /// validation and authoring. Cloud discovery owns which routes are offered.
    pub fn with_descriptors(
        mut self,
        search_label: impl Into<String>,
        search_options_schema: Value,
        search_routes: impl IntoIterator<Item = String>,
        fetch_label: impl Into<String>,
        fetch_options_schema: Value,
        fetch_routes: impl IntoIterator<Item = String>,
    ) -> Self {
        self.search_label = search_label.into();
        self.search_options_schema = search_options_schema;
        self.search_routes = Some(search_routes.into_iter().collect());
        self.fetch_label = fetch_label.into();
        self.fetch_options_schema = fetch_options_schema;
        self.fetch_routes = Some(fetch_routes.into_iter().collect());
        self
    }

    #[must_use]
    pub fn registry(self) -> WebSearchProviderRegistry {
        let mut registry = WebSearchProviderRegistry::server_builtins();
        registry
            .register(Arc::new(self.clone()))
            .expect("managed WebSearch descriptor is valid and unique");
        registry
            .register_fetch(Arc::new(self))
            .expect("managed WebFetch descriptor is valid and unique");
        registry
    }
}

fn managed_search_options_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "route_ref": { "type": "string", "minLength": 1 },
            "country": { "type": "string", "minLength": 2, "maxLength": 2 },
            "search_lang": { "type": "string", "minLength": 2 },
            "safesearch": { "type": "string", "enum": ["off", "moderate", "strict"] }
        },
        "required": ["route_ref"],
        "additionalProperties": false
    })
}

fn managed_fetch_options_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "route_ref": { "type": "string", "minLength": 1 },
            "max_characters": { "type": "integer", "minimum": 1, "maximum": 1048576 }
        },
        "required": ["route_ref"],
        "additionalProperties": false
    })
}

/// Read one immutable Cloud Gateway route from provider-owned options while
/// preserving the builtin tool namespace. Composition crates reuse this
/// parser instead of maintaining a second route grammar.
#[must_use]
pub fn managed_web_route_ref<'a>(tool_id: &str, options: &'a Value) -> Option<&'a str> {
    let prefix = match tool_id {
        WEB_SEARCH_PLUGIN_ID => "web-search:",
        WEB_FETCH_PLUGIN_ID => "web-fetch:",
        _ => return None,
    };
    options
        .get("route_ref")
        .and_then(Value::as_str)
        .filter(|value| {
            value.starts_with(prefix)
                && value.len() > prefix.len()
                && !value.bytes().any(|byte| byte.is_ascii_whitespace())
        })
}

fn managed_endpoint_url(endpoint: &ManagedWebGatewayEndpoint) -> Result<url::Url, ToolError> {
    let mut url = url::Url::parse(&endpoint.gateway_base_url)
        .map_err(|_| ToolError::Execution("managed Web Gateway URL is invalid".into()))?;
    url.path_segments_mut()
        .map_err(|_| ToolError::Execution("managed Web Gateway URL is not hierarchical".into()))?
        .extend(["http", endpoint.route_ref.as_str()]);
    Ok(url)
}

async fn resolve_managed_route(
    resolver: &dyn ManagedWebRouteResolver,
    tool_id: &str,
    options: &Value,
) -> Result<ManagedWebGatewayEndpoint, ToolError> {
    let route_ref = managed_web_route_ref(tool_id, options)
        .ok_or_else(|| ToolError::Execution("managed Web route is not published".into()))?;
    let context = awaken_runtime_contract::tool::current_tool_operation_context()
        .ok_or_else(|| ToolError::Execution("managed Web route requires an active Run".into()))?;
    resolver
        .resolve(&context, tool_id, route_ref)
        .await
        .map_err(|error| match error {
            ManagedWebRouteError::Unavailable => {
                ToolError::UnavailableBeforeDispatch(error.to_string())
            }
            ManagedWebRouteError::Forbidden | ManagedWebRouteError::Invalid => {
                ToolError::Execution(error.to_string())
            }
        })
}

#[derive(Deserialize)]
struct ManagedSearchResult {
    title: String,
    url: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize, Default)]
struct ManagedSearchWeb {
    #[serde(default)]
    results: Vec<ManagedSearchResult>,
}

#[derive(Deserialize)]
struct ManagedSearchResponse {
    #[serde(default)]
    web: ManagedSearchWeb,
}

#[async_trait]
impl WebSearchProvider for ManagedGatewayWebProvider {
    fn descriptor(&self) -> WebSearchProviderDescriptor {
        WebSearchProviderDescriptor {
            id: AWAKEN_CLOUD_PROVIDER_ID.into(),
            label: self.search_label.clone(),
            credential: WebSearchCredentialRequirement::None,
            options_schema: self.search_options_schema.clone(),
        }
    }

    fn validate_options(&self, options: &Value) -> Result<(), String> {
        let route = managed_web_route_ref(WEB_SEARCH_PLUGIN_ID, options)
            .ok_or_else(|| "managed WebSearch requires an immutable web-search route".to_owned())?;
        if self
            .search_routes
            .as_ref()
            .is_some_and(|routes| !routes.contains(route))
        {
            return Err("managed WebSearch route is not in the discovered catalog".into());
        }
        Ok(())
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        credential: Option<&Credential>,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        if credential.is_some() {
            return Err(ToolError::Execution(
                "managed WebSearch must not receive plaintext credentials".into(),
            ));
        }
        let endpoint = resolve_managed_route(
            self.resolver.as_ref(),
            WEB_SEARCH_PLUGIN_ID,
            &request.options,
        )
        .await?;
        let mut url = managed_endpoint_url(&endpoint)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("q", &request.query);
            query.append_pair("count", &request.count.to_string());
            for option in ["country", "search_lang", "safesearch"] {
                if let Some(value) = request.options.get(option).and_then(Value::as_str) {
                    query.append_pair(option, value);
                }
            }
        }
        let token = endpoint.lease_token.expose_secret().to_owned();
        blocking(move || {
            let response: ManagedSearchResponse = ureq::get(url.as_str())
                .set("Authorization", &format!("Bearer {token}"))
                .call()
                .map_err(|error| ToolError::Execution(format!("managed WebSearch: {error}")))?
                .into_json()
                .map_err(|_| {
                    ToolError::Execution("managed WebSearch returned an invalid response".into())
                })?;
            Ok(response
                .web
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

#[async_trait]
impl WebFetchProvider for ManagedGatewayWebProvider {
    fn descriptor(&self) -> WebFetchProviderDescriptor {
        WebFetchProviderDescriptor {
            id: AWAKEN_CLOUD_PROVIDER_ID.into(),
            label: self.fetch_label.clone(),
            credential: WebSearchCredentialRequirement::None,
            options_schema: self.fetch_options_schema.clone(),
        }
    }

    fn validate_options(&self, options: &Value) -> Result<(), String> {
        let route = managed_web_route_ref(WEB_FETCH_PLUGIN_ID, options)
            .ok_or_else(|| "managed WebFetch requires an immutable web-fetch route".to_owned())?;
        if self
            .fetch_routes
            .as_ref()
            .is_some_and(|routes| !routes.contains(route))
        {
            return Err("managed WebFetch route is not in the discovered catalog".into());
        }
        Ok(())
    }

    async fn fetch(
        &self,
        request: WebFetchRequest,
        credential: Option<&Credential>,
        domain_filter: Option<&WebDomainFilter>,
    ) -> Result<String, ToolError> {
        if domain_filter.is_some() {
            return Err(ToolError::Execution(
                "managed WebFetch cannot enforce target redirect domain policy".into(),
            ));
        }
        if credential.is_some() {
            return Err(ToolError::Execution(
                "managed WebFetch must not receive plaintext credentials".into(),
            ));
        }
        let endpoint = resolve_managed_route(
            self.resolver.as_ref(),
            WEB_FETCH_PLUGIN_ID,
            &request.options,
        )
        .await?;
        let mut url = managed_endpoint_url(&endpoint)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("url", &request.url);
            if let Some(maximum) = request
                .options
                .get("max_characters")
                .and_then(Value::as_u64)
            {
                query.append_pair("max_characters", &maximum.to_string());
            }
        }
        let token = endpoint.lease_token.expose_secret().to_owned();
        blocking(move || {
            let response = ureq::get(url.as_str())
                .set("Authorization", &format!("Bearer {token}"))
                .call()
                .map_err(|error| ToolError::Execution(format!("managed WebFetch: {error}")))?;
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(MAX_BODY)
                .read_to_end(&mut bytes)
                .map_err(|error| ToolError::Execution(format!("read managed WebFetch: {error}")))?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FixedManagedResolver {
        gateway_base_url: String,
        seen: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl ManagedWebRouteResolver for FixedManagedResolver {
        async fn resolve(
            &self,
            context: &awaken_runtime_contract::tool::ToolOperationContext,
            tool_id: &str,
            route_ref: &str,
        ) -> Result<ManagedWebGatewayEndpoint, ManagedWebRouteError> {
            self.seen.lock().unwrap().push((
                context.operation_id.clone(),
                tool_id.to_owned(),
                route_ref.to_owned(),
            ));
            Ok(ManagedWebGatewayEndpoint {
                gateway_base_url: self.gateway_base_url.clone(),
                route_ref: route_ref.to_owned(),
                lease_token: awaken_runtime_contract::RedactedString::new("route-capability"),
            })
        }
    }

    #[tokio::test]
    async fn routes_both_builtins_with_operation_identity() {
        // Cause/effect graph: C1 Runtime operation context + C2 exact
        // tool-prefixed route + C3 resolver capability + C4 no plaintext
        // credential => E1 resolver sees operation/tool/route, E2 one bearer
        // Gateway request, E3 normalized output.
        // Decision rules: G1 search+exact+no credential -> results; G2
        // fetch+exact+no credential -> text; G3 wrong prefix -> pre-I/O error;
        // G4 plaintext credential -> pre-I/O error; G5 Agent fetch domain
        // policy + Gateway topology that cannot observe target redirects ->
        // configuration fails before resolver or Gateway I/O.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let gateway_base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let server = std::thread::spawn(move || {
            for body in [
                r#"{"web":{"results":[{"title":"Result","url":"https://result.test","description":"Found"}]}}"#,
                "Fetched content",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let size = std::io::Read::read(&mut stream, &mut request).unwrap();
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..size]).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
            }
        });
        let resolver = Arc::new(FixedManagedResolver {
            gateway_base_url,
            seen: Mutex::new(Vec::new()),
        });
        let provider = ManagedGatewayWebProvider::new(resolver.clone());
        assert!(
            WebFetchPlugin::new(provider.clone().registry(), None)
                .with_execution_configuration(Some(WebFetchExecutionConfiguration {
                    domains: Some(WebDomainFilter::Allow(vec!["docs.example".into()])),
                    max_content_tokens: None,
                }))
                .validate_config(Some(&json!({
                    "provider_id": AWAKEN_CLOUD_PROVIDER_ID,
                    "options": {"route_ref":"web-fetch:direct@3"}
                })))
                .is_err(),
            "G5"
        );

        let search = awaken_runtime_contract::tool::with_tool_operation_context(
            awaken_runtime_contract::tool::ToolOperationContext::for_run("run-search", "op-search"),
            provider.search(
                WebSearchRequest {
                    query: "bounded contexts".into(),
                    count: 3,
                    options: json!({"route_ref":"web-search:brave@7"}),
                    user_location: None,
                },
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(search[0].snippet, "Found", "G1");

        let fetched = awaken_runtime_contract::tool::with_tool_operation_context(
            awaken_runtime_contract::tool::ToolOperationContext::for_run("run-fetch", "op-fetch"),
            provider.fetch(
                WebFetchRequest {
                    url: "https://docs.example/page".into(),
                    options: json!({"route_ref":"web-fetch:direct@3"}),
                },
                None,
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(fetched, "Fetched content", "G2");
        server.join().unwrap();

        assert_eq!(
            resolver.seen.lock().unwrap().as_slice(),
            [
                (
                    "op-search".into(),
                    WEB_SEARCH_PLUGIN_ID.into(),
                    "web-search:brave@7".into()
                ),
                (
                    "op-fetch".into(),
                    WEB_FETCH_PLUGIN_ID.into(),
                    "web-fetch:direct@3".into()
                ),
            ],
            "G1/G2"
        );
        for request in requests.lock().unwrap().iter() {
            assert!(
                request.contains("Authorization: Bearer route-capability"),
                "G1/G2"
            );
        }
        assert!(
            WebSearchProvider::validate_options(&provider, &json!({"route_ref":"web-fetch:wrong"}))
                .is_err(),
            "G3"
        );
        assert!(
            provider
                .search(
                    WebSearchRequest {
                        query: "secret".into(),
                        count: 1,
                        options: json!({"route_ref":"web-search:brave@7"}),
                        user_location: None,
                    },
                    Some(&Credential::Header {
                        name: "X-Forbidden".into(),
                        value: "must-not-pass".into(),
                    }),
                )
                .await
                .is_err(),
            "G4"
        );
    }
}
