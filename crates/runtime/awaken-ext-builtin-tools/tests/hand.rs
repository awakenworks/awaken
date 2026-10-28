// @formal evidence sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
//! Hand tools execute in-process against a real (temp) filesystem.

use awaken_ext_builtin_tools::{HandToolContext, all_hand_tools_in};
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError};
use std::sync::Arc;

#[cfg(windows)]
#[path = "hand/windows_process.rs"]
mod windows_process;

fn tool(id: &str) -> Arc<dyn RawTool> {
    // Temporary fixtures may live on another drive or behind a junction.
    let temp_dir = std::env::temp_dir().canonicalize().expect("temp dir");
    tool_at(
        id,
        temp_dir.ancestors().last().expect("temp filesystem root"),
    )
}

fn tool_at(id: &str, root: &std::path::Path) -> Arc<dyn RawTool> {
    all_hand_tools_in(HandToolContext::new(root))
        .into_iter()
        .find(|t| t.id() == id)
        .unwrap_or_else(|| panic!("no builtin tool {id}"))
}

#[test]
fn hand_context_debug_exposes_shape_without_provider_values() {
    // Diagnostic cause/effect table: D1 physical roots only; D2 root plus Bash
    // environment; D3 root plus launcher. Every row may expose counts/booleans
    // (E1) but never the provider path, environment value or launcher argv
    // (E2). Rules: D1=>E1+E2; D1+D2=>E1+E2; D1+D3=>E1+E2.
    // Constraint: Debug is the logging surface; HandToolContext has no serde
    // surface, and the tool-result wire remains covered by the Namespace test.
    let provider_root = "/provider/private/session-9";
    let mut bash_env = std::collections::BTreeMap::new();
    bash_env.insert("PRIVATE_VALUE".into(), "provider-env-sentinel".into());
    let context = HandToolContext::new(format!("{provider_root}/workspace"))
        .with_allowed_root(format!("{provider_root}/mnt"))
        .with_path_projection("/workspace", format!("{provider_root}/workspace"))
        .with_bash_env(bash_env)
        .with_bash_launcher(
            format!("{provider_root}/bin/bash"),
            ["provider-argv-sentinel".into()],
        );

    let debug = format!("{context:?}");
    assert!(debug.contains("allowed_root_count: 1"), "D1/E1: {debug}");
    assert!(debug.contains("path_projection_count: 1"), "D1/E1: {debug}");
    assert!(debug.contains("has_bash_env: true"), "D2/E1: {debug}");
    assert!(debug.contains("has_bash_launcher: true"), "D3/E1: {debug}");
    for private in [
        provider_root,
        "provider-env-sentinel",
        "provider-argv-sentinel",
    ] {
        assert!(!debug.contains(private), "D1-D3/E2: {debug}");
    }
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
    // Causes: known UTF-8 input is empty/plain/CSV/script/Skill; requested view
    // is full or partial. R1 every full read returns exact bytes (including CJK,
    // CRLF and a missing final newline), without encoding or shell probes.
    // R2 an explicit range returns only that range, never evidence of a full
    // Skill load. Adjacent missing/binary tests cover the failure effect R3.
    // These are execution checks for the public reader guidance, not proof
    // that a model chooses this tool; Cowork's real-model E4/UX gate owns that.
    let dir = tempfile::tempdir().expect("tempdir");
    let reader = tool_at("read", dir.path());
    for (name, content) in [
        ("empty.txt", ""),
        ("note.txt", "hello\nworld"),
        ("客户.csv", "客户,金额\r\n张三,0\r\n"),
        ("generate.py", "print('资料')\n"),
        ("SKILL.md", "---\nname: office\n---\n完整交付规则\n末尾约束"),
    ] {
        std::fs::write(dir.path().join(name), content).expect("write");
        let out = reader
            .invoke(call("read", serde_json::json!({ "file_path": name })))
            .await
            .expect("R1 full read");
        assert_eq!(out.text(), content, "R1 {name}");
        assert!(!out.is_error);
    }
    let partial = reader
        .invoke(call(
            "read",
            serde_json::json!({
                "file_path": "SKILL.md", "view_range": [4, 4]
            }),
        ))
        .await
        .expect("R2 explicit range");
    assert_eq!(partial.text(), "完整交付规则");
}

