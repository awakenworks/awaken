//! Direct and provider-server Web provider adapters.

use std::io::Read;

use super::*;

pub(super) struct ProviderServerWebFetchTool;

#[async_trait]
impl Tool for ProviderServerWebFetchTool {
    type Args = WebFetchArgs;
    type Output = String;
    const ID: &'static str = WEB_FETCH_TOOL_ID;
    const DESCRIPTION: &'static str = "Fetch a URL through the selected model provider";

    async fn call(&self, _args: WebFetchArgs) -> Result<String, ToolError> {
        Err(ToolError::Execution(
            "provider-server WebFetch reached host execution".into(),
        ))
    }
}

pub struct AwakenDirectFetchProvider;

fn read_fetch_body(response: ureq::Response) -> Result<String, ToolError> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BODY)
        .read_to_end(&mut bytes)
        .map_err(|err| ToolError::Execution(format!("read body: {err}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[async_trait]
impl WebFetchProvider for AwakenDirectFetchProvider {
    fn descriptor(&self) -> WebFetchProviderDescriptor {
        WebFetchProviderDescriptor {
            id: AWAKEN_DIRECT_PROVIDER_ID.into(),
            label: "Direct HTTP".into(),
            credential: WebSearchCredentialRequirement::None,
            options_schema: json!({ "type": "object", "additionalProperties": false }),
        }
    }

    fn enforces_domain_filter(&self) -> bool {
        true
    }

    async fn fetch(
        &self,
        request: WebFetchRequest,
        _credential: Option<&Credential>,
        domain_filter: Option<&WebDomainFilter>,
    ) -> Result<String, ToolError> {
        let filter_active = domain_filter.is_some();
        blocking(move || {
            let call = if filter_active {
                ureq::AgentBuilder::new()
                    .redirects(0)
                    .build()
                    .get(&request.url)
            } else {
                ureq::get(&request.url)
            };
            let response = call
                .call()
                .map_err(|err| ToolError::Execution(format!("fetch {}: {err}", request.url)))?;
            // The configured plugin matched the initial URL with the canonical
            // filter. Disabling redirects makes 3xx observable before any
            // unvalidated second request can escape.
            if filter_active && (300..400).contains(&response.status()) {
                return Err(ToolError::Execution(
                    "web_fetch redirect is disabled while domain policy is active".into(),
                ));
            }
            read_fetch_body(response)
        })
        .await
    }
}

pub(super) fn openrouter_server_search_descriptor() -> WebServerToolProviderDescriptor {
    WebServerToolProviderDescriptor {
        id: OPENROUTER_PROVIDER_ID.into(),
        label: "OpenRouter server search".into(),
        options_schema: json!({
            "type": "object",
            "properties": {
                "engine": { "type": "string", "enum": ["auto", "native", "exa", "firecrawl", "parallel", "perplexity"] },
                "max_results": { "type": "integer", "minimum": 1 },
                "max_total_results": { "type": "integer", "minimum": 1 },
                "search_context_size": { "type": "string", "enum": ["low", "medium", "high"] }
            },
            "additionalProperties": false
        }),
    }
}

pub(super) fn native_server_search_descriptors() -> Vec<WebServerToolProviderDescriptor> {
    [
        (OPENAI_PROVIDER_ID, "OpenAI hosted search"),
        (
            DEEPSEEK_RESPONSES_PROVIDER_ID,
            "DeepSeek Responses hosted search",
        ),
        (ANTHROPIC_PROVIDER_ID, "Anthropic server search"),
        (
            DEEPSEEK_ANTHROPIC_PROVIDER_ID,
            "DeepSeek Anthropic server search",
        ),
        (GEMINI_PROVIDER_ID, "Gemini Google Search grounding"),
        (VERTEX_PROVIDER_ID, "Vertex Google Search grounding"),
    ]
    .into_iter()
    .map(|(id, label)| WebServerToolProviderDescriptor {
        id: id.into(),
        label: label.into(),
        options_schema: json!({ "type": "object", "additionalProperties": false }),
    })
    .collect()
}

pub(super) fn openrouter_server_fetch_descriptor() -> WebServerToolProviderDescriptor {
    WebServerToolProviderDescriptor {
        id: OPENROUTER_PROVIDER_ID.into(),
        label: "OpenRouter server fetch".into(),
        options_schema: json!({
            "type": "object",
            "properties": {
                "engine": { "type": "string", "enum": ["auto", "native", "exa", "openrouter", "firecrawl", "parallel"] },
                "max_uses": { "type": "integer", "minimum": 1 },
                "max_content_tokens": { "type": "integer", "minimum": 1 }
            },
            "additionalProperties": false
        }),
    }
}
