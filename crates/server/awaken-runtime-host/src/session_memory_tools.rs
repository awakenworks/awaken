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
use awaken_runtime_contract::tool::{RawTool, Tool, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

#[derive(Clone)]
struct MemoryToolContext {
    bindings: Arc<HashMap<String, Arc<dyn SessionMemoryBinding>>>,
}

impl MemoryToolContext {
    fn binding(&self, id: &str) -> Result<&Arc<dyn SessionMemoryBinding>, ToolError> {
        let id = required("binding", id)?;
        self.bindings
            .get(id)
            .ok_or_else(|| ToolError::Execution(format!("unknown Session memory binding: {id}")))
    }
}

fn required<'a>(name: &str, value: &'a str) -> Result<&'a str, ToolError> {
    let value = value.trim();
    if value.is_empty() {
        Err(ToolError::InvalidArguments(format!(
            "the `{name}` argument is required"
        )))
    } else {
        Ok(value)
    }
}

fn default_prefix() -> String {
    "/".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListMemoriesArgs {
    /// Frozen Session MemoryStore binding id from the system prompt.
    binding: String,
    /// Path prefix within the selected memory store.
    #[serde(default = "default_prefix")]
    prefix: String,
}

#[derive(Debug, Serialize)]
struct ListMemoriesOutput {
    memories: Vec<awaken_resource_contract::MemoryEntry>,
}

struct ListMemoriesTool(MemoryToolContext);

#[async_trait]
impl Tool for ListMemoriesTool {
    type Args = ListMemoriesArgs;
    type Output = ListMemoriesOutput;
    const ID: &'static str = "list_memories";
    const DESCRIPTION: &'static str =
        "List memory metadata in one frozen Session MemoryStore binding.";

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        let binding = self.0.binding(&args.binding)?;
        let prefix = required("prefix", &args.prefix)?;
        let memories = binding.list(prefix).await.map_err(ToolError::Execution)?;
        Ok(ListMemoriesOutput { memories })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadMemoryArgs {
    /// Frozen Session MemoryStore binding id from the system prompt.
    binding: String,
    /// Absolute path within the selected memory store.
    path: String,
}

struct ReadMemoryTool(MemoryToolContext);

#[async_trait]
impl Tool for ReadMemoryTool {
    type Args = ReadMemoryArgs;
    type Output = awaken_resource_contract::Memory;
    const ID: &'static str = "read_memory";
    const DESCRIPTION: &'static str =
        "Read one memory and its id, content hash, version, and content.";

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        let binding = self.0.binding(&args.binding)?;
        let path = required("path", &args.path)?;
        binding
            .read(path)
            .await
            .map_err(ToolError::Execution)?
            .ok_or_else(|| ToolError::Execution(format!("memory not found: {path}")))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteMemoryArgs {
    /// Frozen Session MemoryStore binding id from the system prompt.
    binding: String,
    /// Absolute path within the selected memory store.
    path: String,
    /// Complete replacement content.
    content: String,
    /// Hash returned by read_memory; required when updating an existing path.
    #[serde(default)]
    expected_sha256: Option<String>,
}

struct WriteMemoryTool(MemoryToolContext);

#[async_trait]
impl Tool for WriteMemoryTool {
    type Args = WriteMemoryArgs;
    type Output = awaken_resource_contract::Memory;
    const ID: &'static str = "write_memory";
    const DESCRIPTION: &'static str = "Create or compare-and-swap one memory. Omit expected_sha256 only for create-only semantics.";

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        let binding = self.0.binding(&args.binding)?;
        let path = required("path", &args.path)?;
        binding
            .write(path, &args.content, args.expected_sha256.as_deref())
            .await
            .map_err(ToolError::Execution)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteMemoryArgs {
    /// Frozen Session MemoryStore binding id from the system prompt.
    binding: String,
    /// Absolute path within the selected memory store.
    path: String,
    /// Exact memory id returned by read_memory.
    expected_id: String,
    /// Exact content hash returned by read_memory.
    expected_sha256: String,
}

#[derive(Debug, Serialize)]
struct DeleteMemoryOutput {
    deleted: bool,
}

struct DeleteMemoryTool(MemoryToolContext);

#[async_trait]
impl Tool for DeleteMemoryTool {
    type Args = DeleteMemoryArgs;
    type Output = DeleteMemoryOutput;
    const ID: &'static str = "delete_memory";
    const DESCRIPTION: &'static str =
        "Compare-and-delete one memory using the exact id and hash returned by read_memory.";

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        let binding = self.0.binding(&args.binding)?;
        let path = required("path", &args.path)?;
        let expected_id = required("expected_id", &args.expected_id)?;
        let expected_sha256 = required("expected_sha256", &args.expected_sha256)?;
        let deleted = binding
            .delete(path, expected_id, expected_sha256)
            .await
            .map_err(ToolError::Execution)?;
        Ok(DeleteMemoryOutput { deleted })
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
        let context = MemoryToolContext {
            bindings: Arc::new(bindings),
        };
        Some(Self {
            descriptors: vec![
                ToolDescriptor::for_tool::<ListMemoriesTool>("session-memory"),
                ToolDescriptor::for_tool::<ReadMemoryTool>("session-memory"),
                ToolDescriptor::for_tool::<WriteMemoryTool>("session-memory"),
                ToolDescriptor::for_tool::<DeleteMemoryTool>("session-memory"),
            ],
            executors: vec![
                awaken_ext_builtin_tools::erase(ListMemoriesTool(context.clone())),
                awaken_ext_builtin_tools::erase(ReadMemoryTool(context.clone())),
                awaken_ext_builtin_tools::erase(WriteMemoryTool(context.clone())),
                awaken_ext_builtin_tools::erase(DeleteMemoryTool(context)),
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::tool::ToolCall;
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
            let error = read
                .invoke(ToolCall {
                    call_id: "denied".into(),
                    tool_id: "read_memory".into(),
                    arguments,
                })
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    ToolError::InvalidArguments(_) | ToolError::Execution(_)
                ),
                "R1/R2: {error}"
            );
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

    #[test]
    fn descriptors_are_generated_from_the_typed_memory_arguments() {
        // Schema cause/effect table: T1 each typed Tool implementation contributes
        // its ID, description, and Args-derived schema; T2 a field rename/addition
        // changes that generated descriptor; T3 no handwritten schema authority
        // exists to drift. Effect: the Session surface exactly equals the four
        // canonical `ToolDescriptor::for_tool` values in execution order.
        let wiring = SessionMemoryTools::from_bindings(HashMap::from([(
            "memory".into(),
            Arc::new(FakeBinding {
                label: "memory",
                calls: Mutex::new(Vec::new()),
            }) as Arc<dyn SessionMemoryBinding>,
        )]))
        .unwrap();
        assert_eq!(
            wiring.descriptors,
            vec![
                ToolDescriptor::for_tool::<ListMemoriesTool>("session-memory"),
                ToolDescriptor::for_tool::<ReadMemoryTool>("session-memory"),
                ToolDescriptor::for_tool::<WriteMemoryTool>("session-memory"),
                ToolDescriptor::for_tool::<DeleteMemoryTool>("session-memory"),
            ]
        );
        assert_eq!(
            wiring
                .executors
                .iter()
                .map(|tool| tool.id())
                .collect::<Vec<_>>(),
            [
                ListMemoriesTool::ID,
                ReadMemoryTool::ID,
                WriteMemoryTool::ID,
                DeleteMemoryTool::ID,
            ]
        );
    }
}
