//! Filesystem-free projection of frozen Session MemoryStore bindings.
//!
//! The tools carry only Session binding ids. They never accept a Workspace id,
//! physical store id, or repository handle, so callers cannot widen the frozen
//! Resource authority. All writes and deletes preserve the repository's CAS
//! semantics.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};

#[async_trait]
pub(crate) trait SessionMemoryBinding: Send + Sync {
    async fn list(
        &self,
        prefix: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, String>;
    async fn read(&self, path: &str) -> Result<Option<awaken_resource_contract::Memory>, String>;
    async fn write(
        &self,
        path: &str,
        content: &str,
        expected_sha256: Option<&str>,
    ) -> Result<awaken_resource_contract::Memory, String>;
    async fn delete(
        &self,
        path: &str,
        expected_id: &str,
        expected_sha256: &str,
    ) -> Result<bool, String>;
}

#[async_trait]
impl SessionMemoryBinding for crate::memory::BoundMemory {
    async fn list(
        &self,
        prefix: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, String> {
        self.list_bound_memories(prefix).await
    }

    async fn read(&self, path: &str) -> Result<Option<awaken_resource_contract::Memory>, String> {
        self.read_bound_memory(path).await
    }

    async fn write(
        &self,
        path: &str,
        content: &str,
        expected_sha256: Option<&str>,
    ) -> Result<awaken_resource_contract::Memory, String> {
        self.write_bound_memory(path, content, expected_sha256)
            .await
    }

    async fn delete(
        &self,
        path: &str,
        expected_id: &str,
        expected_sha256: &str,
    ) -> Result<bool, String> {
        self.delete_bound_memory(path, expected_id, expected_sha256)
            .await
    }
}

#[derive(Clone, Copy)]
enum MemoryOperation {
    List,
    Read,
    Write,
    Delete,
}

impl MemoryOperation {
    const fn id(self) -> &'static str {
        match self {
            Self::List => "list_memories",
            Self::Read => "read_memory",
            Self::Write => "write_memory",
            Self::Delete => "delete_memory",
        }
    }
}

struct MemoryTool {
    operation: MemoryOperation,
    bindings: Arc<HashMap<String, Arc<dyn SessionMemoryBinding>>>,
}

impl MemoryTool {
    fn binding(&self, call: &ToolCall) -> Result<&Arc<dyn SessionMemoryBinding>, String> {
        let id = call
            .arguments
            .get("binding")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "the `binding` argument is required".to_string())?;
        self.bindings
            .get(id)
            .ok_or_else(|| format!("unknown Session memory binding: {id}"))
    }

    fn string_arg<'a>(call: &'a ToolCall, name: &str) -> Result<&'a str, String> {
        call.arguments
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("the `{name}` argument is required"))
    }
}

#[async_trait]
impl RawTool for MemoryTool {
    fn id(&self) -> &str {
        self.operation.id()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let result = async {
            let binding = self.binding(&call)?;
            match self.operation {
                MemoryOperation::List => {
                    let prefix = call
                        .arguments
                        .get("prefix")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("/");
                    let memories = binding.list(prefix).await?;
                    serde_json::to_string(&serde_json::json!({"memories": memories}))
                        .map_err(|error| error.to_string())
                }
                MemoryOperation::Read => {
                    let path = Self::string_arg(&call, "path")?;
                    let memory = binding
                        .read(path)
                        .await?
                        .ok_or_else(|| format!("memory not found: {path}"))?;
                    serde_json::to_string(&memory).map_err(|error| error.to_string())
                }
                MemoryOperation::Write => {
                    let path = Self::string_arg(&call, "path")?;
                    let content = call
                        .arguments
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "the `content` argument is required".to_string())?;
                    let expected = call
                        .arguments
                        .get("expected_sha256")
                        .and_then(serde_json::Value::as_str);
                    let memory = binding.write(path, content, expected).await?;
                    serde_json::to_string(&memory).map_err(|error| error.to_string())
                }
                MemoryOperation::Delete => {
                    let path = Self::string_arg(&call, "path")?;
                    let expected_id = Self::string_arg(&call, "expected_id")?;
                    let expected_sha256 = Self::string_arg(&call, "expected_sha256")?;
                    let deleted = binding.delete(path, expected_id, expected_sha256).await?;
                    Ok(serde_json::json!({"deleted": deleted}).to_string())
                }
            }
        }
        .await;
        Ok(match result {
            Ok(content) => ToolOutput::ok(call.call_id, content),
            Err(error) => ToolOutput::error(call.call_id, error),
        })
    }
}

pub(crate) struct SessionMemoryTools {
    pub descriptors: Vec<ToolDescriptor>,
    pub executors: Vec<Arc<dyn RawTool>>,
}

impl SessionMemoryTools {
    pub(crate) fn new(bindings: HashMap<String, Arc<crate::memory::BoundMemory>>) -> Option<Self> {
        let bindings: HashMap<String, Arc<dyn SessionMemoryBinding>> = bindings
            .into_iter()
            .map(|(id, binding)| (id, binding as Arc<dyn SessionMemoryBinding>))
            .collect();
        Self::from_bindings(bindings)
    }

