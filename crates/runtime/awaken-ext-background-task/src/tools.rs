use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::state::{Scope, Store};
use awaken_runtime_contract::plugin::DynamicTool;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::{
    RawTool, ToolCall, ToolError, ToolOutput, ToolRecoveryCapability, current_tool_execution_facts,
    current_tool_operation_context, current_tool_state, parse_tool_args,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BackgroundTaskSupervisor;
use crate::{
    BACKGROUND_TASK_STATE_PREFIX, BackgroundInvocation, BackgroundTask, BackgroundTaskId,
    BackgroundTaskLifecycle, BackgroundTaskOrigin, task_state_cell,
};

pub struct RunInBackground {
    allowed: Arc<BTreeSet<String>>,
    supervisor: Arc<BackgroundTaskSupervisor>,
}
pub struct ListBackgroundTasks;
pub struct GetBackgroundTask;
pub struct CancelBackgroundTask;

impl RunInBackground {
    pub const ID: &str = "run_in_background";
}
impl ListBackgroundTasks {
    pub const ID: &str = "list_background_tasks";
}
impl GetBackgroundTask {
    pub const ID: &str = "get_background_task";
}
impl CancelBackgroundTask {
    pub const ID: &str = "cancel_background_task";
}

#[must_use]
pub fn is_management_tool(id: &str) -> bool {
    [
        RunInBackground::ID,
        ListBackgroundTasks::ID,
        GetBackgroundTask::ID,
        CancelBackgroundTask::ID,
    ]
    .contains(&id)
}

pub fn tools(
    allowed: BTreeSet<String>,
    supervisor: Arc<BackgroundTaskSupervisor>,
) -> Vec<DynamicTool> {
    let allowed = Arc::new(allowed);
    vec![
        dynamic(
            run_descriptor(
                RunInBackground::ID,
                "Run one configured ordinary tool as a durable background task.",
                &allowed,
            ),
            Arc::new(RunInBackground {
                allowed,
                supervisor,
            }),
        ),
        dynamic(
            typed_descriptor::<EmptyArgs>(
                ListBackgroundTasks::ID,
                "List background tasks in this Thread.",
            ),
            Arc::new(ListBackgroundTasks),
        ),
        dynamic(
            typed_descriptor::<TaskIdArgs>(
                GetBackgroundTask::ID,
                "Get one background task in this Thread.",
            ),
            Arc::new(GetBackgroundTask),
        ),
        dynamic(
            typed_descriptor::<TaskIdArgs>(
                CancelBackgroundTask::ID,
                "Request cancellation of one background task.",
            ),
            Arc::new(CancelBackgroundTask),
        ),
    ]
}

fn typed_descriptor<A: JsonSchema>(id: &str, description: &str) -> ToolDescriptor {
    ToolDescriptor::for_args::<A>("background-task", id, description)
}

fn run_descriptor(id: &str, description: &str, allowed: &BTreeSet<String>) -> ToolDescriptor {
    let generated = typed_descriptor::<RunArgs>(id, description);
    let mut parameters = generated.model_parameters();
    parameters["properties"]["tool"]["enum"] = serde_json::Value::Array(
        allowed
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect(),
    );
    ToolDescriptor::pinned("background-task", id, description, parameters)
}

fn dynamic(descriptor: ToolDescriptor, tool: Arc<dyn RawTool>) -> DynamicTool {
    DynamicTool::try_new(descriptor, tool)
        .expect("descriptor and executable share one id authority")
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    tool: String,
    /// Arguments accepted by the selected ordinary tool.
    arguments: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskIdArgs {
    #[schemars(with = "String", length(min = 1))]
    task_id: BackgroundTaskId,
}

#[derive(Serialize)]
struct SubmittedTask<'a> {
    task_id: &'a BackgroundTaskId,
}

#[derive(Serialize)]
struct TaskView<'a> {
    id: &'a BackgroundTaskId,
    tool_id: &'a str,
    lifecycle: &'a BackgroundTaskLifecycle,
    revision: u64,
}

impl<'a> From<&'a BackgroundTask> for TaskView<'a> {
    fn from(task: &'a BackgroundTask) -> Self {
        Self {
            id: &task.id,
            tool_id: &task.invocation.call.tool_id,
            lifecycle: &task.lifecycle,
            revision: task.revision,
        }
    }
}

fn state() -> Result<Arc<Store>, ToolError> {
    current_tool_state().ok_or_else(|| {
        ToolError::Execution("background task tools require Runtime State context".into())
    })
}

pub(crate) fn load_tasks(store: &Store) -> Result<Vec<BackgroundTask>, ToolError> {
    store
        .scan_prefix(Scope::Thread, BACKGROUND_TASK_STATE_PREFIX)
        .map(|(key, value)| {
            BackgroundTask::deserialize(value).map_err(|error| {
                ToolError::Execution(format!(
                    "background task state at {:?} is malformed: {error}",
                    key.0
                ))
            })
        })
        .collect()
}

