//! The `write_memory` tool: a scoped write the memory extractor calls to persist a
//! durable memory outside its ephemeral sandbox.

use async_trait::async_trait;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{Tool, ToolError};

use crate::localfs::{MemoryDir, MemoryStoreHandle};
use std::sync::Arc;

/// Strong input contract shared by schema generation and execution.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteMemoryArgs {
    /// Short slug naming the memory.
    pub name: String,
    /// Durable memory text.
    pub content: String,
}

/// Writes one memory to a stable store. Constructed with the store so the write
/// lands in the persistent memory directory, not the sub-run's sandbox.
pub struct WriteMemoryTool {
    store: Arc<dyn MemoryStoreHandle>,
}

impl WriteMemoryTool {
    pub fn new(store: MemoryDir) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    pub fn from_handle(store: Arc<dyn MemoryStoreHandle>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WriteMemoryTool {
    type Args = WriteMemoryArgs;
    type Output = String;
    const ID: &'static str = "write_memory";
    const DESCRIPTION: &'static str = "Save one durable memory. `name` is a short slug; \
        `content` is the memory text. Call once per distinct memory.";

    async fn call(&self, args: WriteMemoryArgs) -> Result<String, ToolError> {
        let WriteMemoryArgs { name, content } = args;
        // Model-independent backstop for the extractor's "what NOT to save" gate.
        // Skip an implementation note here — NOT an error, so the extractor's
        // other valid writes still land.
        if !accepts_memory_content(&content) {
            return Ok(format!(
                "skipped `{name}`: implementation detail (code / a fix) belongs in the repo, not memory"
            ));
        }
        let path = self
            .store
            .write(&name, &content)
            .await
            .map_err(|e| ToolError::Execution(format!("write memory: {e}")))?;
        Ok(format!("saved memory {path}"))
    }
}

/// Whether a proposed memory is an IMPLEMENTATION note (the "how it was built/fixed") that
/// belongs in the code and git, not in durable memory. A HIGH-PRECISION filter — it fires
/// only on a bug-diagnosis / code-change phrase or a SOURCE-CODE filename (code extensions
/// only, so a `reference` memory citing `openapi.yaml` / `settings.toml` / a doc survives).
/// Durable facts — decisions ("decided to migrate…"), preferences, external pointers — pass.
fn looks_like_implementation_note(content: &str) -> bool {
    let c = content.to_ascii_lowercase();
    // A diagnosis or a code-change ACTION — never a durable fact.
    const IMPL_PHRASES: &[&str] = &[
        "the bug was",
        "the bug is",
        "the issue was",
        "root cause",
        "missing index",
        "off-by-one",
        "race condition",
        "null pointer",
        "fixed it by",
        "fixed the bug",
        "refactored",
        "rewrote",
        "reimplemented",
        "patched",
        "hotfix",
        "wired it",
    ];
    if IMPL_PHRASES.iter().any(|p| c.contains(p)) {
        return true;
    }
    // A source-code file name (code extensions only — `.yaml`/`.toml`/`.md` docs & config
    // are legitimate reference memories and are NOT treated as implementation).
    const CODE_EXT: &[&str] = &[
        ".rs", ".go", ".py", ".ts", ".tsx", ".js", ".jsx", ".java", ".rb", ".cpp", ".cc", ".c",
        ".h", ".hpp", ".cs", ".kt", ".swift", ".php", ".scala", ".sql",
    ];
    c.split(|ch: char| ch.is_whitespace() || matches!(ch, '(' | ')' | ',' | '`' | ';' | ':'))
        .any(|tok| {
            CODE_EXT
                .iter()
                .any(|ext| tok.ends_with(ext) && tok.len() > ext.len())
        })
}

/// Deterministic policy backstop shared by live tool execution and recovery
/// from a committed auxiliary Agent transcript.
#[must_use]
pub fn accepts_memory_content(content: &str) -> bool {
    !looks_like_implementation_note(content)
}

/// The model-visible descriptor for [`WriteMemoryTool`].
pub fn write_memory_descriptor() -> ToolDescriptor {
    ToolDescriptor::for_tool::<WriteMemoryTool>("ext-memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn write_memory_saves_under_the_store_root() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("awaken-writetool-{stamp}"));
        let tool = WriteMemoryTool::new(MemoryDir::new(&root));
        let out = tool
            .call(WriteMemoryArgs {
                name: "pref".into(),
                content: "likes tea".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("saved memory"));
        assert_eq!(
            std::fs::read_to_string(root.join("pref.md")).unwrap(),
            "likes tea"
        );
    }

    #[test]
    fn implementation_notes_are_detected_but_durable_facts_pass() {
        // Drops — a code-change action or a bug diagnosis, or a source-code filename.
        assert!(looks_like_implementation_note(
            "The queue.rs module was refactored to use a PostgreSQL table."
        ));
        assert!(looks_like_implementation_note(
            "Fixed the auth timeout; the bug was a missing index in the tokens table."
        ));
        assert!(looks_like_implementation_note(
            "Wired it up in rpc/server.go."
        ));
        // Passes — durable facts, even when they mention config/doc files or the word "code".
        assert!(!looks_like_implementation_note(
            "Decided to migrate the job queue from Redis to Postgres (locked last sprint)."
        ));
        assert!(!looks_like_implementation_note(
            "The API spec lives in openapi.yaml; the runbook is in Confluence."
        ));
        assert!(!looks_like_implementation_note(
            "The user prefers terse code review."
        ));
        assert!(!looks_like_implementation_note(
            "Deploys go through staging first, never prod."
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_memory_skips_an_implementation_note_without_persisting_it() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("awaken-implskip-{stamp}"));
        let tool = WriteMemoryTool::new(MemoryDir::new(&root));
        // The exact over-extraction the weak model produces — a second memory about the file.
        let out = tool
            .call(WriteMemoryArgs {
                name: "queue-rs-refactor".into(),
                content: "The queue.rs module was refactored to use a pg table.".into(),
            })
            .await
            .unwrap();
        assert!(out.contains("skipped"), "{out}");
        assert!(
            !root.join("queue-rs-refactor.md").exists(),
            "must not persist"
        );
        // A durable decision alongside it still lands.
        let ok = tool
            .call(WriteMemoryArgs {
                name: "queue-to-postgres".into(),
                content: "Migrated the job queue from Redis to Postgres.".into(),
            })
            .await
            .unwrap();
        assert!(ok.contains("saved memory"), "{ok}");
    }

    #[test]
    fn missing_fields_are_rejected_at_the_single_erasure_boundary() {
        use awaken_runtime_contract::tool::parse_tool_args;

        assert!(parse_tool_args::<WriteMemoryArgs>(serde_json::json!({ "name": "a" })).is_err());
        assert!(parse_tool_args::<WriteMemoryArgs>(serde_json::json!({ "content": "b" })).is_err());
    }

    #[test]
    fn non_string_name_or_content_is_rejected_at_the_single_erasure_boundary() {
        use awaken_runtime_contract::tool::parse_tool_args;

        for name in [
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::json!(["a"]),
            serde_json::json!({ "k": "v" }),
            serde_json::Value::Null,
        ] {
            let err = parse_tool_args::<WriteMemoryArgs>(
                serde_json::json!({ "name": name, "content": "ok" }),
            );
            assert!(
                matches!(err, Err(ToolError::InvalidArguments(_))),
                "name {err:?}"
            );
        }
        for content in [
            serde_json::json!(42.0),
            serde_json::json!(false),
            serde_json::json!([]),
            serde_json::Value::Null,
        ] {
            let err = parse_tool_args::<WriteMemoryArgs>(
                serde_json::json!({ "name": "ok", "content": content }),
            );
            assert!(
                matches!(err, Err(ToolError::InvalidArguments(_))),
                "content {err:?}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_content_is_written_whole_while_an_oversized_name_is_clamped() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("awaken-writetool-big-{stamp}"));
        let tool = WriteMemoryTool::new(MemoryDir::new(&root));

        // Content is never capped by the tool: a large body is persisted verbatim.
        let big = "Z".repeat(200_000);
        // The name is far longer than the 120-char stem bound: it is sanitized and
        // truncated to a single safe stem, so the write still lands under root.
        let long_name = "n".repeat(500);
        let out = tool
            .call(WriteMemoryArgs {
                name: long_name,
                content: big,
            })
            .await
            .unwrap();
        assert!(out.contains("saved memory"));

        // Exactly one file was written, its stem clamped to the 120-char bound, and
        // it holds the full oversized content unchanged.
        let entries = tool.store.entries().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content.len(), 200_000);
        let stem = entries[0].path.file_stem().unwrap().to_string_lossy();
        assert_eq!(
            stem.chars().count(),
            120,
            "stem clamped to the length bound"
        );
    }
}
