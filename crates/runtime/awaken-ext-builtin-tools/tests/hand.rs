//! Hand tools execute in-process against a real (temp) filesystem.

use awaken_ext_builtin_tools::executable_hand_tools;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError};
use std::sync::Arc;

fn tool(id: &str) -> Arc<dyn RawTool> {
    executable_hand_tools()
        .into_iter()
        .find(|t| t.id() == id)
        .unwrap_or_else(|| panic!("no builtin tool {id}"))
}

fn call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".to_string(),
        tool_id: id.to_string(),
        arguments: args,
    }
}

#[tokio::test]
async fn read_returns_file_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "hello\nworld").expect("write");

    let out = tool("read")
        .invoke(call("read", serde_json::json!({ "path": path })))
        .await
        .expect("read");
    assert_eq!(out.content, "hello\nworld");
    assert!(!out.is_error);
}

#[tokio::test]
async fn read_missing_file_is_a_typed_error() {
    let err = tool("read")
        .invoke(call("read", serde_json::json!({ "path": "/no/such/file" })))
        .await
        .expect_err("missing file");
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn glob_lists_matching_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.rs"), "").expect("write");
    std::fs::write(dir.path().join("b.rs"), "").expect("write");
    std::fs::write(dir.path().join("c.txt"), "").expect("write");

    let pattern = format!("{}/*.rs", dir.path().display());
    let out = tool("glob")
        .invoke(call("glob", serde_json::json!({ "pattern": pattern })))
        .await
        .expect("glob");
    let mut lines: Vec<&str> = out.content.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines.len(), 2, "two .rs files: {:?}", out.content);
    assert!(lines.iter().all(|l| l.ends_with(".rs")));
}

#[tokio::test]
async fn grep_finds_matching_lines_with_line_numbers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("log.txt");
    std::fs::write(&path, "alpha\nbeta error\ngamma\ndelta error").expect("write");

    let out = tool("grep")
        .invoke(call(
            "grep",
            serde_json::json!({ "pattern": "error", "path": path }),
        ))
        .await
        .expect("grep");
    let lines: Vec<&str> = out.content.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains(":2:beta error"));
    assert!(lines[1].contains(":4:delta error"));
}

#[tokio::test]
async fn write_creates_a_file_with_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.txt");
    let out = tool("write")
        .invoke(call(
            "write",
            serde_json::json!({ "path": path, "content": "payload" }),
        ))
        .await
        .expect("write");
    assert!(out.content.contains("wrote"));
    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "payload"
    );
}

#[tokio::test]
async fn edit_replaces_a_unique_occurrence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("code.txt");
    std::fs::write(&path, "let x = 1;\nlet y = 2;").expect("write");

    tool("edit")
        .invoke(call(
            "edit",
            serde_json::json!({ "path": path, "old": "x = 1", "new": "x = 42" }),
        ))
        .await
        .expect("edit");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "let x = 42;\nlet y = 2;"
    );
}

#[tokio::test]
async fn edit_fails_closed_on_missing_or_ambiguous_old() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dup.txt");
    std::fs::write(&path, "a a a").expect("write");

    let missing = tool("edit")
        .invoke(call(
            "edit",
            serde_json::json!({ "path": path, "old": "zzz", "new": "q" }),
        ))
        .await
        .expect_err("missing old");
    assert!(matches!(missing, ToolError::Execution(_)));

    let ambiguous = tool("edit")
        .invoke(call(
            "edit",
            serde_json::json!({ "path": path, "old": "a", "new": "b" }),
        ))
        .await
        .expect_err("ambiguous old");
    assert!(matches!(ambiguous, ToolError::Execution(_)));
    // The file is untouched because the edit failed closed.
    assert_eq!(std::fs::read_to_string(&path).expect("read back"), "a a a");
}

#[tokio::test]
async fn bash_runs_a_command_and_returns_stdout() {
    let out = tool("bash")
        .invoke(call("bash", serde_json::json!({ "command": "echo hello" })))
        .await
        .expect("bash");
    assert_eq!(out.content.trim(), "hello");
}

#[tokio::test]
async fn bash_nonzero_exit_is_a_typed_error() {
    let err = tool("bash")
        .invoke(call("bash", serde_json::json!({ "command": "exit 3" })))
        .await
        .expect_err("nonzero exit");
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn grep_invalid_regex_is_a_typed_error() {
    let err = tool("grep")
        .invoke(call(
            "grep",
            serde_json::json!({ "pattern": "(unclosed", "path": "/tmp/whatever" }),
        ))
        .await
        .expect_err("bad regex");
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn grep_missing_file_is_a_typed_error() {
    let err = tool("grep")
        .invoke(call(
            "grep",
            serde_json::json!({ "pattern": "x", "path": "/no/such/file" }),
        ))
        .await
        .expect_err("missing file");
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn glob_invalid_pattern_is_a_typed_error() {
    let err = tool("glob")
        .invoke(call("glob", serde_json::json!({ "pattern": "a[b" })))
        .await
        .expect_err("bad glob");
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[tokio::test]
async fn write_to_an_unwritable_path_is_a_typed_error() {
    let err = tool("write")
        .invoke(call(
            "write",
            serde_json::json!({ "path": "/no/such/dir/out.txt", "content": "y" }),
        ))
        .await
        .expect_err("bad dir");
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn edit_missing_file_is_a_typed_error() {
    let err = tool("edit")
        .invoke(call(
            "edit",
            serde_json::json!({ "path": "/no/such/file", "old": "a", "new": "b" }),
        ))
        .await
        .expect_err("missing file");
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn unknown_argument_shape_is_an_invalid_arguments_error() {
    // `read` requires `path`; a wrong shape is a typed arg error from erasure.
    let err = tool("read")
        .invoke(call("read", serde_json::json!({ "wrong": 1 })))
        .await
        .expect_err("bad args");
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}
