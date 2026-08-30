//! Adapt an MCP server tool into an awaken [`RawTool`].
//!
//! [`McpRawTool`] erases one MCP tool behind the runtime's `RawTool` port, keyed
//! by the namespaced id `mcp__<server>__<tool>`. It maps the MCP three-state
//! result onto the neutral one:
//!
//! - a transport failure is a [`ToolError`] (aborts the call);
//! - an MCP `isError` result is a model-visible [`ToolOutput::error`];
//! - a normal result is [`ToolOutput::ok`].

use std::sync::Arc;

use async_trait::async_trait;
use awaken_mcp_wire::{McpTask, McpToolDefinition, TaskStatus, TaskSupport, ToolContent};
use awaken_runtime_contract::ContentBlock;
use awaken_runtime_contract::resolved::{ToolDescriptor, ToolKind};
use awaken_runtime_contract::tool::{
    RawTool, ToolCall, ToolError, ToolOutput, ToolTaskHandle, ToolTaskPoll, ToolTaskStart,
};
use serde_json::Value;

use crate::error::McpError;
use crate::id_mapping::to_tool_id;
use crate::transport::McpToolTransport;

/// Stable adapter owner stored in protocol-neutral durable task handles.
pub const MCP_TASK_OWNER: &str = "mcp";

/// One MCP server tool presented as a runtime [`RawTool`].
pub struct McpRawTool {
    /// Namespaced runtime id (`mcp__<server>__<tool>`); the descriptor shares it.
    id: String,
    /// The tool's name on the wire (unsanitized), passed to `tools/call`.
    tool_name: String,
    /// Tool-local half of task capability negotiation. The connection half is
    /// retained by `transport`; both must agree before augmentation is used.
    task_support: TaskSupport,
    transport: Arc<dyn McpToolTransport>,
}

impl McpRawTool {
    /// Build a raw tool for `tool_name` on `server_name`, invoked through
    /// `transport`. Fails if either name sanitizes to an empty id component.
    pub fn new(
        server_name: &str,
        tool_name: &str,
        transport: Arc<dyn McpToolTransport>,
    ) -> Result<Self, McpError> {
        let id = to_tool_id(server_name, tool_name)?;
        Ok(Self {
            id,
            tool_name: tool_name.to_string(),
            task_support: TaskSupport::Forbidden,
            transport,
        })
    }

    /// Build from the complete discovered definition, preserving the tool's
    /// task-support declaration. Required support fails closed when the server
    /// did not negotiate task-augmented `tools/call` during initialize.
    pub fn from_definition(
        server_name: &str,
        definition: &McpToolDefinition,
        transport: Arc<dyn McpToolTransport>,
    ) -> Result<Self, McpError> {
        let task_support = task_support(definition);
        validate_task_negotiation(server_name, task_support, transport.as_ref())?;
        let id = to_tool_id(server_name, &definition.name)?;
        Ok(Self {
            id,
            tool_name: definition.name.clone(),
            task_support,
            transport,
        })
    }

    fn task_capable(&self) -> bool {
        self.task_support != TaskSupport::Forbidden && self.transport.supports_task_tools_call()
    }

    fn task_handle(&self, task: McpTask) -> Result<ToolTaskHandle, ToolError> {
        validate_remote_task(&task, None)?;
        let handle = ToolTaskHandle {
            owner: MCP_TASK_OWNER.to_string(),
            binding: self.id.clone(),
            task_id: task.task_id,
            poll_interval_ms: task.poll_interval,
        };
        handle.validate()?;
        Ok(handle)
    }

    fn validate_handle(&self, task: &ToolTaskHandle) -> Result<(), ToolError> {
        task.validate()?;
        if task.owner != MCP_TASK_OWNER || task.binding != self.id {
            return Err(ToolError::Execution(
                "MCP task handle does not belong to this resolved tool".into(),
            ));
        }
        Ok(())
    }

    async fn invoke_foreground(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let arguments = normalized_arguments(call.arguments);
        let result = self
            .transport
            .call_tool(&self.tool_name, arguments)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(result_to_output(&call.call_id, result))
    }

