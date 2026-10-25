// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Projects frozen Session Memory binding ids into semantic tools while all CAS writes and
// @formal reason: durable heads remain owned by the canonical Memory repository.
//! Filesystem-free Memory tools selected from frozen contracts and Session bindings.
//!
//! Store tools carry only Session binding ids. They never accept a Workspace id,
//! physical store id, or repository handle, so callers cannot widen the frozen
//! Resource authority. Store writes and deletes preserve repository CAS. An
//! extractor instead selects the SDK's stateless proposal tool through its
//! frozen descriptor; the two same-named write contracts never coexist.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::{
    ExecutableAgentSnapshot,
    resolved::{ToolDescriptor, ToolKind},
};
use awaken_session_run_delivery::MemoryWriteProposals;
use awaken_tool_contract::tool::{RawTool, Tool, ToolError};
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
        expected_generation: Option<&awaken_resource_contract::MemoryGeneration>,
    ) -> Result<awaken_resource_contract::Memory, String>;
    async fn delete(
        &self,
        path: &str,
        expected_generation: &awaken_resource_contract::MemoryGeneration,
    ) -> Result<bool, String>;
}

#[async_trait]
impl SessionMemoryBinding for crate::memory_binding::MemoryBinding {
    async fn list(
        &self,
        prefix: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, String> {
        self.validate_live_resource().await?;
        self.platform().list(prefix).await
    }

    async fn read(&self, path: &str) -> Result<Option<awaken_resource_contract::Memory>, String> {
        self.validate_live_resource().await?;
        self.platform().read(path).await
    }

    async fn write(
        &self,
        path: &str,
        content: &str,
        expected_generation: Option<&awaken_resource_contract::MemoryGeneration>,
    ) -> Result<awaken_resource_contract::Memory, String> {
        self.validate_live_resource().await?;
        self.platform()
            .write_exact(path, content, expected_generation)
            .await
    }

    async fn delete(
        &self,
        path: &str,
        expected_generation: &awaken_resource_contract::MemoryGeneration,
    ) -> Result<bool, String> {
        self.validate_live_resource().await?;
        self.platform()
            .delete_exact(path, expected_generation)
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
        self.bindings.get(id).ok_or_else(|| {
            ToolError::DefinitiveFailure(format!("unknown Session memory binding: {id}"))
        })
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
        let memories = binding
            .list(prefix)
            .await
            .map_err(ToolError::DefinitiveFailure)?;
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
            .map_err(ToolError::DefinitiveFailure)?
            .ok_or_else(|| ToolError::DefinitiveFailure(format!("memory not found: {path}")))
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
    /// Complete generation returned by read_memory; required for an update.
    #[serde(default)]
    expected_generation: Option<MemoryGenerationArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoryGenerationArgs {
    memory_id: String,
    version: u64,
    content_sha256: String,
}

impl From<MemoryGenerationArgs> for awaken_resource_contract::MemoryGeneration {
    fn from(value: MemoryGenerationArgs) -> Self {
        Self::new(value.memory_id, value.version, value.content_sha256)
    }
}

struct WriteMemoryTool(MemoryToolContext);

#[async_trait]
impl Tool for WriteMemoryTool {
    type Args = WriteMemoryArgs;
    type Output = awaken_resource_contract::Memory;
    const ID: &'static str = "write_memory";
    const DESCRIPTION: &'static str = "Create or compare-and-swap one memory. Omit expected_generation only for create-only semantics.";

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        let binding = self.0.binding(&args.binding)?;
        let path = required("path", &args.path)?;
        let expected_generation = args.expected_generation.map(Into::into);
        binding
            .write(path, &args.content, expected_generation.as_ref())
            .await
            .map_err(ToolError::DefinitiveFailure)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeleteMemoryArgs {
    /// Frozen Session MemoryStore binding id from the system prompt.
    binding: String,
    /// Absolute path within the selected memory store.
    path: String,
    /// Complete generation returned by read_memory.
    expected_generation: MemoryGenerationArgs,
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
        "Compare-and-delete one memory using the exact generation returned by read_memory.";

    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
        let binding = self.0.binding(&args.binding)?;
        let path = required("path", &args.path)?;
        let expected_generation = args.expected_generation.into();
        let deleted = binding
            .delete(path, &expected_generation)
            .await
            .map_err(ToolError::DefinitiveFailure)?;
        Ok(DeleteMemoryOutput { deleted })
    }
}

pub(crate) struct SessionMemoryTools {
    pub descriptors: Vec<ToolDescriptor>,
    pub executors: Vec<Arc<dyn RawTool>>,
}

impl SessionMemoryTools {
    pub(crate) fn new(
        bindings: HashMap<String, Arc<crate::memory_binding::MemoryBinding>>,
        snapshot: Option<&ExecutableAgentSnapshot>,
    ) -> Option<Self> {
        let bindings: HashMap<String, Arc<dyn SessionMemoryBinding>> = bindings
            .into_iter()
            .map(|(id, binding)| (id, binding as Arc<dyn SessionMemoryBinding>))
            .collect();
        Self::from_bindings(bindings, snapshot)
    }

    fn from_bindings(
        bindings: HashMap<String, Arc<dyn SessionMemoryBinding>>,
        snapshot: Option<&ExecutableAgentSnapshot>,
    ) -> Option<Self> {
        let proposal = snapshot.and_then(MemoryWriteProposals::descriptor);
        if bindings.is_empty() && proposal.is_none() {
            return None;
        }
        let stored_write = ToolDescriptor::for_tool::<WriteMemoryTool>("session-memory");
        let declared_write = snapshot.and_then(|snapshot| {
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .find(|descriptor| descriptor.id == stored_write.id)
        });
        // A binding-derived dynamic surface must not replace an explicitly
        // declared proposal, client-owned or otherwise differently-owned tool.
        let store_write_selected = declared_write.is_none_or(|descriptor| {
            descriptor.content_namespace() == stored_write.content_namespace()
                && descriptor.kind == ToolKind::Regular
                && descriptor.provider_server_tool.is_none()
        });
        let context = (!bindings.is_empty()).then(|| MemoryToolContext {
            bindings: Arc::new(bindings),
        });
        let mut descriptors = Vec::new();
        let mut executors = Vec::new();
        if let Some(context) = &context {
            descriptors.extend([
                ToolDescriptor::for_tool::<ListMemoriesTool>("session-memory"),
                ToolDescriptor::for_tool::<ReadMemoryTool>("session-memory"),
            ]);
            executors.extend([
                awaken_ext_builtin_tools::erase(ListMemoriesTool(context.clone())),
                awaken_ext_builtin_tools::erase(ReadMemoryTool(context.clone())),
            ]);
        }
        if let Some(descriptor) = proposal {
            descriptors.push(descriptor.clone());
            executors.push(awaken_ext_builtin_tools::erase(
                awaken_ext_memory::WriteMemoryTool::from_handle(Arc::new(MemoryWriteProposals)),
            ));
        } else if let Some(context) = &context
            && store_write_selected
        {
            descriptors.push(stored_write);
            executors.push(awaken_ext_builtin_tools::erase(WriteMemoryTool(
                context.clone(),
            )));
        }
        if let Some(context) = context {
            descriptors.push(ToolDescriptor::for_tool::<DeleteMemoryTool>(
                "session-memory",
            ));
            executors.push(awaken_ext_builtin_tools::erase(DeleteMemoryTool(context)));
        }
        Some(Self {
            descriptors,
            executors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_tool_contract::tool::ToolCall;
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
            _expected_generation: Option<&awaken_resource_contract::MemoryGeneration>,
        ) -> Result<awaken_resource_contract::Memory, String> {
            Err("unused".into())
        }

        async fn delete(
            &self,
            _path: &str,
            _expected_generation: &awaken_resource_contract::MemoryGeneration,
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
        let wiring = SessionMemoryTools::from_bindings(
            HashMap::from([
                (
                    "alpha".into(),
                    alpha.clone() as Arc<dyn SessionMemoryBinding>,
                ),
                ("beta".into(), beta.clone() as Arc<dyn SessionMemoryBinding>),
            ]),
            None,
        )
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
                    ToolError::InvalidArguments(_) | ToolError::DefinitiveFailure(_)
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
        let wiring = SessionMemoryTools::from_bindings(
            HashMap::from([(
                "memory".into(),
                Arc::new(FakeBinding {
                    label: "memory",
                    calls: Mutex::new(Vec::new()),
                }) as Arc<dyn SessionMemoryBinding>,
            )]),
            None,
        )
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

    #[tokio::test]
    async fn one_memory_writer_is_selected_by_frozen_contract_not_display_name() {
        // Cause/effect matrix: C1 bindings absent/present; C2 declaration is
        // absent, stored-write, proposal, client-owned, foreign, detached, or
        // provider-native. O1 absent/stored + C1 => the original CAS writer;
        // O2 proposal => one stateless writer with or without bindings;
        // O3 other ownership => no adapter-local writer. Existing read/delete
        // bindings remain, and a client declaration cannot become Host execution.
        use awaken_runtime_contract::resolved::ProviderServerTool;
        for bound in [false, true] {
            for case in [
                "absent", "stored", "proposal", "client", "foreign", "detached", "provider",
            ] {
                let proposal = awaken_ext_memory::write_memory_descriptor();
                let descriptor = match case {
                    "absent" => None,
                    "stored" => Some(ToolDescriptor::for_tool::<WriteMemoryTool>(
                        "session-memory",
                    )),
                    "proposal" => Some(proposal.clone()),
                    "client" => Some(ToolDescriptor::client_executed(
                        proposal.id.clone(),
                        "client",
                        proposal.model_parameters(),
                    )),
                    "foreign" => Some(ToolDescriptor::pinned(
                        "foreign",
                        proposal.id.clone(),
                        "foreign",
                        proposal.model_parameters(),
                    )),
                    "detached" => Some(proposal.clone().with_kind(ToolKind::DetachedOnly)),
                    "provider" => Some(
                        proposal
                            .clone()
                            .with_provider_server_tool(ProviderServerTool::openai_web_search()),
                    ),
                    _ => unreachable!(),
                };
                let mut snapshot = crate::config::server_config(
                    "writer",
                    "test",
                    &Default::default(),
                    &Default::default(),
                    &[],
                    &Default::default(),
                    &[],
                    awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
                )
                .unwrap();
                snapshot.resolved_spec.tool_descriptors = descriptor.into_iter().collect();
                snapshot.recompute_fingerprint().unwrap();
                let binding = Arc::new(FakeBinding {
                    label: "store",
                    calls: Mutex::new(Vec::new()),
                });
                let bindings = if bound {
                    HashMap::from([(
                        "memory".into(),
                        binding.clone() as Arc<dyn SessionMemoryBinding>,
                    )])
                } else {
                    HashMap::new()
                };
                let wiring = SessionMemoryTools::from_bindings(bindings, Some(&snapshot));
                let writers = wiring
                    .as_ref()
                    .map(|wiring| {
                        wiring
                            .executors
                            .iter()
                            .filter(|tool| tool.id() == "write_memory")
                            .count()
                    })
                    .unwrap_or(0);
                let expected =
                    usize::from(case == "proposal" || bound && matches!(case, "absent" | "stored"));
                assert_eq!(writers, expected, "{bound}/{case}");
                if case == "proposal" {
                    let wiring = wiring.unwrap();
                    let write = wiring
                        .executors
                        .iter()
                        .find(|tool| tool.id() == "write_memory")
                        .unwrap();
                    let result = write
                        .invoke(ToolCall {
                            call_id: "proposal".into(),
                            tool_id: "write_memory".into(),
                            arguments: serde_json::json!({"name":"pref","content":"prefers tea"}),
                        })
                        .await
                        .unwrap();
                    assert!(!result.is_error);
                    assert!(result.text().contains("saved memory /pref.md"));
                    assert!(
                        binding.calls.lock().unwrap().is_empty(),
                        "proposal never reaches the bound store"
                    );
                    let ids = wiring
                        .descriptors
                        .iter()
                        .filter(|tool| tool.id == "write_memory")
                        .collect::<Vec<_>>();
                    assert_eq!(ids.len(), 1);
                    assert_eq!(ids[0].content_namespace(), proposal.content_namespace());
                }
            }
        }
    }
}
