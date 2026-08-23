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

    async fn fetch(
        &self,
        request: WebFetchRequest,
        _credential: Option<&CredentialMaterial>,
    ) -> Result<String, ToolError> {
        blocking(move || {
            let response = ureq::get(&request.url)
                .call()
                .map_err(|err| ToolError::Execution(format!("fetch {}: {err}", request.url)))?;
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

pub(super) fn openrouter_server_search_descriptor() -> WebServerToolProviderDescriptor {
    WebServerToolProviderDescriptor {
        id: OPENROUTER_PROVIDER_ID.into(),
        label: "OpenRouter server search".into(),
        provider_kind: OPENROUTER_PROVIDER_ID.into(),
        tool_type: "openrouter:web_search".into(),
        options_schema: json!({
            "type": "object",
            "properties": {
                "engine": { "type": "string", "enum": ["auto", "native", "exa", "firecrawl", "parallel", "perplexity"] },
                "max_results": { "type": "integer", "minimum": 1 },
                "max_uses": { "type": "integer", "minimum": 1 },
                "search_context_size": { "type": "string", "enum": ["low", "medium", "high"] }
            },
            "additionalProperties": true
        }),
    }
}

pub(super) fn openrouter_server_fetch_descriptor() -> WebServerToolProviderDescriptor {
    WebServerToolProviderDescriptor {
        id: OPENROUTER_PROVIDER_ID.into(),
        label: "OpenRouter server fetch".into(),
        provider_kind: OPENROUTER_PROVIDER_ID.into(),
        tool_type: "openrouter:web_fetch".into(),
        options_schema: json!({
            "type": "object",
            "properties": {
                "engine": { "type": "string", "enum": ["auto", "native", "exa", "openrouter", "firecrawl", "parallel"] },
                "max_uses": { "type": "integer", "minimum": 1 },
                "max_content_tokens": { "type": "integer", "minimum": 1 }
            },
            "additionalProperties": true
        }),
    }
}