fn render<T: Serialize>(call_id: &str, value: &T) -> Result<ToolOutput, ToolError> {
    serde_json::to_string(value)
        .map(|value| ToolOutput::ok(call_id, value))
        .map_err(|error| ToolError::Execution(error.to_string()))
}

#[async_trait]
impl RawTool for RunInBackground {
    fn id(&self) -> &str {
        Self::ID
    }
    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::DurableRequest
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let ToolCall {
            call_id, arguments, ..
        } = call;
        let RunArgs { tool, arguments } = parse_tool_args(arguments)?;
        if !self.allowed.contains(&tool) || is_management_tool(&tool) {
            return Err(ToolError::InvalidArguments(format!(
                "tool `{tool}` is not configured for background execution"
            )));
        }
        let context = current_tool_operation_context().ok_or_else(|| {
            ToolError::Execution("run_in_background requires Runtime operation context".into())
        })?;
        let run_id = context
            .run_id
            .ok_or_else(|| ToolError::Execution("background task origin has no Run".into()))?;
        let thread_id = context
            .thread_id
            .ok_or_else(|| ToolError::Execution("background task origin has no Thread".into()))?;
        if context.operation_id.trim().is_empty() {
            return Err(ToolError::Execution(
                "background task operation id is empty".into(),
            ));
        }
        let fingerprint = awaken_runtime_contract::content_fingerprint(&(
            "background-task-v1",
            &thread_id.0,
            &run_id.0,
            &context.operation_id,
        ))
        .map_err(|error| ToolError::Execution(error.to_string()))?;
        let id = BackgroundTaskId::new(format!("task-{fingerprint}"))
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut task = BackgroundTask::requested(
            id,
            BackgroundTaskOrigin {
                thread_id,
                run_id,
                operation_id: context.operation_id,
            },
            BackgroundInvocation {
                call: ToolCall {
                    call_id,
                    tool_id: tool,
                    arguments: serde_json::Value::Object(arguments.into_iter().collect()),
                },
            },
        );
        let execution = current_tool_execution_facts(&task.invocation.call)?;
        task.start(
            self.supervisor.worker_id(),
            BackgroundTaskSupervisor::now_ms(),
            BackgroundTaskSupervisor::lease_ms(),
            crate::TaskExecutionPolicy {
                recovery: execution.recovery,
                concurrency: execution.concurrency,
            },
        )
        .map_err(|error| ToolError::Execution(error.to_string()))?;
        let store = state()?;
        let cell = task_state_cell(&task.id);
        let mut output = render(
            &task.invocation.call.call_id,
            &SubmittedTask { task_id: &task.id },
        )?;
        match cell
            .load(&store)
            .map_err(|error| ToolError::Execution(error.to_string()))?
        {
            Some(existing) if existing.same_request_as(&task) => Ok(output),
            Some(_) => Err(ToolError::Execution(
                "deterministic background task id collided with different state".into(),
            )),
            None => {
                output.state.push(
                    cell.write(&task)
                        .map_err(|error| ToolError::Execution(error.to_string()))?,
                );
                Ok(output)
            }
        }
    }
}

#[async_trait]
impl RawTool for ListBackgroundTasks {
    fn id(&self) -> &str {
        Self::ID
    }
    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::ReplaySafe
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let _: EmptyArgs = parse_tool_args(call.arguments)?;
        let state = state()?;
        let tasks = load_tasks(&state)?;
        render(
            &call.call_id,
            &tasks.iter().map(TaskView::from).collect::<Vec<_>>(),
        )
    }
}

#[async_trait]
impl RawTool for GetBackgroundTask {
    fn id(&self) -> &str {
        Self::ID
    }
    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::ReplaySafe
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let TaskIdArgs { task_id } = parse_tool_args(call.arguments)?;
        let state = state()?;
        let task = task_state_cell(&task_id)
            .load(&state)
            .map_err(|error| ToolError::Execution(error.to_string()))?
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "background task {:?} was not found",
                    task_id.as_str()
                ))
            })?;
        render(&call.call_id, &TaskView::from(&task))
    }
}

#[async_trait]
impl RawTool for CancelBackgroundTask {
    fn id(&self) -> &str {
        Self::ID
    }
    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::ReplaySafe
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let TaskIdArgs { task_id } = parse_tool_args(call.arguments)?;
        let store = state()?;
        let cell = task_state_cell(&task_id);
        let mut task = cell
            .load(&store)
            .map_err(|error| ToolError::Execution(error.to_string()))?
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "background task {:?} was not found",
                    task_id.as_str()
                ))
            })?;
        let revision = task.revision;
        task.request_cancel()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut output = render(&call.call_id, &TaskView::from(&task))?;
        if task.revision != revision {
            output.state.push(
                cell.write(&task)
                    .map_err(|error| ToolError::Execution(error.to_string()))?,
            );
        }
        Ok(output)
    }
}
