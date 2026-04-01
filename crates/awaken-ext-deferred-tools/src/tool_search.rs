//! ToolSearch tool: searches the deferred tool registry by name, id, or keyword.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};

use crate::state::{DeferralRegistry, DeferralRegistryValue, StoredToolDescriptor};

pub const TOOL_SEARCH_ID: &str = "ToolSearch";

/// Query variants for the ToolSearch tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSearchQuery {
    /// Select specific tools by exact ID. Parsed from `"select:tool1,tool2"`.
    Select(Vec<String>),
    /// Keyword search with a required term. Parsed from `"+required rest terms"`.
    RequiredKeyword { required: String, rest: Vec<String> },
    /// General keyword search. Default when no prefix is present.
    Keywords(Vec<String>),
}

impl ToolSearchQuery {
    /// Parse a raw query string into a `ToolSearchQuery`.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();

        if let Some(rest) = raw.strip_prefix("select:") {
            let ids = rest
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return Self::Select(ids);
        }

        if let Some(rest) = raw.strip_prefix('+') {
            let mut parts = rest.split_whitespace();
            if let Some(required) = parts.next() {
                let rest_terms: Vec<String> = parts.map(str::to_string).collect();
                return Self::RequiredKeyword {
                    required: required.to_string(),
                    rest: rest_terms,
                };
            }
        }

        let keywords = raw
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        Self::Keywords(keywords)
    }
}

/// Count how many keywords appear (case-insensitively) in the tool's id, name, or description.
pub fn keyword_score(tool: &StoredToolDescriptor, keywords: &[String]) -> usize {
    let haystack = format!(
        "{} {} {}",
        tool.id.to_lowercase(),
        tool.name.to_lowercase(),
        tool.description.to_lowercase()
    );
    keywords
        .iter()
        .filter(|kw| haystack.contains(kw.to_lowercase().as_str()))
        .count()
}

/// Search the registry and return up to `max_results` matching tool descriptors.
///
/// - `Select`: returns tools found by exact id, in the order listed (skips unknown).
/// - `RequiredKeyword`: filters to tools matching `required`, then sorts by keyword score.
/// - `Keywords`: scores all tools and returns those with score > 0, sorted descending.
pub fn search_registry<'a>(
    query: &ToolSearchQuery,
    registry: &'a DeferralRegistryValue,
    max_results: usize,
) -> Vec<&'a StoredToolDescriptor> {
    match query {
        ToolSearchQuery::Select(ids) => ids
            .iter()
            .filter_map(|id| registry.tools.get(id.as_str()))
            .take(max_results)
            .collect(),

        ToolSearchQuery::RequiredKeyword { required, rest } => {
            let req_lower = required.to_lowercase();
            let mut matches: Vec<(&StoredToolDescriptor, usize)> = registry
                .tools
                .values()
                .filter(|tool| {
                    let haystack = format!(
                        "{} {} {}",
                        tool.id.to_lowercase(),
                        tool.name.to_lowercase(),
                        tool.description.to_lowercase()
                    );
                    haystack.contains(&req_lower)
                })
                .map(|tool| {
                    let all_kw: Vec<String> = std::iter::once(required.clone())
                        .chain(rest.iter().cloned())
                        .collect();
                    let score = keyword_score(tool, &all_kw);
                    (tool, score)
                })
                .collect();

            matches.sort_by(|a, b| b.1.cmp(&a.1));
            matches
                .into_iter()
                .take(max_results)
                .map(|(tool, _)| tool)
                .collect()
        }

        ToolSearchQuery::Keywords(keywords) => {
            let mut matches: Vec<(&StoredToolDescriptor, usize)> = registry
                .tools
                .values()
                .filter_map(|tool| {
                    let score = keyword_score(tool, keywords);
                    if score > 0 { Some((tool, score)) } else { None }
                })
                .collect();

            matches.sort_by(|a, b| b.1.cmp(&a.1));
            matches
                .into_iter()
                .take(max_results)
                .map(|(tool, _)| tool)
                .collect()
        }
    }
}

/// Format search results as `<functions>...</functions>` XML.
///
/// Returns the formatted string and a list of tool IDs to promote.
pub fn format_search_results(results: &[&StoredToolDescriptor]) -> (String, Vec<String>) {
    if results.is_empty() {
        return (
            "No matching tools found in the deferred tool registry.".to_string(),
            vec![],
        );
    }

    let mut parts = Vec::with_capacity(results.len());
    let mut promote_ids = Vec::with_capacity(results.len());

    for tool in results {
        promote_ids.push(tool.id.clone());
        let description =
            serde_json::to_string(&tool.description).unwrap_or_else(|_| "\"\"".to_string());
        let name = serde_json::to_string(&tool.name).unwrap_or_else(|_| "\"\"".to_string());
        let entry = format!(
            "<function>{{\"description\": {description}, \"name\": {name}, \"parameters\": {}}}</function>",
            tool.parameters,
        );
        parts.push(entry);
    }

    let formatted = format!("<functions>\n{}\n</functions>", parts.join("\n"));
    (formatted, promote_ids)
}

/// Arguments for the ToolSearch tool call.
#[derive(Deserialize)]
struct ToolSearchArgs {
    query: String,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_max_results() -> usize {
    5
}

/// The ToolSearch tool — searches the deferred tool registry and promotes results.
pub struct ToolSearchTool;

#[async_trait]
impl Tool for ToolSearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            TOOL_SEARCH_ID,
            "ToolSearch",
            "Fetches full schema definitions for deferred tools so they can be called. \
             Use \"select:Tool1,Tool2\" to fetch specific tools by name, \
             \"+required rest\" to require a keyword while ranking by others, \
             or plain keywords for general search.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. Formats: \"select:Tool1,Tool2\", \"+required rest terms\", or plain keywords."
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum number of tools to return.",
                    "default": 5
                }
            },
            "required": ["query"]
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let parsed: ToolSearchArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid arguments: {e}")))?;

        let registry = ctx.state::<DeferralRegistry>();
        let empty_registry = DeferralRegistryValue::default();
        let registry = registry.unwrap_or(&empty_registry);

        let query = ToolSearchQuery::parse(&parsed.query);
        let results = search_registry(&query, registry, parsed.max_results);
        let (formatted, promote_ids) = format_search_results(&results);

        Ok(ToolResult::success(
            TOOL_SEARCH_ID,
            json!({
                "tools": formatted,
                "__promote": promote_ids,
            }),
        )
        .into())
    }
}
