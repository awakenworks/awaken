// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Complete bounded tools/list discovery for both MCP client transports.
use std::{collections::HashSet, future::Future, time::Duration};

use awaken_mcp_wire::{ListToolsParams, ListToolsResult, McpToolDefinition, McpTransportError};
use serde_json::Value;

const MAX_PAGES: usize = 64;
const MAX_TOOLS: usize = 4_096;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

pub(crate) async fn list_all<F>(
    budget: Duration,
    mut request: impl FnMut(Value) -> F,
) -> Result<Vec<McpToolDefinition>, McpTransportError>
where
    F: Future<Output = Result<Value, McpTransportError>>,
{
    tokio::time::timeout(budget, async {
        let mut tools = Vec::new();
        let mut seen_names = HashSet::new();
        let mut seen_cursors = HashSet::new();
        let mut cursor = None;
        let mut bytes = 0;
        for _ in 0..MAX_PAGES {
            let page = request(serde_json::to_value(ListToolsParams { cursor })?).await?;
            bytes += serde_json::to_vec(&page)?.len();
            if bytes > MAX_CATALOG_BYTES {
                return Err(invalid("exceeded the catalog byte limit"));
            }
            let page: ListToolsResult = serde_json::from_value(page)?;
            if tools.len() + page.tools.len() > MAX_TOOLS {
                return Err(invalid("exceeded the tool count limit"));
            }
            for tool in page.tools {
                if tool.name.trim().is_empty()
                    || !tool.input_schema.is_object()
                    || tool
                        .output_schema
                        .as_ref()
                        .is_some_and(|schema| !schema.is_object())
                    || !seen_names.insert(tool.name.clone())
                {
                    return Err(invalid("invalid or duplicate tool definition"));
                }
                tools.push(tool);
            }
            let Some(next) = page.next_cursor else {
                return Ok(tools);
            };
            if next.is_empty() || !seen_cursors.insert(next.clone()) {
                return Err(invalid("empty or repeated pagination cursor"));
            }
            cursor = Some(next);
        }
        Err(invalid("exceeded the page limit"))
    })
    .await
    .map_err(|_| McpTransportError::Timeout("tools/list discovery deadline".into()))?
}

fn invalid(message: &str) -> McpTransportError {
    McpTransportError::ProtocolError(format!("tools/list: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::future::{pending, ready};

    #[tokio::test]
    async fn complete_catalog_uses_opaque_cursor_and_accepts_empty_pages() {
        let mut requests = Vec::new();
        let tools = list_all(Duration::from_secs(1), |params| {
            let page = match requests.len() {
                0 => json!({"tools": [{"name": "first"}], "nextCursor": "a/+"}),
                1 => json!({"tools": [], "nextCursor": "b"}),
                2 => json!({"tools": [{"name": "last"}]}),
                _ => panic!("list continued after the last page"),
            };
            requests.push(params);
            ready(Ok(page))
        })
        .await
        .unwrap();
        assert_eq!(
            requests,
            [json!({}), json!({"cursor": "a/+"}), json!({"cursor": "b"})]
        );
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "last"]
        );
    }

    #[tokio::test]
    async fn page_failures_never_return_a_partial_catalog() {
        let first = json!({"tools": [{"name": "first"}], "nextCursor": "a"});
        for next in [
            Ok(json!({"tools": [{"name": "first"}]})),
            Ok(json!({"tools": [], "nextCursor": "a"})),
            Ok(json!({"tools": [{"name": "invalid", "inputSchema": null}]})),
            Err(McpTransportError::ConnectionClosed),
        ] {
            let mut pages = [Ok(first.clone()), next].into_iter();
            assert!(
                list_all(Duration::from_secs(1), |_| ready(pages.next().unwrap()))
                    .await
                    .is_err()
            );
        }
        let tools = list_all(Duration::from_secs(1), |_| ready(Ok(json!({"tools": []}))))
            .await
            .unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_uses_one_deadline_for_all_pages() {
        let mut page = 0;
        let result = list_all(Duration::from_secs(1), |_| {
            page += 1;
            let page = page;
            async move {
                if page == 1 {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    Ok(json!({"tools": [], "nextCursor": "a"}))
                } else {
                    pending().await
                }
            }
        })
        .await;
        assert_eq!(page, 2);
        assert!(matches!(result, Err(McpTransportError::Timeout(_))));
    }
}