#[tokio::test]
async fn filesystem_and_frozen_read_share_range_and_size_semantics() {
    // Cause/effect table: R4 UTF-8 + full/range (negative, reversed, EOF,
    // i64 extremes) => exact expected text in both storage adapters;
    // R5 >limit, even with range => fail before returning any slice;
    // R6 invalid UTF-8 => typed failure. A range does not bypass the file limit.
    use awaken_ext_builtin_tools::{ReadArgs, ReadTool};
    let dir = tempfile::tempdir().unwrap();
    let reader = tool_at("read", dir.path());
    let bytes = "一\r\n二\n三\n".as_bytes();
    std::fs::write(dir.path().join("guide.md"), bytes).unwrap();
    for (range, expected) in [
        (None, "一\r\n二\n三\n"),
        (Some([i64::MIN, 1]), "一\r"),
        (Some([2, 2]), "二"),
        (Some([2, 0]), "二\n三\n"),
        (Some([3, 1]), ""),
        (Some([i64::MAX, i64::MAX]), ""),
    ] {
        let args = ReadArgs {
            path: "guide.md".into(),
            view_range: range,
        };
        assert_eq!(
            args.read_content(bytes, Some(ReadTool::DEFAULT_MAX_FILE_BYTES))
                .unwrap(),
            expected
        );
        let result = reader
            .invoke(call(
                "read",
                serde_json::json!({"file_path": "guide.md", "view_range": range}),
            ))
            .await
            .unwrap();
        assert_eq!(result.text(), expected, "R4 {range:?}");
    }
    for bytes in [
        vec![b'a'; ReadTool::DEFAULT_MAX_FILE_BYTES as usize + 1],
        vec![0xff],
    ] {
        std::fs::write(dir.path().join("guide.md"), &bytes).unwrap();
        let args = ReadArgs {
            path: "guide.md".into(),
            view_range: Some([1, 1]),
        };
        let frozen = args
            .read_content(&bytes, Some(ReadTool::DEFAULT_MAX_FILE_BYTES))
            .unwrap_err();
        let file = reader
            .invoke(call(
                "read",
                serde_json::json!({"file_path": "guide.md", "view_range": [1,1]}),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            frozen.to_string(),
            file.to_string(),
            "R5/R6 same error contract"
        );
    }
}

#[tokio::test]
async fn read_does_not_decode_non_utf8_files() {
    // R3 invalid UTF-8 bytes -> typed failure, never a lossy successful text
    // result; the input remains unchanged. Format-specific extraction belongs
    // to document/OCR tools, not a second path inside the text reader.
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = b"PK\x03\x04\xff\xfe";
    let path = dir.path().join("document.docx");
    std::fs::write(&path, bytes).expect("binary fixture");
    let err = tool_at("read", dir.path())
        .invoke(call(
            "read",
            serde_json::json!({ "file_path": "document.docx" }),
        ))
        .await
        .expect_err("R3 binary decoding is unavailable");
    assert!(matches!(err, ToolError::DefinitiveFailure(_)));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn read_missing_file_is_a_typed_error() {
    let dir = tempfile::tempdir().expect("private missing-file root");
    let err = tool_at("read", dir.path())
        .invoke(call("read", serde_json::json!({ "path": "missing-file" })))
        .await
        .expect_err("missing file");
    assert!(matches!(err, ToolError::DefinitiveFailure(_)));
    assert!(err.to_string().contains("no such file or directory"));
}

#[tokio::test]
async fn glob_lists_matching_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.rs"), "").expect("write");
    std::fs::write(dir.path().join("b.rs"), "").expect("write");
    std::fs::write(dir.path().join("c.txt"), "").expect("write");

    let out = tool_at("glob", dir.path())
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
async fn glob_matches_files_and_directories_with_doublestar_segments() {
    // Cause/effect graph: C1 the candidate is a file/directory; C2 the pattern
    // contains a zero-or-more-segment `**`; C3 the suffix matches/does not
    // match. E1 both matching entry kinds are returned; E2 `**` consumes zero
    // or multiple segments; E3 non-matching entries are absent.
    //
    // | Rule | Kind | `**` depth | Suffix | Effect |
    // |---|---|---|---|---|
    // | G5 | directory | zero | match | E1+E2 |
    // | G6 | file | multiple | match | E1+E2 |
    // | G7 | file | multiple | miss | E3 |
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src/nested")).expect("nested directory");
    std::fs::write(dir.path().join("src/nested/lib.rs"), "").expect("matching file");
    std::fs::write(dir.path().join("src/nested/lib.txt"), "").expect("non-matching file");

    let directories = tool_at("glob", dir.path())
        .invoke(call("glob", serde_json::json!({ "pattern": "**/src" })))
        .await
        .expect("G5 directory match")
        .text();
    assert!(directories.ends_with("/src"), "G5/E1+E2: {directories}");

    let files = tool_at("glob", dir.path())
        .invoke(call("glob", serde_json::json!({ "pattern": "**/*.rs" })))
        .await
        .expect("G6 recursive file match")
        .text();
    assert!(files.ends_with("/src/nested/lib.rs"), "G6/E1+E2: {files}");
    assert!(!files.contains("lib.txt"), "G7/E3: {files}");
}

#[tokio::test]
async fn glob_absolute_pattern_reuses_the_confined_logical_path_projection() {
    // Cause/effect graph: C1 pattern is relative/absolute; C2 an absolute
    // pattern is inside/outside a trusted logical projection; C3 `path` is
    // absent/present. Effects: E1 relative behavior is unchanged; E2 a trusted
    // absolute pattern matches and returns logical paths; E3 escape and
    // ambiguous absolute+path inputs fail closed without exposing host paths.
    //
    // | Rule | Pattern | Projection | path | Effect |
    // |---|---|---|---|---|
    // | G1 | relative | n/a | present | E1 |
    // | G2 | absolute | trusted | absent | E2 |
    // | G3 | absolute | absent | absent | E3 |
    // | G4 | absolute | trusted | present | E3 |
    // Constraint: FileContext remains the one confinement/projection owner for
    // every filesystem tool; glob adds no path map of its own.
    let workdir = tempfile::tempdir().expect("workdir");
    let projected = tempfile::tempdir().expect("physical projection");
    std::fs::write(projected.path().join("a.rs"), "a").expect("projected file");
    let context =
        HandToolContext::new(workdir.path()).with_path_projection("/managed", projected.path());
    let glob = all_hand_tools_in(context)
        .into_iter()
        .find(|candidate| candidate.id() == "glob")
        .expect("glob tool");

    let relative = glob
        .invoke(call(
            "glob",
            serde_json::json!({ "pattern": "*.rs", "path": "/managed" }),
        ))
        .await
        .expect("G1 relative pattern under a logical root");
    assert_eq!(relative.text(), "/managed/a.rs", "G1/E1");

    let output = glob
        .invoke(call(
            "glob",
            serde_json::json!({ "pattern": "/managed/*.rs" }),
        ))
        .await
        .expect("G2 trusted logical pattern");
    assert_eq!(output.text(), "/managed/a.rs", "G2/E2");
    assert!(
        !output
            .text()
            .contains(&projected.path().to_string_lossy()[..]),
        "G2/E2"
    );

    let outside = glob
        .invoke(call(
            "glob",
            serde_json::json!({ "pattern": "/outside/*.rs" }),
        ))
        .await
        .expect_err("G3 untrusted absolute pattern");
    assert!(matches!(outside, ToolError::DefinitiveFailure(_)), "G3/E3");

    let ambiguous = glob
        .invoke(call(
            "glob",
            serde_json::json!({
                "pattern": "/managed/*.rs",
                "path": workdir.path()
            }),
        ))
        .await
        .expect_err("G4 absolute pattern has no second root");
    assert!(matches!(ambiguous, ToolError::InvalidArguments(_)), "G4/E3");
}

#[tokio::test]
async fn glob_supports_node_brace_and_at_extglob_alternation_without_duplicates() {
    // Cause/effect graph: Node fs.glob alternation forms select two extensions;
    // overlapping alternatives must not duplicate a path in the 200-result budget.
    let dir = tempfile::tempdir().expect("tempdir");
    for name in ["a.rs", "b.ts", "c.txt"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    for pattern in ["*.{rs,ts,rs}", "@(a.rs|b.ts|a.rs)"] {
        let output = tool_at("glob", dir.path())
            .invoke(call("glob", serde_json::json!({ "pattern": pattern })))
            .await
            .unwrap()
            .text();
        let mut names = output
            .lines()
            .map(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
            })
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["a.rs", "b.ts"], "pattern {pattern}");
    }
}

#[tokio::test]
async fn glob_rejects_combinatorial_alternation_before_walking() {
    // Decision table: <=256 expansions are bounded work; >256 is a typed
    // argument error and performs no filesystem traversal.
    let pattern = "{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}";
    let error = tool("glob")
        .invoke(call("glob", serde_json::json!({ "pattern": pattern })))
        .await
        .expect_err("512 alternatives must fail closed");
    assert!(matches!(error, ToolError::InvalidArguments(_)));
    assert!(error.to_string().contains("more than 256 alternatives"));
}

#[tokio::test]
async fn grep_finds_matching_lines_with_line_numbers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("log.txt");
    std::fs::write(&path, "alpha\nbeta error\ngamma\ndelta error").expect("write");

    let out = tool_at("grep", dir.path())
        .invoke(call(
            "grep",
            serde_json::json!({ "pattern": "error", "path": path }),
        ))
        .await
        .expect("grep");
    let content = out.text();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "2:beta error");
    assert_eq!(lines[1], "4:delta error");
}