    async fn map_task_status(
        &self,
        call: &ToolCall,
        expected_task_id: &str,
        task: McpTask,
    ) -> Result<ToolTaskPoll, ToolError> {
        validate_remote_task(&task, Some(expected_task_id))?;
        match task.status {
            TaskStatus::Working => Ok(ToolTaskPoll::Pending {
                poll_interval_ms: task.poll_interval,
            }),
            TaskStatus::InputRequired => Ok(ToolTaskPoll::InputRequired {
                poll_interval_ms: task.poll_interval,
                message: task.status_message,
            }),
            TaskStatus::Completed => {
                let result = self
                    .transport
                    .get_task_result(expected_task_id)
                    .await
                    .map_err(|error| ToolError::Execution(error.to_string()))?;
                Ok(ToolTaskPoll::Completed(result_to_output(
                    &call.call_id,
                    result,
                )))
            }
            TaskStatus::Failed => Ok(ToolTaskPoll::Failed {
                message: task
                    .status_message
                    .unwrap_or_else(|| "MCP task failed".to_string()),
            }),
            TaskStatus::Cancelled => Ok(ToolTaskPoll::Cancelled),
        }
    }
}

#[async_trait]
impl RawTool for McpRawTool {
    fn id(&self) -> &str {
        &self.id
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        if self.task_capable() {
            return Err(ToolError::Execution(
                "MCP task-capable tools require detached execution".into(),
            ));
        }
        self.invoke_foreground(call).await
    }

    async fn start_task(&self, call: ToolCall) -> Result<ToolTaskStart, ToolError> {
        if !self.task_capable() {
            return self
                .invoke_foreground(call)
                .await
                .map(ToolTaskStart::Completed);
        }
        let arguments = normalized_arguments(call.arguments);
        let created = self
            .transport
            .call_tool_as_task(&self.tool_name, arguments, None)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        self.task_handle(created.task).map(ToolTaskStart::Pending)
    }

