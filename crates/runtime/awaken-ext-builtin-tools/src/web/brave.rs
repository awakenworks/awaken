//! Brave Search provider adapter. The provider owns only its descriptor and
//! wire protocol; Vault lookup and policy remain outside this module.

use async_trait::async_trait;
use awaken_credential::Credential;
use awaken_runtime_contract::{CredentialUsage, tool::ToolError};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    BRAVE_PROVIDER_ID, WebSearchCredentialRequirement, WebSearchProvider,
    WebSearchProviderDescriptor, WebSearchRequest, WebSearchResult, blocking,
};

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

fn apply_http_credential(
    request: ureq::Request,
    credential: &Credential,
) -> Result<ureq::Request, ToolError> {
    let (name, value) = credential
        .header()
        .ok_or_else(|| ToolError::Execution("outbound HTTP credential is missing".into()))?;
    Ok(request.set(&name, &value))
}

async fn execute_brave_search(
    endpoint: String,
    request: WebSearchRequest,
    credential: Credential,
) -> Result<Vec<WebSearchResult>, ToolError> {
    blocking(move || {
        let mut call = apply_http_credential(
            ureq::get(&endpoint).set("Accept", "application/json"),
            &credential,
        )?
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
            .or_else(|| request.options.get("country").and_then(Value::as_str))
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
        credential: Option<&Credential>,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        let credential = credential
            .ok_or_else(|| ToolError::Execution("Brave Search API key is missing".into()))?
            .clone();
        execute_brave_search(
            "https://api.search.brave.com/res/v1/web/search".into(),
            request,
            credential,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[tokio::test]
    async fn published_wire_credential_has_no_second_header_rule() {
        // Cause/effect graph: C1 resolver-produced exact custom-header
        // credential; C2 Brave request; C3 successful upstream response.
        // Effects: E1 exactly one descriptor-owned X-Subscription-Token on the
        // wire; E2 no Authorization fallback; E3 normalized output contains no
        // credential. Rule B1=C1+C2+C3=>E1+E2+E3. The local socket verifies the
        // real ureq request rather than a mock Provider implementation.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/search", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(String::new()));
        let request_capture = captured.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let size = std::io::Read::read(&mut stream, &mut request).unwrap();
            *request_capture.lock().unwrap() =
                String::from_utf8_lossy(&request[..size]).into_owned();
            let body = r#"{"web":{"results":[{"title":"Result","url":"https://result.test","description":"Found"}]}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
        });
        let results = execute_brave_search(
            endpoint,
            WebSearchRequest {
                query: "water cycle".into(),
                count: 3,
                options: json!({"safesearch":"strict"}),
                user_location: None,
            },
            Credential::Header {
                name: "X-Subscription-Token".into(),
                value: "vault-key".into(),
            },
        )
        .await
        .unwrap();
        server.join().unwrap();
        let request = captured.lock().unwrap().clone();
        assert_eq!(
            request.matches("X-Subscription-Token: vault-key").count(),
            1,
            "B1/E1: {request}"
        );
        assert!(!request.contains("Authorization:"), "B1/E2: {request}");
        let output = super::super::render_results(&results);
        assert!(output.contains("https://result.test"), "B1/E3");
        assert!(!output.contains("vault-key"), "B1/E3");
    }
}