#[tokio::test]
async fn write_creates_a_file_with_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.txt");
    let out = tool_at("write", dir.path())
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

    tool_at("edit", dir.path())
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

    let missing = tool_at("edit", dir.path())
        .invoke(call(
            "edit",
            serde_json::json!({ "path": path, "old": "zzz", "new": "q" }),
        ))
        .await
        .expect_err("missing old");
    assert!(matches!(missing, ToolError::DefinitiveFailure(_)));
    assert!(missing.to_string().contains("old_string not found"));

    let ambiguous = tool_at("edit", dir.path())
        .invoke(call(
            "edit",
            serde_json::json!({ "path": path, "old": "a", "new": "b" }),
        ))
        .await
        .expect_err("ambiguous old");
    assert!(matches!(ambiguous, ToolError::DefinitiveFailure(_)));
    assert!(ambiguous.to_string().contains("appears 3 times"));
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

    let err = tool_at("edit", dir.path())
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
    assert!(matches!(err, ToolError::DefinitiveFailure(_)));
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
async fn grep_matches_managed_ignore_and_hidden_file_semantics_without_host_tools() {
    // Cause/effect graph: C1 the search root has a visible file; C2 a root
    // `.gitignore` excludes one file; C3 one file is hidden. E1 visible content
    // is reported; E2 ignored and hidden content are absent. Rule S1:
    // C1+C2+C3 => E1+E2, independently of host PATH or an installed `rg`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("visible.txt"), "needle\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "needle\n").unwrap();
    std::fs::write(dir.path().join(".hidden.txt"), "needle\n").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested/ignored.txt"), "needle\n").unwrap();
    std::fs::write(dir.path().join("nested/keep.txt"), "needle\n").unwrap();
    std::fs::write(dir.path().join("nested/.gitignore"), "*.txt\n!keep.txt\n").unwrap();

    let output = tool_at("grep", dir.path())
        .invoke(call("grep", serde_json::json!({ "pattern": "needle" })))
        .await
        .unwrap()
        .text();
    assert!(output.contains("visible.txt:1:needle"));
    assert!(!output.contains("ignored.txt"));
    assert!(!output.contains(".hidden.txt"));
    assert!(output.contains("nested/keep.txt:1:needle"));
}