    async fn poll_task(
        &self,
        call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> Result<ToolTaskPoll, ToolError> {
        self.validate_handle(task)?;
        let status = self
            .transport
            .get_task(&task.task_id)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        self.map_task_status(call, &task.task_id, status).await
    }

    async fn cancel_task(
        &self,
        call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> Result<ToolTaskPoll, ToolError> {
        self.validate_handle(task)?;
        if !self.transport.supports_task_cancel() {
            return Err(ToolError::Execution(
                "MCP server did not negotiate tasks/cancel".into(),
            ));
        }
        let status = self
            .transport
            .cancel_task(&task.task_id)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        self.map_task_status(call, &task.task_id, status).await
    }
}

fn normalized_arguments(arguments: Value) -> Value {
    if arguments.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        arguments
    }
}

fn result_to_output(call_id: &str, result: awaken_mcp_wire::CallToolResult) -> ToolOutput {
    let content = content_blocks(&result.content);
    if result.is_error.unwrap_or(false) {
        ToolOutput::error_blocks(call_id, content)
    } else {
        ToolOutput::ok_blocks(call_id, content)
    }
}

fn validate_remote_task(task: &McpTask, expected_id: Option<&str>) -> Result<(), ToolError> {
    if task.task_id.trim().is_empty()
        || task.poll_interval == Some(0)
        || expected_id.is_some_and(|expected| expected != task.task_id.as_str())
    {
        return Err(ToolError::Execution(
            "MCP server returned invalid or mismatched task coordinates".into(),
        ));
    }
    Ok(())
}

fn task_support(definition: &McpToolDefinition) -> TaskSupport {
    definition
        .execution
        .as_ref()
        .and_then(|execution| execution.task_support)
        .unwrap_or_default()
}

fn validate_task_negotiation(
    server_name: &str,
    support: TaskSupport,
    transport: &dyn McpToolTransport,
) -> Result<(), McpError> {
    if support == TaskSupport::Required && !transport.supports_task_tools_call() {
        Err(McpError::UnsupportedCapability {
            server_name: server_name.to_string(),
            capability: "task-augmented tools/call required by an advertised tool",
        })
    } else {
        Ok(())
    }
}

pub(crate) fn validate_tools_task_negotiation(
    server_name: &str,
    definitions: &[McpToolDefinition],
    transport: &dyn McpToolTransport,
) -> Result<(), McpError> {
    for definition in definitions {
        validate_task_negotiation(server_name, task_support(definition), transport)?;
    }
    Ok(())
}

/// Preserve MCP text and image payloads in the runtime's canonical multimodal
/// blocks. Media without a neutral consumer remains an explicit text marker.
pub(crate) fn content_blocks(content: &[ToolContent]) -> Vec<ContentBlock> {
    content
        .iter()
        .map(|block| match block {
            ToolContent::Text { text, .. } => ContentBlock::text(text),
            ToolContent::Image {
                data, mime_type, ..
            } => ContentBlock::image_base64(mime_type, data),
            other => ContentBlock::text(
                serde_json::to_string(other)
                    .unwrap_or_else(|_| "[unrenderable mcp content]".to_string()),
            ),
        })
        .collect()
}

/// Build the compatibility descriptor for an MCP tool without task capability
/// context: namespaced id, the server-supplied description, and its input
/// schema. New discovery paths use [`mcp_tool_descriptor_for_transport`] so a
/// negotiated task-capable tool becomes `DetachedOnly`. The canonical
/// descriptor authority normalizes compatible omissions and rejects
/// contradictory shapes before its content hash is pinned (G3/G8).
pub fn mcp_tool_descriptor(
    server_name: &str,
    def: &McpToolDefinition,
) -> Result<ToolDescriptor, McpError> {
    let id = to_tool_id(server_name, &def.name)?;
    let description = def.description.clone().unwrap_or_default();
    ToolDescriptor::try_pinned("mcp", id, description, def.input_schema.clone())
        .map_err(|error| McpError::InvalidToolSchema(error.to_string()))
}

/// Build the descriptor from both halves of MCP task negotiation. Task-capable
/// tools are execution-only behind a detached wrapper and intentionally keep
/// `NeverReplay`: MCP does not give the initial `tools/call` request a
/// client-stable id, so only a committed remote task id may be recovered.
pub(crate) fn mcp_tool_descriptor_for_transport(
    server_name: &str,
    def: &McpToolDefinition,
    transport: &dyn McpToolTransport,
) -> Result<ToolDescriptor, McpError> {
    let support = task_support(def);
    validate_task_negotiation(server_name, support, transport)?;
    let descriptor = mcp_tool_descriptor(server_name, def)?;
    if support != TaskSupport::Forbidden && transport.supports_task_tools_call() {
        Ok(descriptor.with_kind(ToolKind::DetachedOnly))
    } else {
        Ok(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_mcp_wire::McpTransportError;
    use awaken_mcp_wire::{CallToolResult, CreateTaskResult};
    use awaken_runtime_contract::tool::{ToolRecoveryCapability, ToolRecoveryMode};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A transport whose single `call_tool` outcome is fixed, so each of the
    /// three result states can be driven deterministically.
    struct FakeTransport {
        tools: Vec<McpToolDefinition>,
        outcome: Outcome,
    }

    enum Outcome {
        /// A successful call returning `text`, with the given `is_error` flag.
        Result { text: String, is_error: bool },
        /// The transport itself failed.
        TransportErr(String),
    }

    #[async_trait]
    impl McpToolTransport for FakeTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            match &self.outcome {
                Outcome::Result { text, is_error } => Ok(CallToolResult {
                    content: vec![ToolContent::Text {
                        text: text.clone(),
                        annotations: None,
                        meta: None,
                    }],
                    structured_content: None,
                    is_error: Some(*is_error),
                }),
                Outcome::TransportErr(msg) => Err(McpTransportError::TransportError(msg.clone())),
            }
        }
    }

    fn tool_def(name: &str) -> McpToolDefinition {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "description": format!("the {name} tool"),
            "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } },
        }))
        .expect("valid tool definition")
    }

    fn task_tool_def(name: &str, support: TaskSupport) -> McpToolDefinition {
        let mut definition = tool_def(name);
        definition.execution = Some(awaken_mcp_wire::ToolExecution {
            task_support: Some(support),
        });
        definition
    }

    fn transport(outcome: Outcome) -> Arc<dyn McpToolTransport> {
        Arc::new(FakeTransport {
            tools: vec![tool_def("echo")],
            outcome,
        })
    }

    fn call() -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            tool_id: "mcp__srv__echo".to_string(),
            arguments: serde_json::json!({ "q": "hi" }),
        }
    }

    struct TaskTransport {
        negotiated: bool,
        cancel: bool,
        polls: AtomicUsize,
    }

    impl TaskTransport {
        fn task(&self, status: TaskStatus) -> McpTask {
            McpTask {
                task_id: "remote-1".to_string(),
                status,
                status_message: None,
                created_at: "2026-08-30T00:00:00Z".to_string(),
                last_updated_at: "2026-08-30T00:00:01Z".to_string(),
                ttl: Some(60_000),
                poll_interval: Some(25),
            }
        }
    }

    #[async_trait]
    impl McpToolTransport for TaskTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(vec![task_tool_def("echo", TaskSupport::Optional)])
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            panic!("task-capable definition must not use foreground tools/call")
        }

        fn supports_task_tools_call(&self) -> bool {
            self.negotiated
        }

        fn supports_task_cancel(&self) -> bool {
            self.cancel
        }

        async fn call_tool_as_task(
            &self,
            tool_name: &str,
            arguments: Value,
            ttl_ms: Option<u64>,
        ) -> Result<CreateTaskResult, McpTransportError> {
            assert_eq!(tool_name, "echo");
            assert_eq!(arguments, serde_json::json!({"q": "hi"}));
            assert_eq!(ttl_ms, None);
            Ok(CreateTaskResult {
                task: self.task(TaskStatus::Working),
            })
        }

        async fn get_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
            assert_eq!(task_id, "remote-1");
            let status = if self.polls.fetch_add(1, Ordering::SeqCst) == 0 {
                TaskStatus::Working
            } else {
                TaskStatus::Completed
            };
            Ok(self.task(status))
        }

        async fn get_task_result(
            &self,
            task_id: &str,
        ) -> Result<CallToolResult, McpTransportError> {
            assert_eq!(task_id, "remote-1");
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: "done".to_string(),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }

        async fn cancel_task(&self, task_id: &str) -> Result<McpTask, McpTransportError> {
            assert_eq!(task_id, "remote-1");
            Ok(self.task(TaskStatus::Cancelled))
        }
    }

    #[tokio::test]
    async fn success_result_maps_to_ok_output() {
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::Result {
                text: "pong".to_string(),
                is_error: false,
            }),
        )
        .expect("builds");
        assert_eq!(tool.id(), "mcp__srv__echo");
        assert_eq!(
            tool.execution_target(),
            awaken_runtime_contract::tool::ToolExecutionTarget::Brain
        );
        assert_eq!(
            tool.concurrency(&serde_json::json!({"q": "hi"})),
            awaken_runtime_contract::tool::ToolConcurrency::Parallel,
            "MCP tools use maximum parallelism unless host configuration narrows it"
        );
        let out = tool.invoke(call()).await.expect("invokes");
        assert!(!out.is_error);
        assert_eq!(out.text(), "pong");
        assert_eq!(out.call_id, "c1");
    }

    #[tokio::test]
    async fn tool_error_result_maps_to_model_visible_error() {
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::Result {
                text: "boom".to_string(),
                is_error: true,
            }),
        )
        .expect("builds");
        // A tool-level error does not abort the run: it is a model-visible
        // error result, not a `ToolError`.
        let out = tool.invoke(call()).await.expect("invokes");
        assert!(out.is_error);
        assert_eq!(out.text(), "boom");
    }

    #[tokio::test]
    async fn transport_failure_maps_to_tool_error() {
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::TransportErr("connection reset".to_string())),
        )
        .expect("builds");
        let err = tool.invoke(call()).await.expect_err("transport fails");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("connection reset")));
    }

    #[tokio::test]
    async fn null_arguments_become_an_empty_object() {
        // A no-argument call must still reach the transport (as `{}`), not be
        // rejected. The fake ignores args, so a successful result proves the
        // null path did not panic or short-circuit.
        let tool = McpRawTool::new(
            "srv",
            "echo",
            transport(Outcome::Result {
                text: "ok".to_string(),
                is_error: false,
            }),
        )
        .expect("builds");
        let mut c = call();
        c.arguments = Value::Null;
        let out = tool.invoke(c).await.expect("invokes");
        assert_eq!(out.text(), "ok");
    }

    #[test]
    fn task_capability_matrix_controls_projection_without_inventing_replay_safety() {
        // Cause/effect graph: C1=tool taskSupport forbidden/optional/required;
        // C2=server negotiated task tools/call. Effects: E1=ordinary Regular
        // projection; E2=DetachedOnly projection; E3=required mismatch fails
        // closed. Constraint: every rule keeps initial recovery NeverReplay
        // because MCP task creation has no stable client request id. Rules:
        // M1=forbidden,*=>E1; M2=optional,false=>E1;
        // M3=optional|required,true=>E2; M4=required,false=>E3.
        let no_tasks = TaskTransport {
            negotiated: false,
            cancel: false,
            polls: AtomicUsize::new(0),
        };
        let tasks = TaskTransport {
            negotiated: true,
            cancel: true,
            polls: AtomicUsize::new(0),
        };

        let forbidden = mcp_tool_descriptor_for_transport(
            "srv",
            &task_tool_def("echo", TaskSupport::Forbidden),
            &tasks,
        )
        .expect("M1");
        assert_eq!(forbidden.kind, ToolKind::Regular, "M1/E1");
        let optional_without = mcp_tool_descriptor_for_transport(
            "srv",
            &task_tool_def("echo", TaskSupport::Optional),
            &no_tasks,
        )
        .expect("M2");
        assert_eq!(optional_without.kind, ToolKind::Regular, "M2/E1");
        for support in [TaskSupport::Optional, TaskSupport::Required] {
            let descriptor =
                mcp_tool_descriptor_for_transport("srv", &task_tool_def("echo", support), &tasks)
                    .expect("M3");
            assert_eq!(descriptor.kind, ToolKind::DetachedOnly, "M3/E2");
            assert_eq!(
                descriptor.recovery_policy.mode(),
                ToolRecoveryMode::NeverReplay,
                "M3 must not fabricate initial replay safety"
            );
        }
        assert!(
            matches!(
                mcp_tool_descriptor_for_transport(
                    "srv",
                    &task_tool_def("echo", TaskSupport::Required),
                    &no_tasks,
                ),
                Err(McpError::UnsupportedCapability { .. })
            ),
            "M4/E3"
        );
    }

    #[tokio::test]
    async fn task_raw_tool_starts_polls_completes_and_cancels_by_opaque_handle() {
        // Cause/effect graph: C1=both task capability halves agree; C2=start
        // returns a valid remote id; C3=status progresses working->completed;
        // C4=cancel is negotiated; C5=handle owner/binding mismatch. Effects:
        // E1=foreground invocation is rejected; E2=opaque handle is returned;
        // E3=working stays pending then tasks/result becomes ToolOutput;
        // E4=cancelled is terminal; E5=foreign handle fails before I/O. Rules:
        // L1=C1+C2=>E1+E2; L2=C3=>E3; L3=C4=>E4; L4=C5=>E5.
        let transport = Arc::new(TaskTransport {
            negotiated: true,
            cancel: true,
            polls: AtomicUsize::new(0),
        });
        let tool = McpRawTool::from_definition(
            "srv",
            &task_tool_def("echo", TaskSupport::Optional),
            transport,
        )
        .expect("L1 negotiated");
        assert_eq!(
            tool.recovery_capability(),
            ToolRecoveryCapability::NonRecoverable,
            "initial task creation stays non-replayable"
        );
        assert!(
            matches!(
                tool.invoke(call()).await,
                Err(ToolError::Execution(message)) if message.contains("detached execution")
            ),
            "L1/E1"
        );

        let ToolTaskStart::Pending(handle) = tool.start_task(call()).await.expect("L1/E2") else {
            panic!("expected remote task handle");
        };
        assert_eq!(handle.owner, "mcp", "L1/E2");
        assert_eq!(handle.binding, "mcp__srv__echo", "L1/E2");
        assert_eq!(handle.task_id, "remote-1", "L1/E2");
        assert!(
            matches!(
                tool.poll_task(&call(), &handle).await.unwrap(),
                ToolTaskPoll::Pending {
                    poll_interval_ms: Some(25)
                }
            ),
            "L2/E3"
        );
        let ToolTaskPoll::Completed(output) =
            tool.poll_task(&call(), &handle).await.expect("L2/E3")
        else {
            panic!("expected completed result");
        };
        assert_eq!(output.text(), "done", "L2/E3");
        assert_eq!(
            tool.cancel_task(&call(), &handle).await.unwrap(),
            ToolTaskPoll::Cancelled,
            "L3/E4"
        );

        let mut foreign = handle;
        foreign.binding = "mcp__other__echo".to_string();
        assert!(
            matches!(
                tool.poll_task(&call(), &foreign).await,
                Err(ToolError::Execution(message)) if message.contains("does not belong")
            ),
            "L4/E5"
        );
    }

    #[tokio::test]
    async fn task_status_and_cancel_capability_fail_closed_decision_table() {
        // Cause/effect graph: C1=status input_required; C2=status failed with a
        // diagnostic; C3=cancel was not negotiated; C4=server returns another
        // task id or zero poll interval. Effects E1=InputRequired keeps its
        // status metadata; E2=Failed keeps the diagnostic; E3=cancel errors
        // before transport I/O; E4=malformed task evidence is rejected. Rules
        // S1=C1=>E1; S2=C2=>E2; S3=C3=>E3; S4=C4=>E4.
        let transport = Arc::new(TaskTransport {
            negotiated: true,
            cancel: false,
            polls: AtomicUsize::new(0),
        });
        let tool = McpRawTool::from_definition(
            "srv",
            &task_tool_def("echo", TaskSupport::Required),
            transport,
        )
        .unwrap();
        let handle = match tool.start_task(call()).await.unwrap() {
            ToolTaskStart::Pending(handle) => handle,
            ToolTaskStart::Completed(_) => panic!("task unexpectedly completed inline"),
        };
        let mut input_required = TaskTransport {
            negotiated: true,
            cancel: false,
            polls: AtomicUsize::new(0),
        }
        .task(TaskStatus::InputRequired);
        input_required.status_message = Some("approval needed".into());
        assert_eq!(
            tool.map_task_status(&call(), "remote-1", input_required.clone())
                .await
                .unwrap(),
            ToolTaskPoll::InputRequired {
                poll_interval_ms: Some(25),
                message: Some("approval needed".into())
            },
            "S1/E1"
        );

        input_required.status = TaskStatus::Failed;
        input_required.status_message = Some("quota exhausted".into());
        assert_eq!(
            tool.map_task_status(&call(), "remote-1", input_required.clone())
                .await
                .unwrap(),
            ToolTaskPoll::Failed {
                message: "quota exhausted".into()
            },
            "S2/E2"
        );
        assert!(
            matches!(
                tool.cancel_task(&call(), &handle).await,
                Err(ToolError::Execution(message)) if message.contains("did not negotiate")
            ),
            "S3/E3"
        );

        input_required.status = TaskStatus::Working;
        input_required.task_id = "other".into();
        assert!(
            tool.map_task_status(&call(), "remote-1", input_required.clone())
                .await
                .is_err(),
            "S4/E4 mismatched id"
        );
        input_required.task_id = "remote-1".into();
        input_required.poll_interval = Some(0);
        assert!(
            tool.map_task_status(&call(), "remote-1", input_required)
                .await
                .is_err(),
            "S4/E4 zero interval"
        );
    }

    #[test]
    fn descriptor_is_namespaced_and_carries_the_schema() {
        let def = tool_def("echo");
        let descriptor = mcp_tool_descriptor("srv", &def).expect("builds");
        assert_eq!(descriptor.id, "mcp__srv__echo");
        assert_eq!(descriptor.description, "the echo tool");
        assert_eq!(descriptor.parameters["properties"]["q"]["type"], "string");
        // The id feeds the content hash, so it is present in the pinned hash.
        assert!(descriptor.content_hash().contains("mcp__srv__echo"));
    }

    #[test]
    fn descriptor_normalizes_a_missing_schema_to_an_object() {
        // Rule M1: omitted MCP inputSchema -> wire default -> the canonical
        // descriptor adds explicit empty properties for provider compatibility.
        let def: McpToolDefinition = serde_json::from_value(serde_json::json!({
            "name": "bare",
        }))
        .expect("valid tool definition");
        let descriptor = mcp_tool_descriptor("srv", &def).expect("builds");
        assert_eq!(descriptor.parameters["type"], "object");
        assert_eq!(descriptor.parameters["properties"], serde_json::json!({}));
    }

    #[test]
    fn descriptor_rejects_a_non_object_argument_schema() {
        // Rule M2: an MCP server declaring scalar tool arguments contradicts
        // the runtime invocation contract and fails before registration.
        let def =
            McpToolDefinition::new("broken").with_schema(serde_json::json!({"type":"string"}));
        assert!(matches!(
            mcp_tool_descriptor("srv", &def),
            Err(McpError::InvalidToolSchema(_))
        ));
    }

    #[test]
    fn multiple_text_blocks_keep_order_without_flattening() {
        // Cause/effect rule M1: two ordered MCP text blocks -> two ordered
        // neutral Text blocks. No synthesized delimiter or parallel string
        // representation may become authoritative.
        let content = vec![
            ToolContent::Text {
                text: "line one".to_string(),
                annotations: None,
                meta: None,
            },
            ToolContent::Text {
                text: "line two".to_string(),
                annotations: None,
                meta: None,
            },
        ];
        assert_eq!(
            content_blocks(&content),
            vec![
                ContentBlock::text("line one"),
                ContentBlock::text("line two")
            ]
        );
    }

    #[test]
    fn an_image_block_preserves_pixels_interleaved_with_text() {
        // Cause/effect rule M2: MCP text + base64 image -> ordered Text + Image
        // with exact MIME/data. Unsupported media alone may use a marker; an image
        // must never be demoted to JSON text.
        let content = vec![
            ToolContent::Text {
                text: "here is a chart".to_string(),
                annotations: None,
                meta: None,
            },
            ToolContent::Image {
                data: "AAAA".to_string(),
                mime_type: "image/png".to_string(),
                annotations: None,
                meta: None,
            },
        ];
        assert_eq!(
            content_blocks(&content),
            vec![
                ContentBlock::text("here is a chart"),
                ContentBlock::image_base64("image/png", "AAAA"),
            ]
        );
    }

    #[tokio::test]
    async fn an_image_result_reaches_the_runtime_as_pixels() {
        // Cause/effect rule M3: a successful MCP image-only result -> successful
        // ToolOutput with the exact Image block; error status and pixel bytes are
        // independent dimensions and neither is flattened.
        struct ImageTransport;
        #[async_trait]
        impl McpToolTransport for ImageTransport {
            async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
                Ok(vec![tool_def("shot")])
            }
            async fn call_tool(
                &self,
                _tool_name: &str,
                _arguments: Value,
            ) -> Result<CallToolResult, McpTransportError> {
                Ok(CallToolResult {
                    content: vec![ToolContent::Image {
                        data: "ZZZZ".to_string(),
                        mime_type: "image/jpeg".to_string(),
                        annotations: None,
                        meta: None,
                    }],
                    structured_content: None,
                    is_error: Some(false),
                })
            }
        }
        let tool = McpRawTool::new("srv", "shot", Arc::new(ImageTransport)).expect("builds");
        let out = tool.invoke(call()).await.expect("invokes");
        assert!(!out.is_error);
        assert_eq!(
            out.content,
            vec![ContentBlock::image_base64("image/jpeg", "ZZZZ")]
        );
    }
}
