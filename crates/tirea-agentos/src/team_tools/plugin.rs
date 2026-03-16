use async_trait::async_trait;
use serde_json::Value;

use super::{
    TeamStores, SEND_MESSAGE_TOOL_ID, TASK_LIST_TOOL_ID, TASK_UPDATE_TOOL_ID, TEAM_PLUGIN_ID,
};
use crate::contracts::runtime::behavior::ReadOnlyContext;
use crate::contracts::runtime::phase::{ActionSet, AfterToolExecuteAction, BeforeInferenceAction};
use crate::contracts::runtime::AgentBehavior;
use crate::contracts::storage::{TaskQuery, TaskStatus};

/// Team collaboration behavior plugin.
///
/// - **before_inference**: renders pending tasks and team tool usage hints
///   into the system context so the LLM knows what's available.
/// - **after_tool_execute**: if the agent just used a task tool, render a
///   brief reminder of the current task board state.
pub struct TeamPlugin {
    stores: TeamStores,
    max_tasks: usize,
}

impl TeamPlugin {
    pub fn new(stores: TeamStores) -> Self {
        Self {
            stores,
            max_tasks: 20,
        }
    }
}

#[async_trait]
impl AgentBehavior for TeamPlugin {
    fn id(&self) -> &str {
        TEAM_PLUGIN_ID
    }

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let Some(team_id) = self.resolve_team_id(ctx).await else {
            return ActionSet::empty();
        };

        let rendered = self.render_task_board(&team_id).await;
        if rendered.is_empty() {
            ActionSet::empty()
        } else {
            ActionSet::single(BeforeInferenceAction::AddSystemContext(rendered))
        }
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        let tool_name = ctx.tool_name().unwrap_or_default();
        if tool_name != TASK_UPDATE_TOOL_ID
            && tool_name != TASK_LIST_TOOL_ID
            && tool_name != SEND_MESSAGE_TOOL_ID
        {
            return ActionSet::empty();
        }

        let Some(team_id) = self.resolve_team_id(ctx).await else {
            return ActionSet::empty();
        };

        let rendered = self.render_task_board_reminder(&team_id).await;
        match rendered {
            Some(s) => ActionSet::single(AfterToolExecuteAction::AddSystemReminder(s)),
            None => ActionSet::empty(),
        }
    }
}

impl TeamPlugin {
    async fn resolve_team_id(&self, ctx: &ReadOnlyContext<'_>) -> Option<String> {
        if let Some(team_id) = team_id_from_snapshot(&ctx.snapshot()) {
            return Some(team_id);
        }

        let tool_args = ctx.tool_args()?;
        if let Some(team_id) = tool_args
            .get("team_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|team_id| !team_id.is_empty())
        {
            return Some(team_id.to_string());
        }

        let task_id = tool_args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task_id| !task_id.is_empty())?;
        self.stores
            .tasks
            .load_task(task_id)
            .await
            .ok()
            .flatten()
            .and_then(|task| task.team_id)
    }

    async fn render_task_board(&self, team_id: &str) -> String {
        let pending = self
            .stores
            .tasks
            .list_tasks(&TaskQuery {
                team_id: Some(team_id.to_string()),
                status: Some(TaskStatus::Pending),
                limit: self.max_tasks,
                ..Default::default()
            })
            .await;
        let in_progress = self
            .stores
            .tasks
            .list_tasks(&TaskQuery {
                team_id: Some(team_id.to_string()),
                status: Some(TaskStatus::InProgress),
                limit: self.max_tasks,
                ..Default::default()
            })
            .await;

        let pending_tasks = pending.map(|p| p.items).unwrap_or_default();
        let active_tasks = in_progress.map(|p| p.items).unwrap_or_default();

        if pending_tasks.is_empty() && active_tasks.is_empty() {
            return String::new();
        }

        let mut out = String::new();
        out.push_str("<team_task_board>\n");

        if !pending_tasks.is_empty() {
            out.push_str("<pending_tasks>\n");
            for t in &pending_tasks {
                out.push_str(&format!(
                    "<task id=\"{}\" subject=\"{}\"",
                    t.task_id, t.subject
                ));
                if !t.blocked_by.is_empty() {
                    out.push_str(&format!(" blocked_by=\"{}\"", t.blocked_by.join(",")));
                }
                out.push_str("/>\n");
            }
            out.push_str("</pending_tasks>\n");
        }

        if !active_tasks.is_empty() {
            out.push_str("<active_tasks>\n");
            for t in &active_tasks {
                out.push_str(&format!(
                    "<task id=\"{}\" subject=\"{}\" owner=\"{}\"/>\n",
                    t.task_id,
                    t.subject,
                    t.owner.as_deref().unwrap_or("?")
                ));
            }
            out.push_str("</active_tasks>\n");
        }

        out.push_str("</team_task_board>\n");
        out.push_str("<team_tools_usage>\n");
        out.push_str(
            "List tasks: tool \"task_list\" with {\"team_id\":\"...\",\"status\":\"pending\"}.\n",
        );
        out.push_str("Create task: tool \"task_update\" with {\"action\":\"create\",\"subject\":\"...\",\"team_id\":\"...\"}.\n");
        out.push_str(
            "Claim task: tool \"task_update\" with {\"action\":\"claim\",\"task_id\":\"...\"}.\n",
        );
        out.push_str("Assign task: tool \"task_update\" with {\"action\":\"assign\",\"task_id\":\"...\",\"agent_id\":\"...\"}.\n");
        out.push_str("Complete task: tool \"task_update\" with {\"action\":\"complete\",\"task_id\":\"...\"}.\n");
        out.push_str("Send message: tool \"send_message\" with {\"recipient_thread_id\":\"...\",\"recipient_agent_id\":\"...\",\"message\":\"...\"}.\n");
        out.push_str("</team_tools_usage>");
        out
    }

    async fn render_task_board_reminder(&self, team_id: &str) -> Option<String> {
        let page = self
            .stores
            .tasks
            .list_tasks(&TaskQuery {
                team_id: Some(team_id.to_string()),
                status: Some(TaskStatus::InProgress),
                limit: 10,
                ..Default::default()
            })
            .await
            .ok()?;

        if page.items.is_empty() {
            return None;
        }

        let mut s = String::new();
        s.push_str("<active_tasks_reminder>\n");
        for t in &page.items {
            s.push_str(&format!(
                "<task id=\"{}\" subject=\"{}\" owner=\"{}\"/>\n",
                t.task_id,
                t.subject,
                t.owner.as_deref().unwrap_or("?")
            ));
        }
        s.push_str("</active_tasks_reminder>");
        Some(s)
    }
}

fn team_id_from_snapshot(snapshot: &Value) -> Option<String> {
    snapshot
        .get("team_id")
        .and_then(Value::as_str)
        .or_else(|| {
            snapshot
                .get("team")
                .and_then(|team| team.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|team_id| !team_id.is_empty())
        .map(ToOwned::to_owned)
}