#[tokio::test]
async fn grep_caps_output_and_marks_truncation_without_host_tools() {
    // Cause/effect graph: C1 matching output is below/above 100 KiB. E1 output
    // is complete below the cap; E2 output is bounded and marked above it.
    // This case covers C1=above => E2 on the deterministic in-process path.
    let dir = tempfile::tempdir().unwrap();
    let line = format!("needle-{}", "x".repeat(1900));
    std::fs::write(
        dir.path().join("large.txt"),
        format!("{}\n", line).repeat(100),
    )
    .unwrap();
    let output = tool_at("grep", dir.path())
        .invoke(call("grep", serde_json::json!({ "pattern": "needle" })))
        .await
        .unwrap()
        .text();
    assert!(output.ends_with("[output truncated]"));
    assert!(output.len() <= 102400 + "\n[output truncated]".len());
}

#[tokio::test]
async fn grep_searches_long_text_lines_and_skips_binary_and_oversized_files() {
    // Cause/effect graph: C1 UTF-8 text/binary input; C2 file size is at/beyond
    // the 8 MiB managed limit; C3 a matching line is short/long. E1 ordinary
    // and long text lines are searched; E2 binary and oversized files are
    // skipped without an error or leaked match.
    //
    // | Rule | Input | Size | Line | Effect |
    // |---|---|---|---|---|
    // | S2 | text | below cap | >2000 bytes | E1 |
    // | S3 | binary | below cap | matching bytes | E2 |
    // | S4 | text | above cap | matching line | E2 |
    let dir = tempfile::tempdir().expect("tempdir");
    let long_line = format!("{}needle-long", "x".repeat(3_000));
    std::fs::write(dir.path().join("long.txt"), &long_line).expect("long text line");
    std::fs::write(dir.path().join("binary.bin"), b"needle-binary\0payload").expect("binary file");
    let oversized =
        std::fs::File::create(dir.path().join("oversized.txt")).expect("oversized file");
    oversized
        .set_len(8 * 1024 * 1024 + 1)
        .expect("sparse oversized file");

    let output = tool_at("grep", dir.path())
        .invoke(call("grep", serde_json::json!({ "pattern": "needle" })))
        .await
        .expect("bounded grep")
        .text();
    assert!(output.contains("long.txt:1:"), "S2/E1: {output}");
    assert!(output.contains("needle-long"), "S2/E1: {output}");
    assert!(!output.contains("binary.bin"), "S3/E2: {output}");
    assert!(!output.contains("oversized.txt"), "S4/E2: {output}");
}

