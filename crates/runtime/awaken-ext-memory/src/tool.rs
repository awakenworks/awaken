//! The `write_memory` tool: a scoped write the memory extractor calls to persist a
//! durable memory outside its ephemeral sandbox.

use async_trait::async_trait;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{Tool, ToolError};

use crate::localfs::MemoryDir;

/// Writes one memory to a stable store. Constructed with the store so the write
/// lands in the persistent memory directory, not the sub-run's sandbox.
pub struct WriteMemoryTool {
    store: MemoryDir,
}

impl WriteMemoryTool {
    pub fn new(store: MemoryDir) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for WriteMemoryTool {
    // A JSON value rather than a derived struct keeps the arg shape flexible and
    // matches how other in-tree tools read loosely-typed input.
    type Args = serde_json::Value;
    type Output = String;

    fn id(&self) -> &str {
        "write_memory"
    }

    async fn call(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("write_memory needs a `name`".into()))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("write_memory needs `content`".into()))?;
        let path = self
            .store
            .write(name, content)
            .map_err(|e| ToolError::Execution(format!("write memory: {e}")))?;
        Ok(format!("saved memory {}", path.display()))
    }
}

/// The model-visible descriptor for [`WriteMemoryTool`].
pub fn write_memory_descriptor() -> ToolDescriptor {
    ToolDescriptor::pinned(
        "ext-memory",
        "write_memory",
        "Save one durable memory. `name` is a short slug; `content` is the memory text. \
         Call once per distinct memory.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "short slug naming the memory" },
                "content": { "type": "string", "description": "the memory text" }
            },
            "required": ["name", "content"]
        }),
    )
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
            .call(serde_json::json!({ "name": "pref", "content": "likes tea" }))
            .await
            .unwrap();
        assert!(out.contains("saved memory"));
        assert_eq!(
            std::fs::read_to_string(root.join("pref.md")).unwrap(),
            "likes tea"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_fields_are_rejected() {
        let tool = WriteMemoryTool::new(MemoryDir::new(std::env::temp_dir().join("x")));
        assert!(tool.call(serde_json::json!({ "name": "a" })).await.is_err());
        assert!(
            tool.call(serde_json::json!({ "content": "b" }))
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_string_name_or_content_is_rejected() {
        // `as_str()` yields None for any non-string JSON, so the tool rejects it as
        // invalid arguments rather than coercing (numbers, bools, arrays, objects,
        // and explicit null all fail the same way).
        let tool = WriteMemoryTool::new(MemoryDir::new(std::env::temp_dir().join("nonstring")));
        for name in [
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::json!(["a"]),
            serde_json::json!({ "k": "v" }),
            serde_json::Value::Null,
        ] {
            let err = tool
                .call(serde_json::json!({ "name": name, "content": "ok" }))
                .await;
            assert!(
                matches!(err, Err(ToolError::InvalidArguments(_))),
                "name {err:?}"
            );
        }
        for content in [
            serde_json::json!(3.14),
            serde_json::json!(false),
            serde_json::json!([]),
            serde_json::Value::Null,
        ] {
            let err = tool
                .call(serde_json::json!({ "name": "ok", "content": content }))
                .await;
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
            .call(serde_json::json!({ "name": long_name, "content": big }))
            .await
            .unwrap();
        assert!(out.contains("saved memory"));

        // Exactly one file was written, its stem clamped to the 120-char bound, and
        // it holds the full oversized content unchanged.
        let entries = tool.store.entries();
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
