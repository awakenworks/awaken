//! Network hand tools (ADR-0007): `web_fetch` (HTTP GET) and `web_search`
//! (DuckDuckGo Instant Answer API, no key required). The blocking HTTP call runs
//! on a blocking thread so the async loop is not stalled.

use std::io::Read;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError};
use serde::Deserialize;

use crate::erasure::erase;

/// Cap on fetched bytes so a huge response cannot blow up the transcript.
const MAX_BODY: u64 = 1 << 20; // 1 MiB

/// Run a blocking closure on Tokio's blocking pool, mapping a join failure to a
/// typed error.
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

/// Search the web via DuckDuckGo's Instant Answer API and return a text summary.
pub struct WebSearchTool;

#[derive(Deserialize)]
pub struct WebSearchArgs {
    pub query: String,
}

/// The subset of the DuckDuckGo Instant Answer response we render. Strongly typed
/// rather than `Value` so the shape we depend on is explicit.
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

fn render_search(response: &DdgResponse) -> String {
    let mut out = String::new();
    if !response.heading.is_empty() {
        out.push_str(&response.heading);
        out.push('\n');
    }
    if !response.abstract_text.is_empty() {
        out.push_str(&response.abstract_text);
        if !response.abstract_url.is_empty() {
            out.push_str(&format!(" ({})", response.abstract_url));
        }
        out.push('\n');
    }
    for topic in response
        .related_topics
        .iter()
        .filter(|t| !t.text.is_empty())
        .take(8)
    {
        out.push_str("- ");
        out.push_str(&topic.text);
        if !topic.first_url.is_empty() {
            out.push_str(&format!(" ({})", topic.first_url));
        }
        out.push('\n');
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        "no instant-answer results".to_string()
    } else {
        trimmed.to_string()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    type Args = WebSearchArgs;
    type Output = String;
    fn id(&self) -> &str {
        "web_search"
    }
    async fn call(&self, args: WebSearchArgs) -> Result<String, ToolError> {
        blocking(move || {
            let response: DdgResponse = ureq::get("https://api.duckduckgo.com/")
                .query("q", &args.query)
                .query("format", "json")
                .query("no_html", "1")
                .query("no_redirect", "1")
                .call()
                .map_err(|err| ToolError::Execution(format!("search: {err}")))?
                .into_json()
                .map_err(|err| ToolError::Execution(format!("parse search: {err}")))?;
            Ok(render_search(&response))
        })
        .await
    }
}

/// The network hand tools, erased for `Runtime::with_tool` registration.
pub fn web_hand_tools() -> Vec<Arc<dyn RawTool>> {
    vec![erase(WebFetchTool), erase(WebSearchTool)]
}
