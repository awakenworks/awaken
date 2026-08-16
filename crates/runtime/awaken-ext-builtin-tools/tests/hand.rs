//! Hand tools execute in-process against a real (temp) filesystem.

use awaken_ext_builtin_tools::{HandToolContext, executable_hand_tools_in};
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError};
use std::sync::Arc;

fn tool(id: &str) -> Arc<dyn RawTool> {
    tool_at(id, std::path::Path::new(std::path::MAIN_SEPARATOR_STR))
}

fn tool_at(id: &str, root: &std::path::Path) -> Arc<dyn RawTool> {
    executable_hand_tools_in(HandToolContext::new(root))
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
    assert_eq!(out.text(), "hello\nworld");
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

    let out = tool("glob")
        .invoke(call(
            "glob",
            serde_json::json!({ "pattern": "*.rs", "path": dir.path() }),
        ))
        .await
        .expect("glob");
    let content = out.text();
    let mut lines: Vec<&str> = content.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines.len(), 2, "two .rs files: {content:?}");
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
    let content = out.text();
    let lines: Vec<&str> = content.lines().collect();
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
    assert!(out.text().contains("wrote"));
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
    assert_eq!(out.text().trim(), "hello");
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
    let dir = tempfile::tempdir().expect("tempdir");
    let parent_file = dir.path().join("not-a-directory");
    std::fs::write(&parent_file, "occupied").expect("write parent file");
    let err = tool("write")
        .invoke(call(
            "write",
            serde_json::json!({ "path": parent_file.join("out.txt"), "content": "y" }),
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
    let out = tool("glob")
        .invoke(call(
            "glob",
            serde_json::json!({ "pattern": "*.nonesuch", "path": dir.path() }),
        ))
        .await
        .expect("glob with no matches is not an error");
    assert!(!out.is_error);
    assert_eq!(out.text(), "no matches");
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
    assert_eq!(out.text(), "no matches");
}

#[tokio::test]
#[cfg(unix)]
async fn bash_terminated_by_signal_reports_session_termination() {
    let err = tool("bash")
        .invoke(call("bash", serde_json::json!({ "command": "kill -9 $$" })))
        .await
        .expect_err("signal-terminated command");
    assert!(err.to_string().contains("bash session terminated"));
}

#[tokio::test]
async fn bash_failure_surfaces_stderr_and_stdout_not_a_swallowed_error() {
    // A non-zero exit must carry the command's stdout AND stderr into the model-
    // visible error, so a failing command is diagnosable rather than swallowed.
    let err = tool("bash")
        .invoke(call(
            "bash",
            serde_json::json!({ "command": failing_shell_command() }),
        ))
        .await
        .expect_err("nonzero exit");
    let msg = err.to_string();
    assert!(msg.contains("out-line"), "carries stdout: {msg}");
    assert!(msg.contains("err-line"), "carries stderr: {msg}");
}

#[tokio::test]
async fn bash_persists_cwd_and_environment_and_restart_clears_both() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    let bash = tool_at("bash", dir.path());
    bash.invoke(call(
        "bash",
        serde_json::json!({ "command": "cd nested; export AWAKEN_TEST_VALUE=kept" }),
    ))
    .await
    .unwrap();
    let persisted = bash
        .invoke(call(
            "bash",
            serde_json::json!({ "command": "printf '%s:%s' \"$PWD\" \"$AWAKEN_TEST_VALUE\"" }),
        ))
        .await
        .unwrap();
    assert!(persisted.text().ends_with("/nested:kept"));

    let restarted = bash
        .invoke(call(
            "bash",
            serde_json::json!({
                "restart": true,
                "command": "printf '%s:%s' \"$PWD\" \"$AWAKEN_TEST_VALUE\""
            }),
        ))
        .await
        .unwrap();
    assert_eq!(restarted.text(), format!("{}:", dir.path().display()));
}

#[tokio::test]
async fn bash_timeout_discards_the_session_and_next_call_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let bash = tool_at("bash", dir.path());
    let timeout = bash
        .invoke(call(
            "bash",
            serde_json::json!({
                "command": "export SHOULD_DISAPPEAR=yes; sleep 1",
                "timeout_ms": 25
            }),
        ))
        .await
        .expect_err("command must time out");
    assert!(timeout.to_string().contains("timed out after 25ms"));

    let clean = bash
        .invoke(call(
            "bash",
            serde_json::json!({ "command": "printf '%s' \"$SHOULD_DISAPPEAR\"" }),
        ))
        .await
        .unwrap();
    assert_eq!(clean.text(), "");
}

#[tokio::test]
async fn bash_strips_ansi_and_keeps_only_the_last_100_kib() {
    let bash = tool("bash");
    let output = bash
        .invoke(call(
            "bash",
            serde_json::json!({
                "command": "printf '\\033[31mred\\033[0m\\n'; head -c 110000 /dev/zero | tr '\\0' x"
            }),
        ))
        .await
        .unwrap()
        .text();
    assert!(output.starts_with("[output truncated]\n"));
    assert!(!output.contains("\\u{1b}["));
    assert!(output.len() <= 100 * 1024 + "[output truncated]\n".len());
}

#[tokio::test]
async fn file_tools_reject_parent_absolute_and_symlink_escapes() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "secret").unwrap();
    for path in [root.path().join("../escape"), outside.path().join("secret")] {
        let error = tool_at("read", root.path())
            .invoke(call("read", serde_json::json!({ "file_path": path })))
            .await
            .expect_err("outside workdir must fail");
        assert!(error.to_string().contains("escapes workdir"));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let error = tool_at("read", root.path())
            .invoke(call(
                "read",
                serde_json::json!({ "file_path": "link/secret" }),
            ))
            .await
            .expect_err("symlink escape must fail");
        assert!(error.to_string().contains("escapes workdir"));
    }
}

#[tokio::test]
async fn read_and_edit_reject_oversized_files() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("large"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    for id in ["read", "edit"] {
        let arguments = if id == "read" {
            serde_json::json!({ "file_path": "large" })
        } else {
            serde_json::json!({
                "file_path": "large",
                "old_string": "x",
                "new_string": "y"
            })
        };
        let error = tool_at(id, root.path())
            .invoke(call(id, arguments))
            .await
            .expect_err("large file is rejected before allocation");
        assert!(error.to_string().contains("262144-byte limit"));
    }
}

#[cfg(windows)]
fn failing_shell_command() -> &'static str {
    "echo out-line & echo err-line 1>&2 & cmd /D /C exit 7"
}

#[cfg(not(windows))]
fn failing_shell_command() -> &'static str {
    "echo out-line; echo err-line >&2; false"
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
