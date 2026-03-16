use std::sync::Arc;

use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::storage::{MailboxStore, TaskStore};

mod plugin;
mod tools;
mod wiring;

#[cfg(test)]
mod tests;

pub(crate) const TEAM_PLUGIN_ID: &str = "team_collab";
pub(crate) const SEND_MESSAGE_TOOL_ID: &str = "send_message";
pub(crate) const TASK_LIST_TOOL_ID: &str = "task_list";
pub(crate) const TASK_UPDATE_TOOL_ID: &str = "task_update";

pub use plugin::TeamPlugin;
pub use tools::{SendMessageTool, TaskListTool, TaskUpdateTool};
pub use wiring::TeamWiring;

/// Shared handles for team-related stores.
#[derive(Clone)]
pub struct TeamStores {
    pub mailbox: Arc<dyn MailboxStore>,
    pub tasks: Arc<dyn TaskStore>,
}

pub(crate) fn team_tools(stores: &TeamStores) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SendMessageTool::new(stores.mailbox.clone())),
        Arc::new(TaskListTool::new(stores.tasks.clone())),
        Arc::new(TaskUpdateTool::new(stores.tasks.clone())),
    ]
}