#[tokio::test]
async fn grep_missing_file_is_a_typed_error() {
    let dir = tempfile::tempdir().expect("private missing-file root");
    let err = tool_at("grep", dir.path())
        .invoke(call(
            "grep",
            serde_json::json!({ "pattern": "x", "path": "missing-file" }),
        ))
        .await
        .expect_err("missing file");
    assert!(matches!(err, ToolError::DefinitiveFailure(_)));
    assert!(err.to_string().contains("no such file or directory"));
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
async fn glob_caps_results_and_denies_symlink_escape() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    for index in 0..205 {
        std::fs::write(root.path().join(format!("file-{index:03}.rs")), "").unwrap();
    }
    std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();

    let output = tool_at("glob", root.path())
        .invoke(call("glob", serde_json::json!({ "pattern": "**/*.rs" })))
        .await
        .unwrap()
        .text();
    assert_eq!(output.lines().count(), 200);
    assert!(!output.contains("secret.rs"));
}

#[tokio::test]
async fn search_purpose_keeps_glob_complete_and_grep_noise_filtered() {
    // Anthropic conformance cause/effect graph: C1 candidate is below `.git`,
    // `node_modules`, or an ordinary directory; C2 purpose is Glob/Grep.
    // E1 Glob returns every matching real entry; E2 Grep searches only the
    // ordinary entry. The shared walker owns traversal, while `WalkPurpose`
    // alone owns the documented policy difference.
    //
    // | Rule | Candidate | Purpose | Effect |
    // |---|---|---|---|
    // | C1 | `.git` | Glob | E1 include |
    // | C2 | `node_modules` | Glob | E1 include |
    // | C3 | either noise directory | Grep | E2 prune |
    // | C4 | ordinary file | Glob/Grep | include/search |
    let root = tempfile::tempdir().expect("root");
    std::fs::create_dir(root.path().join(".git")).expect(".git");
    std::fs::create_dir(root.path().join("node_modules")).expect("node_modules");
    for relative in [".git/inside.rs", "node_modules/inside.rs", "visible.rs"] {
        std::fs::write(root.path().join(relative), "needle\n").expect("fixture");
    }

    let glob = tool_at("glob", root.path())
        .invoke(call("glob", serde_json::json!({ "pattern": "**/*.rs" })))
        .await
        .expect("Glob conformance")
        .text();
    assert!(glob.contains("/.git/inside.rs"), "C1/E1: {glob}");
    assert!(glob.contains("/node_modules/inside.rs"), "C2/E1: {glob}");
    assert!(glob.contains("/visible.rs"), "C4: {glob}");

    let grep = tool_at("grep", root.path())
        .invoke(call("grep", serde_json::json!({ "pattern": "needle" })))
        .await
        .expect("Grep conformance")
        .text();
    assert_eq!(grep.lines().count(), 1, "C3/E2: {grep}");
    assert!(grep.contains("/visible.rs:1:needle"), "C4: {grep}");
}

#[tokio::test]
async fn write_to_an_unwritable_path_is_a_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let parent_file = dir.path().join("not-a-directory");
    std::fs::write(&parent_file, "occupied").expect("write parent file");
    let err = tool_at("write", dir.path())
        .invoke(call(
            "write",
            serde_json::json!({ "path": parent_file.join("out.txt"), "content": "y" }),
        ))
        .await
        .expect_err("bad dir");
    assert!(matches!(err, ToolError::DefinitiveFailure(_)));
    assert!(!err.to_string().contains("escapes workdir"));
    assert_eq!(std::fs::read_to_string(parent_file).unwrap(), "occupied");
}

#[tokio::test]
async fn edit_missing_file_is_a_typed_error() {
    let dir = tempfile::tempdir().expect("private missing-file root");
    let err = tool_at("edit", dir.path())
        .invoke(call(
            "edit",
            serde_json::json!({ "path": "missing-file", "old": "a", "new": "b" }),
        ))
        .await
        .expect_err("missing file");
    assert!(matches!(err, ToolError::DefinitiveFailure(_)));
    assert!(err.to_string().contains("no such file or directory"));
}

