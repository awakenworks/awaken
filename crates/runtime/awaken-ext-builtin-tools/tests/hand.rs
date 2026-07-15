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

// REGRESSION: an empty `old` is a no-op anchor — on an empty file the "exactly
// one occurrence" arm would silently insert `new`. It must be rejected, and the
// file left untouched.
#[tokio::test]
async fn edit_rejects_an_empty_old_and_does_not_mutate_an_empty_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("empty.txt");
    std::fs::write(&path, "").expect("write");

    let err = tool("edit")
        .invoke(call(
            "edit",
            serde_json::json!({ "path": path, "old": "", "new": "injected" }),
        ))
        .await
        .expect_err("empty old must be rejected");
    assert!(matches!(err, ToolError::InvalidArguments(_)));
    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "",
        "the file must be untouched"
    );
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
async fn glob_with_no_matches_returns_empty_success_not_an_error() {
    // A well-formed pattern that matches nothing is a valid empty result, not an
    // error — the model reads "no files" from empty output, not a failure.
    let dir = tempfile::tempdir().expect("tempdir");
    let pattern = format!("{}/*.nonesuch", dir.path().display());
    let out = tool("glob")
        .invoke(call("glob", serde_json::json!({ "pattern": pattern })))
        .await
        .expect("glob with no matches is not an error");
    assert!(!out.is_error);
    assert_eq!(out.content, "", "no matches renders as empty output");
}

#[tokio::test]
async fn grep_with_no_matching_lines_returns_empty_success_not_an_error() {
    // A valid regex that matches no line is an empty result, not an error.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("plain.txt");
    std::fs::write(&path, "alpha\nbeta\ngamma").expect("write");
    let out = tool("grep")
        .invoke(call(
            "grep",
            serde_json::json!({ "pattern": "no-such-token", "path": path }),
        ))
        .await
        .expect("grep with no hits is not an error");
    assert!(!out.is_error);
    assert_eq!(out.content, "", "no hits renders as empty output");
}

#[tokio::test]
async fn bash_terminated_by_signal_reports_a_signal_code() {
    // A command killed by a signal has no exit code; the error must name "signal"
    // rather than panic on the `None` exit status.
    let err = tool("bash")
        .invoke(call("bash", serde_json::json!({ "command": "kill -9 $$" })))
        .await
        .expect_err("signal-terminated command");
    match err {
        ToolError::Execution(msg) => assert!(
            msg.contains("signal"),
            "signal termination names the signal branch: {msg}"
        ),
        other => panic!("expected Execution, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_failure_surfaces_stderr_and_stdout_not_a_swallowed_error() {
    // A non-zero exit must carry the command's stdout AND stderr into the model-
    // visible error, so a failing command is diagnosable rather than swallowed.
    let err = tool("bash")
        .invoke(call(
            "bash",
            serde_json::json!({ "command": "echo out-line; echo err-line >&2; exit 7" }),
        ))
        .await
        .expect_err("nonzero exit");
    let msg = err.to_string();
    assert!(msg.contains("exited 7"), "carries the exit code: {msg}");
    assert!(msg.contains("out-line"), "carries stdout: {msg}");
    assert!(msg.contains("err-line"), "carries stderr: {msg}");
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
