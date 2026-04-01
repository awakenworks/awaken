use serde_json::json;

use crate::state::{DeferralRegistryValue, StoredToolDescriptor};
use crate::tool_search::{ToolSearchQuery, format_search_results, keyword_score, search_registry};

fn make_descriptor(id: &str, name: &str, description: &str) -> StoredToolDescriptor {
    StoredToolDescriptor {
        id: id.into(),
        name: name.into(),
        description: description.into(),
        parameters: json!({"type": "object", "properties": {}}),
        category: None,
    }
}

fn make_registry() -> DeferralRegistryValue {
    let mut registry = DeferralRegistryValue::default();
    let tools = vec![
        make_descriptor(
            "mcp__slack__read_channel",
            "mcp__slack__read_channel",
            "Read messages from a Slack channel",
        ),
        make_descriptor(
            "mcp__slack__send_message",
            "mcp__slack__send_message",
            "Send a message to a Slack channel",
        ),
        make_descriptor(
            "NotebookEdit",
            "NotebookEdit",
            "Edit Jupyter notebook cells",
        ),
        make_descriptor("WebSearch", "WebSearch", "Search the web for information"),
    ];
    for tool in tools {
        registry.tools.insert(tool.id.clone(), tool);
    }
    registry
}

// --- Parse tests ---

#[test]
fn parse_select_single() {
    let q = ToolSearchQuery::parse("select:NotebookEdit");
    assert_eq!(q, ToolSearchQuery::Select(vec!["NotebookEdit".into()]));
}

#[test]
fn parse_select_batch() {
    let q = ToolSearchQuery::parse("select:Read,Edit,Write");
    assert_eq!(
        q,
        ToolSearchQuery::Select(vec!["Read".into(), "Edit".into(), "Write".into()])
    );
}

#[test]
fn parse_required_keyword() {
    let q = ToolSearchQuery::parse("+slack send message");
    assert_eq!(
        q,
        ToolSearchQuery::RequiredKeyword {
            required: "slack".into(),
            rest: vec!["send".into(), "message".into()],
        }
    );
}

#[test]
fn parse_keyword_search() {
    let q = ToolSearchQuery::parse("notebook jupyter");
    assert_eq!(
        q,
        ToolSearchQuery::Keywords(vec!["notebook".into(), "jupyter".into()])
    );
}

#[test]
fn parse_single_keyword() {
    let q = ToolSearchQuery::parse("slack");
    assert_eq!(q, ToolSearchQuery::Keywords(vec!["slack".into()]));
}

// --- Search tests ---

#[test]
fn search_select_existing() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::Select(vec!["NotebookEdit".into()]),
        &registry,
        10,
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "NotebookEdit");
}

#[test]
fn search_select_batch_partial() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::Select(vec!["NotebookEdit".into(), "NonExistent".into()]),
        &registry,
        10,
    );
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "NotebookEdit");
}

#[test]
fn search_keywords_match() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::Keywords(vec!["slack".into()]),
        &registry,
        10,
    );
    assert_eq!(results.len(), 2);
    let ids: Vec<&str> = results.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"mcp__slack__read_channel"));
    assert!(ids.contains(&"mcp__slack__send_message"));
}

#[test]
fn search_keywords_ranked_by_relevance() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::Keywords(vec!["slack".into(), "send".into()]),
        &registry,
        10,
    );
    // send_message has both "slack" and "send", so it should rank first
    assert!(!results.is_empty());
    assert_eq!(results[0].id, "mcp__slack__send_message");
}

#[test]
fn search_required_keyword() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::RequiredKeyword {
            required: "slack".into(),
            rest: vec!["read".into()],
        },
        &registry,
        10,
    );
    // Only slack tools returned, read_channel should rank first
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, "mcp__slack__read_channel");
}

#[test]
fn search_max_results_limit() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::Keywords(vec!["slack".into()]),
        &registry,
        1,
    );
    assert_eq!(results.len(), 1);
}

#[test]
fn search_no_match() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::Keywords(vec!["nonexistent".into()]),
        &registry,
        10,
    );
    assert!(results.is_empty());
}

// --- Format tests ---

#[test]
fn format_results_contains_tool_info() {
    let registry = make_registry();
    let results = search_registry(
        &ToolSearchQuery::Select(vec!["NotebookEdit".into()]),
        &registry,
        10,
    );
    let (formatted, promote_ids) = format_search_results(&results);
    assert!(formatted.contains("NotebookEdit"));
    assert!(formatted.contains("Edit Jupyter notebook cells"));
    assert_eq!(promote_ids, vec!["NotebookEdit"]);
}

#[test]
fn format_empty_results() {
    let (formatted, promote_ids) = format_search_results(&[]);
    assert!(formatted.contains("No matching"));
    assert!(promote_ids.is_empty());
}

// --- keyword_score unit test ---

#[test]
fn keyword_score_counts_matches() {
    let tool = make_descriptor(
        "mcp__slack__send_message",
        "mcp__slack__send_message",
        "Send a Slack message",
    );
    let score = keyword_score(&tool, &["slack".to_string(), "send".to_string()]);
    // "slack" appears in id (twice: mcp__slack__send...) + description, "send" appears in id + description
    assert!(score >= 2);
}

#[test]
fn keyword_score_zero_for_no_match() {
    let tool = make_descriptor("WebSearch", "WebSearch", "Search the web");
    let score = keyword_score(&tool, &["slack".to_string()]);
    assert_eq!(score, 0);
}