#[tokio::test]
async fn glob_with_no_matches_returns_empty_success_not_an_error() {
    // A well-formed pattern that matches nothing is a valid empty result, not an
    // error — the model reads "no files" from empty output, not a failure.
    let dir = tempfile::tempdir().expect("tempdir");
    let out = tool_at("glob", dir.path())
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
    let out = tool_at("grep", dir.path())
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
async fn bash_shell_exit_preserves_diagnostics_status_and_next_call() {
    // X1 explicit exit 0/7, X2 set -e failure in this/a prior invocation,
    // X3 syntax failure, X4 signal -> drain both streams before classifying.
    // Exit 0 is success; numeric failure retains its status and diagnostics;
    // signals stay errors. Never run the sentinel-tail effect after failure,
    // never replay the old command; a new call starts a clean shell while its
    // workspace file survives. This recreates the live Cowork set -e failure.
    for (setup, command, expected_status, diagnostic) in [
        ("true", "printf saved; exit 0", Some(0), "saved"),
        (
            "true",
            "printf saved; printf detail >&2; exit 7",
            Some(7),
            "detail",
        ),
        (
            "true",
            "set -e; printf saved; printf detail >&2; false; touch replayed",
            Some(1),
            "detail",
        ),
        (
            "set -e",
            "printf saved; printf detail >&2; false; touch replayed",
            Some(1),
            "detail",
        ),
        ("true", "if ; then", Some(2), "syntax error"),
        #[cfg(unix)]
        ("true", "printf saved; kill -9 $$", None, "saved"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let bash = tool_at("bash", dir.path());
        bash.invoke(call("bash", serde_json::json!({"command": format!("printf durable > workspace-file; export OLD_STATE=yes; {setup}")}))).await.unwrap();
        let result = bash
            .invoke(call("bash", serde_json::json!({"command": command})))
            .await;
        match expected_status {
            Some(0) => assert_eq!(result.unwrap().text(), diagnostic),
            code => {
                let message = result.unwrap_err().to_string();
                assert!(message.contains(diagnostic), "{command}: {message}");
                if let Some(code) = code {
                    assert!(message.contains(&format!("exit {code}")), "{message}");
                    assert!(
                        !message.contains("session terminated"),
                        "numeric exit is known"
                    );
                } else {
                    assert!(message.contains("terminated by signal"), "{message}");
                }
            }
        }
        assert!(!dir.path().join("replayed").exists());
        let next = bash
            .invoke(call(
                "bash",
                serde_json::json!({"command": "printf '%s:' \"$OLD_STATE\"; cat workspace-file"}),
            ))
            .await
            .unwrap();
        assert_eq!(next.text(), ":durable", "fresh shell, same workspace");
    }
}

#[tokio::test]
async fn bash_persists_cwd_and_environment_and_restart_clears_both() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    let bash = tool_at("bash", dir.path());
    let initial = bash.invoke(call(
        "bash",
        serde_json::json!({ "command": "printf '%s' \"$PWD\"; cd nested; export AWAKEN_TEST_VALUE=kept" }),
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
                "command": "printf reset > restarted-workdir; printf '%s:%s' \"$PWD\" \"$AWAKEN_TEST_VALUE\""
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        restarted.text(),
        format!("{}:", initial.text()),
        "restart returns to the same physical workdir and clears shell state"
    );
    assert_eq!(
        std::fs::read(dir.path().join("restarted-workdir")).unwrap(),
        b"reset"
    );
    assert!(!dir.path().join("nested/restarted-workdir").exists());
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
    // Cause/effect rules: ANSI control sequences and NUL bytes emitted by a
    // command are not model-visible text and must be removed, while an output
    // over the existing byte limit still reports truncation. This keeps the
    // resulting ToolOutput valid for every durable JSON store.
    let bash = tool("bash");
    let output = bash
        .invoke(call(
            "bash",
            serde_json::json!({
                "command": "printf '\\033[31mred\\033[0m\\n'; head -c 110000 /dev/zero"
            }),
        ))
        .await
        .unwrap()
        .text();
    assert!(output.starts_with("[output truncated]\n"));
    assert!(!output.contains("\\u{1b}["));
    assert!(!output.contains('\0'));
    assert!(output.len() <= 100 * 1024 + "[output truncated]\n".len());
}

#[tokio::test]
async fn concurrent_huge_bash_outputs_stay_bounded_and_sessions_remain_usable() {
    // Cause/effect graph: C1 eight independent Bash sessions emit 2 MiB each;
    // C2 outputs complete concurrently; C3 each session receives a small
    // follow-up command. Effects: E1 every ToolOutput is capped at the existing
    // 100 KiB authority with an explicit truncation marker; E2 no task stalls;
    // E3 truncation does not poison persistent shell state. Decision table:
    // R1=C1&&!C2=>deadline failure; R2=C1+C2=>E1+E2;
    // R3=C1+C2+C3=>E1+E2+E3. The Bash tool's existing output limiter remains
    // the sole budget owner; this adds pressure, not a second limiter.
    let tasks = (0..8)
        .map(|worker| {
            tokio::spawn(async move {
                let bash = tool("bash");
                let output = bash
                    .invoke(call(
                        "bash",
                        serde_json::json!({
                            "command": "printf '%*s' 2097152 '' | tr ' ' x"
                        }),
                    ))
                    .await
                    .expect("large command completes")
                    .text();
                (worker, bash, output)
            })
        })
        .collect::<Vec<_>>();

    let results = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.expect("Bash stress task joins"));
        }
        results
    })
    .await
    .expect("R2/E2 concurrent output budget");

    for (worker, bash, output) in results {
        assert!(
            output.starts_with("[output truncated]\n"),
            "R2/E1 worker {worker}"
        );
        assert!(
            output.len() <= 100 * 1024 + "[output truncated]\n".len(),
            "R2/E1 worker {worker} returned {} bytes",
            output.len()
        );
        let follow_up = bash
            .invoke(call(
                "bash",
                serde_json::json!({ "command": format!("printf worker-{worker}") }),
            ))
            .await
            .expect("follow-up command completes");
        assert_eq!(follow_up.text(), format!("worker-{worker}"), "R3/E3");
    }
}

