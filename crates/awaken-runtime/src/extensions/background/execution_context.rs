use std::sync::Arc;

use super::send_message_tool::DurableMessageSink;
use super::{BackgroundTaskManager, TaskId};

tokio::task_local! {
    static CURRENT_BACKGROUND_TASK_CONTEXT: BackgroundTaskExecutionContext;
}

tokio::task_local! {
    static CURRENT_TOOL_LINEAGE_CONTEXT: ToolLineageContext;
}

tokio::task_local! {
    /// Ambient durable message sink for the current run, scoped by the host
    /// around run execution (e.g. the server). Read by the per-run background
    /// plugin auto-creation in the local backend so `send_message`'s durable
    /// routes deliver. Reuses the same task-local mechanism as the contexts
    /// above; no new injection interface.
    static CURRENT_DURABLE_MESSAGE_SINK: Arc<dyn DurableMessageSink>;
}

pub(crate) async fn scope_durable_message_sink<Fut>(
    sink: Arc<dyn DurableMessageSink>,
    future: Fut,
) -> Fut::Output
where
    Fut: std::future::Future,
{
    CURRENT_DURABLE_MESSAGE_SINK.scope(sink, future).await
}

pub(crate) fn current_durable_message_sink() -> Option<Arc<dyn DurableMessageSink>> {
    CURRENT_DURABLE_MESSAGE_SINK.try_with(Clone::clone).ok()
}

#[derive(Clone)]
pub(crate) struct BackgroundTaskExecutionContext {
    pub(crate) manager: Arc<BackgroundTaskManager>,
    pub(crate) task_id: TaskId,
    pub(crate) run_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ToolLineageContext {
    pub(crate) run_id: String,
    pub(crate) call_id: String,
    pub(crate) agent_id: String,
}

pub(crate) async fn scope_background_task_context<Fut>(
    context: BackgroundTaskExecutionContext,
    future: Fut,
) -> Fut::Output
where
    Fut: std::future::Future,
{
    CURRENT_BACKGROUND_TASK_CONTEXT.scope(context, future).await
}

pub(crate) fn current_background_task_context() -> Option<BackgroundTaskExecutionContext> {
    CURRENT_BACKGROUND_TASK_CONTEXT.try_with(Clone::clone).ok()
}

pub fn current_background_task_id() -> Option<TaskId> {
    CURRENT_BACKGROUND_TASK_CONTEXT
        .try_with(|context| context.task_id.clone())
        .ok()
}

pub(crate) async fn scope_tool_lineage_context<Fut>(
    context: ToolLineageContext,
    future: Fut,
) -> Fut::Output
where
    Fut: std::future::Future,
{
    CURRENT_TOOL_LINEAGE_CONTEXT.scope(context, future).await
}

pub(crate) fn current_tool_lineage_context() -> Option<ToolLineageContext> {
    CURRENT_TOOL_LINEAGE_CONTEXT.try_with(Clone::clone).ok()
}