    fn from_bindings(bindings: HashMap<String, Arc<dyn SessionMemoryBinding>>) -> Option<Self> {
        if bindings.is_empty() {
            return None;
        }
        let bindings = Arc::new(bindings);
        let operations = [
            MemoryOperation::List,
            MemoryOperation::Read,
            MemoryOperation::Write,
            MemoryOperation::Delete,
        ];
        Some(Self {
            descriptors: operations.into_iter().map(descriptor).collect(),
            executors: operations
                .into_iter()
                .map(|operation| {
                    Arc::new(MemoryTool {
                        operation,
                        bindings: bindings.clone(),
                    }) as Arc<dyn RawTool>
                })
                .collect(),
        })
    }
}

fn descriptor(operation: MemoryOperation) -> ToolDescriptor {
    let binding = serde_json::json!({
        "type": "string",
        "description": "Frozen Session MemoryStore binding id from the system prompt."
    });
    let path = serde_json::json!({"type": "string", "description": "Absolute path within the selected memory store."});
    let (description, schema) = match operation {
        MemoryOperation::List => (
            "List memory metadata in one frozen Session MemoryStore binding.",
            serde_json::json!({
                "type": "object",
                "properties": {"binding": binding, "prefix": {"type": "string", "default": "/"}},
                "required": ["binding"]
            }),
        ),
        MemoryOperation::Read => (
            "Read one memory and its id, content hash, version, and content.",
            serde_json::json!({
                "type": "object",
                "properties": {"binding": binding, "path": path},
                "required": ["binding", "path"]
            }),
        ),
        MemoryOperation::Write => (
            "Create or compare-and-swap one memory. Omit expected_sha256 only for create-only semantics.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "binding": binding,
                    "path": path,
                    "content": {"type": "string"},
                    "expected_sha256": {"type": "string", "description": "Hash returned by read_memory; required when updating an existing path."}
                },
                "required": ["binding", "path", "content"]
            }),
        ),
        MemoryOperation::Delete => (
            "Compare-and-delete one memory using the exact id and hash returned by read_memory.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "binding": binding,
                    "path": path,
                    "expected_id": {"type": "string"},
                    "expected_sha256": {"type": "string"}
                },
                "required": ["binding", "path", "expected_id", "expected_sha256"]
            }),
        ),
    };
    ToolDescriptor::pinned("session-memory", operation.id(), description, schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeBinding {
        label: &'static str,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SessionMemoryBinding for FakeBinding {
        async fn list(
            &self,
            prefix: &str,
        ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, String> {
            self.calls.lock().unwrap().push(format!("list:{prefix}"));
            Ok(Vec::new())
        }

        async fn read(
            &self,
            path: &str,
        ) -> Result<Option<awaken_resource_contract::Memory>, String> {
            self.calls.lock().unwrap().push(format!("read:{path}"));
            Ok(Some(awaken_resource_contract::Memory {
                id: self.label.into(),
                path: path.into(),
                content_sha256: "sha".into(),
                content_size: 1,
                version: 1,
                created_unix_nanos: 0,
                updated_unix_nanos: 0,
                content: Some(self.label.into()),
            }))
        }

        async fn write(
            &self,
            _path: &str,
            _content: &str,
            _expected_sha256: Option<&str>,
        ) -> Result<awaken_resource_contract::Memory, String> {
            Err("unused".into())
        }

        async fn delete(
            &self,
            _path: &str,
            _expected_id: &str,
            _expected_sha256: &str,
        ) -> Result<bool, String> {
            Err("unused".into())
        }
    }

    #[tokio::test]
    async fn every_operation_requires_an_explicit_session_binding() {
        // Cause/effect decision table: R1 missing binding -> no store call and
        // model-visible error; R2 unknown binding -> no store call and error;
        // R3 one of multiple exact bindings -> only that binding receives the
        // operation. This is the cross-store isolation invariant.
        let alpha = Arc::new(FakeBinding {
            label: "alpha",
            calls: Mutex::new(Vec::new()),
        });
        let beta = Arc::new(FakeBinding {
            label: "beta",
            calls: Mutex::new(Vec::new()),
        });
        let wiring = SessionMemoryTools::from_bindings(HashMap::from([
            (
                "alpha".into(),
                alpha.clone() as Arc<dyn SessionMemoryBinding>,
            ),
            ("beta".into(), beta.clone() as Arc<dyn SessionMemoryBinding>),
        ]))
        .unwrap();
        let read = wiring
            .executors
            .iter()
            .find(|tool| tool.id() == "read_memory")
            .unwrap();
        for arguments in [
            serde_json::json!({"path": "/note.md"}),
            serde_json::json!({"binding": "unknown", "path": "/note.md"}),
        ] {
            let output = read
                .invoke(ToolCall {
                    call_id: "denied".into(),
                    tool_id: "read_memory".into(),
                    arguments,
                })
                .await
                .unwrap();
            assert!(output.is_error, "R1/R2");
        }
        let output = read
            .invoke(ToolCall {
                call_id: "selected".into(),
                tool_id: "read_memory".into(),
                arguments: serde_json::json!({"binding": "beta", "path": "/note.md"}),
            })
            .await
            .unwrap();
        assert!(!output.is_error, "R3");
        assert!(output.text().contains("beta"));
        assert!(alpha.calls.lock().unwrap().is_empty());
        assert_eq!(&*beta.calls.lock().unwrap(), &["read:/note.md"]);
    }
}