#[tokio::test]
async fn file_tools_reject_parent_absolute_and_symlink_escapes() {
    // Causal graph for the shared file-authority boundary:
    // C1 lexical parent escape -> reject before filesystem mutation.
    // C2 absolute path outside every trusted root -> reject.
    // C3 symlink resolving outside a trusted root -> reject.
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

async fn reject_foreign_file_root(
    root: &std::path::Path,
    directory: &std::path::Path,
    file: &std::path::Path,
    diagnostics: &[&str],
) {
    for (id, arguments) in [
        ("read", serde_json::json!({"file_path":file})),
        (
            "write",
            serde_json::json!({"file_path":file,"content":"must not write"}),
        ),
        (
            "edit",
            serde_json::json!({"file_path":file,"old_string":"original","new_string":"must not edit"}),
        ),
        ("glob", serde_json::json!({"path":directory,"pattern":"*"})),
        (
            "grep",
            serde_json::json!({"path":file,"pattern":"original"}),
        ),
    ] {
        let error = tool_at(id, root)
            .invoke(call(id, arguments))
            .await
            .expect_err("foreign root grants no file capability");
        assert!(
            matches!(error, ToolError::DefinitiveFailure(_)),
            "{id}: {error}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|text| error.to_string().contains(text)),
            "{id}: {error}"
        );
    }
}

#[tokio::test]
async fn every_file_tool_rejects_foreign_roots_without_mutation() {
    // F1 the native absolute path belongs to the declared root: existing success
    // rows execute real I/O, including a temp drive different from the checkout.
    // F2 another valid root; F3 parent traversal; F4 another Windows drive.
    // F2-F3 x all five tools must fail for confinement. F4 preserves a foreign
    // drive prefix, including an unavailable volume. No outside file changes.
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("sentinel.txt");
    std::fs::write(&file, "original").unwrap();
    reject_foreign_file_root(root.path(), outside.path(), &file, &["escapes workdir"]).await;
    assert_eq!(root.path().parent(), outside.path().parent());
    let parent = std::path::PathBuf::from("..").join(outside.path().file_name().unwrap());
    reject_foreign_file_root(
        root.path(),
        &parent,
        &parent.join("sentinel.txt"),
        &["escapes workdir"],
    )
    .await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
}

#[cfg(windows)]
#[tokio::test]
async fn every_file_tool_preserves_a_foreign_drive_prefix() {
    // No second volume or Developer Mode is required: a foreign drive prefix
    // itself cannot acquire a capability from the current physical root.
    use std::path::{Component, Prefix};
    let root = tempfile::tempdir().unwrap();
    let shadow = root.path().join("foreign-hand-root");
    std::fs::create_dir(&shadow).unwrap();
    std::fs::write(shadow.join("sentinel.txt"), "original").unwrap();
    let drive = match root.path().components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)
                if letter.eq_ignore_ascii_case(&b'Z') =>
            {
                'Y'
            }
            _ => 'Z',
        },
        _ => 'Z',
    };
    let directory = std::path::PathBuf::from(format!(r"{drive}:\foreign-hand-root"));
    reject_foreign_file_root(
        root.path(),
        &directory,
        &directory.join("sentinel.txt"),
        &["escapes workdir", "no such file or directory"],
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(shadow.join("sentinel.txt")).unwrap(),
        "original"
    );
    assert_eq!(std::fs::read_dir(&shadow).unwrap().count(), 1);
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

fn failing_shell_command() -> &'static str {
    "echo out-line; echo err-line >&2; false"
}

