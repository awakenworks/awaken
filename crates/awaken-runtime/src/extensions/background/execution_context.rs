use std::sync::Arc;

use super::{BackgroundTaskManager, TaskId};

tokio::task_local! {
    static CURRENT_BACKGROUND_TASK_CONTEXT: BackgroundTaskExecutionContext;
}

tokio::task_local! {
    static CURRENT_TOOL_LINEAGE_CONTEXT: ToolLineageContext;
}

#[derive(Clone)]
pub(crate) struct BackgroundTaskExecutionContext {
    pub(crate) manager: Arc<BackgroundTaskManager>,
    pub(crate) task_id: TaskId,
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