#[tokio::test]
async fn unknown_argument_shape_is_an_invalid_arguments_error() {
    // Decision table for the strong erasure boundary: R1 missing required field,
    // R2 a valid object with an undeclared field, and R3 a tuple with the wrong
    // cardinality all fail before filesystem execution. The same Rust type
    // generates the schema that declares these three constraints.
    let err = tool("read")
        .invoke(call("read", serde_json::json!({ "wrong": 1 })))
        .await
        .expect_err("bad args");
    assert!(matches!(err, ToolError::InvalidArguments(_)));

    let err = tool("read")
        .invoke(call(
            "read",
            serde_json::json!({ "file_path": "fixture", "unexpected": true }),
        ))
        .await
        .expect_err("undeclared args fail closed");
    assert!(matches!(err, ToolError::InvalidArguments(_)));

    let err = tool("read")
        .invoke(call(
            "read",
            serde_json::json!({ "file_path": "fixture", "view_range": [1] }),
        ))
        .await
        .expect_err("fixed-size view range is enforced by deserialization");
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn mutation_receipts_keep_the_resolved_logical_path_after_bash_changes_directory() {
    // W graph: relative/absolute/normalized input x direct/projected roots,
    // with persistent Bash cwd changed. W1 write still uses the FileContext;
    // W2 its receipt names the resolved logical target, usable by another tool;
    // W3 edit (one/all matches) returns that same target; W4 a host projection
    // never leaks physical storage paths. Existing escape/symlink/error tests
    // retain ownership of failed admission; this changes only success receipts.
    // The direct root expectation follows the host canonical path (macOS /var
    // may resolve through /private); projected output remains /managed.
    // W5 model-schema guidance must survive the one descriptor projection.
    // New relative output paths use the initial FileContext; an explicitly
    // writable absolute path remains compatible. Read-only input mount prefixes
    // are not another writable root (the Cowork headcount E2E reproduced three
    // parallel bad guesses). Existing W1-W4 execute every supported path form;
    // actual model compliance and its error budget stay in the real E2E.
    let descriptor = awaken_ext_builtin_tools::builtin_tools()
        .into_iter()
        .find(|tool| tool.descriptor().id == "write")
        .unwrap()
        .into_descriptor();
    let guidance = descriptor.parameters["properties"]["file_path"]["description"]
        .as_str()
        .unwrap();
    assert!(guidance.contains("initial working"));
    assert!(guidance.contains("read-only input mount"));
    assert!(guidance.contains("exact absolute path"));
    assert_eq!(
        descriptor.model_parameters()["properties"]["file_path"]["description"],
        guidance
    );
    for projected in [false, true] {
        for form in ["relative", "absolute", "normalized"] {
            let root = tempfile::tempdir().unwrap();
            let mount = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("nested")).unwrap();
            let context = if projected {
                HandToolContext::new(root.path())
                    .with_path_projection("/managed", mount.path())
                    .with_path_projection("outputs", mount.path())
            } else {
                HandToolContext::new(root.path())
            };
            let tools = all_hand_tools_in(context);
            let get = |id: &str| tools.iter().find(|tool| tool.id() == id).unwrap();
            get("bash")
                .invoke(call("bash", serde_json::json!({"command":"cd nested"})))
                .await
                .unwrap();
            let expected = if projected {
                "/managed/result.txt".to_string()
            } else {
                root.path()
                    .canonicalize()
                    .unwrap()
                    .join("result.txt")
                    .display()
                    .to_string()
            };
            let input = match (projected, form) {
                (_, "absolute") => expected.clone(),
                (true, "relative") => "outputs/result.txt".into(),
                (true, _) => "outputs/sub/../result.txt".into(),
                (false, "relative") => "result.txt".into(),
                (false, _) => "nested/../result.txt".into(),
            };
            let written = get("write")
                .invoke(call(
                    "write",
                    serde_json::json!({"file_path":input,"content":"甲 old old"}),
                ))
                .await
                .unwrap()
                .text();
            assert_eq!(
                written,
                format!("wrote 11 bytes to {expected}"),
                "{projected}/{form}"
            );
            for (old, new, all, suffix) in [
                ("甲", "乙", false, "1 replacement(s)"),
                ("old", "new", true, "2 replacements"),
            ] {
                let edited = get("edit").invoke(call("edit", serde_json::json!({"file_path":input,"old_string":old,"new_string":new,"replace_all":all}))).await.unwrap().text();
                assert_eq!(edited, format!("edited {expected} ({suffix})"));
            }
            let read = get("read")
                .invoke(call("read", serde_json::json!({"file_path":expected})))
                .await
                .unwrap();
            assert_eq!(read.text(), "乙 new new");
            assert!(!root.path().join("nested/result.txt").exists());
            if projected {
                assert!(!written.contains(&mount.path().display().to_string()));
            }
        }
    }
}
