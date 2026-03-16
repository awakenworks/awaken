use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{SEND_MESSAGE_TOOL_ID, TASK_LIST_TOOL_ID, TASK_UPDATE_TOOL_ID};
use crate::contracts::runtime::tool_call::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolResult,
};
use crate::contracts::storage::{
    MailboxEntry, MailboxEntryOrigin, MailboxEntryStatus, MailboxStore, Task, TaskQuery,
    TaskStatus, TaskStore, TaskStoreError, TaskUpdate,
};

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn caller_agent_id(ctx: &ToolCallContext<'_>) -> Option<String> {
    ctx.caller_context()
        .agent_id()
        .or_else(|| ctx.run_identity().agent_id_opt())
        .map(ToOwned::to_owned)
}

// ────────────────────────────────────────────────────────────────────────────
// SendMessageTool
// ────────────────────────────────────────────────────────────────────────────

pub struct SendMessageTool {
    mailbox: Arc<dyn MailboxStore>,
}

impl SendMessageTool {
    pub fn new(mailbox: Arc<dyn MailboxStore>) -> Self {
        Self { mailbox }
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            SEND_MESSAGE_TOOL_ID,
            "Send Message",
            "Send a message to another agent's mailbox. The message will be delivered to the \
             recipient's thread and processed when the recipient is available.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "recipient_thread_id": {
                    "type": "string",
                    "description": "Thread ID of the recipient agent"
                },
                "recipient_agent_id": {
                    "type": "string",
                    "description": "Agent ID of the recipient"
                },
                "message": {
                    "type": "string",
                    "description": "The message content to send"
                },
                "kind": {
                    "type": "string",
                    "description": "Optional message type tag (e.g. 'task_assignment', 'status_update', 'chat')"
                },
                "priority": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 255,
                    "description": "Dispatch priority (0-255, higher = dispatched first). Default 0."
                }
            },
            "required": ["recipient_thread_id", "recipient_agent_id", "message"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let mailbox_id = args["recipient_thread_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let recipient_agent_id = args["recipient_agent_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let message_text = args["message"].as_str().unwrap_or_default().to_string();
        let kind = args["kind"].as_str().map(|s| s.to_string());
        let priority = args["priority"].as_u64().unwrap_or(0).min(255) as u8;

        if mailbox_id.is_empty() || recipient_agent_id.is_empty() || message_text.is_empty() {
            return Ok(ToolResult::error_with_code(
                SEND_MESSAGE_TOOL_ID,
                "invalid_args",
                "recipient_thread_id, recipient_agent_id, and message are required",
            ));
        }

        let sender_id = caller_agent_id(ctx)
            .or_else(|| ctx.caller_context().thread_id().map(ToOwned::to_owned));

        let now = now_unix_millis();
        let entry_id = uuid::Uuid::now_v7().simple().to_string();

        // Ensure mailbox state exists for the recipient.
        let mailbox_state = self
            .mailbox
            .ensure_mailbox_state(&mailbox_id, now)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let entry = MailboxEntry {
            entry_id: entry_id.clone(),
            mailbox_id: mailbox_id.clone(),
            origin: MailboxEntryOrigin::Internal,
            sender_id,
            payload: json!({
                "recipient_agent_id": recipient_agent_id,
                "message": message_text,
                "kind": kind,
            }),
            priority,
            dedupe_key: None,
            generation: mailbox_state.current_generation,
            status: MailboxEntryStatus::Queued,
            available_at: now,
            attempt_count: 0,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            created_at: now,
            updated_at: now,
        };

        self.mailbox
            .enqueue_mailbox_entry(&entry)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolResult::success(
            SEND_MESSAGE_TOOL_ID,
            json!({
                "entry_id": entry_id,
                "mailbox_id": mailbox_id,
                "recipient_agent_id": recipient_agent_id,
                "status": "queued"
            }),
        ))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// TaskListTool
// ────────────────────────────────────────────────────────────────────────────

pub struct TaskListTool {
    tasks: Arc<dyn TaskStore>,
}

impl TaskListTool {
    pub fn new(tasks: Arc<dyn TaskStore>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            TASK_LIST_TOOL_ID,
            "Task List",
            "List tasks on the shared task board. Can filter by team, owner, or status.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "team_id": { "type": "string", "description": "Filter by team ID" },
                "owner": { "type": "string", "description": "Filter by owner agent ID" },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "Filter by task status"
                },
                "offset": { "type": "integer", "minimum": 0, "description": "Pagination offset" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Max items (default 50)" }
            }
        }))
    }

    async fn execute(
        &self,
        args: Value,
        _ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let status = args["status"].as_str().and_then(|s| match s {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            _ => None,
        });

        let query = TaskQuery {
            team_id: args["team_id"].as_str().map(|s| s.to_string()),
            owner: args["owner"].as_str().map(|s| s.to_string()),
            status,
            offset: args["offset"].as_u64().unwrap_or(0) as usize,
            limit: args["limit"].as_u64().unwrap_or(50) as usize,
        };

        let page = self
            .tasks
            .list_tasks(&query)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        let items: Vec<Value> = page.items.iter().map(task_to_json).collect();

        Ok(ToolResult::success(
            TASK_LIST_TOOL_ID,
            json!({
                "items": items,
                "total": page.total,
                "has_more": page.has_more
            }),
        ))
    }
}

fn task_to_json(t: &Task) -> Value {
    json!({
        "task_id": t.task_id,
        "subject": t.subject,
        "description": t.description,
        "owner": t.owner,
        "status": t.status,
        "blocks": t.blocks,
        "blocked_by": t.blocked_by,
        "team_id": t.team_id,
        "created_by": t.created_by,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// TaskUpdateTool
// ────────────────────────────────────────────────────────────────────────────

pub struct TaskUpdateTool {
    tasks: Arc<dyn TaskStore>,
}

impl TaskUpdateTool {
    pub fn new(tasks: Arc<dyn TaskStore>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            TASK_UPDATE_TOOL_ID,
            "Task Update",
            "Create, update, claim, assign, complete, or delete a task on the shared task board.",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "claim", "assign", "complete", "delete"],
                    "description": "The operation to perform"
                },
                "task_id": { "type": "string", "description": "Task ID (required for all actions except create)" },
                "subject": { "type": "string", "description": "Task subject (required for create)" },
                "description": { "type": "string", "description": "Task description" },
                "agent_id": { "type": "string", "description": "Agent ID for claim/assign" },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "New status (for update)"
                },
                "team_id": { "type": "string", "description": "Team ID (for create)" },
                "blocks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task IDs this task blocks"
                },
                "blocked_by": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task IDs that block this task"
                }
            },
            "required": ["action"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let action = args["action"].as_str().unwrap_or_default();
        let now = now_unix_millis();

        match action {
            "create" => self.handle_create(&args, ctx, now).await,
            "update" => self.handle_update(&args, now).await,
            "claim" => self.handle_claim(&args, ctx, now).await,
            "assign" => self.handle_assign(&args, now).await,
            "complete" => self.handle_complete(&args, now).await,
            "delete" => self.handle_delete(&args).await,
            _ => Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_action",
                format!("unknown action: {action}"),
            )),
        }
    }
}

impl TaskUpdateTool {
    async fn handle_create(
        &self,
        args: &Value,
        ctx: &ToolCallContext<'_>,
        now: u64,
    ) -> Result<ToolResult, ToolError> {
        let subject = args["subject"].as_str().unwrap_or_default();
        if subject.is_empty() {
            return Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_args",
                "subject is required for create",
            ));
        }

        let task_id = args["task_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::now_v7().simple().to_string());

        let created_by = caller_agent_id(ctx);

        let task = Task {
            task_id: task_id.clone(),
            subject: subject.to_string(),
            description: args["description"].as_str().map(|s| s.to_string()),
            owner: None,
            status: TaskStatus::Pending,
            blocks: string_array(&args["blocks"]),
            blocked_by: string_array(&args["blocked_by"]),
            team_id: args["team_id"].as_str().map(|s| s.to_string()),
            metadata: None,
            created_at: now,
            updated_at: now,
            created_by,
        };

        self.tasks
            .create_task(&task)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolResult::success(
            TASK_UPDATE_TOOL_ID,
            json!({ "action": "created", "task": task_to_json(&task) }),
        ))
    }

    async fn handle_update(&self, args: &Value, now: u64) -> Result<ToolResult, ToolError> {
        let task_id = args["task_id"].as_str().unwrap_or_default();
        if task_id.is_empty() {
            return Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_args",
                "task_id is required for update",
            ));
        }

        let status = args["status"].as_str().and_then(|s| match s {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            _ => None,
        });

        let update = TaskUpdate {
            subject: args["subject"].as_str().map(|s| s.to_string()),
            description: args
                .get("description")
                .filter(|v| !v.is_null())
                .map(|v| v.as_str().map(|s| s.to_string())),
            owner: None,
            status,
            blocks: args
                .get("blocks")
                .filter(|v| v.is_array())
                .map(string_array),
            blocked_by: args
                .get("blocked_by")
                .filter(|v| v.is_array())
                .map(string_array),
            metadata: None,
        };

        let task = self
            .tasks
            .update_task(task_id, &update, now)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolResult::success(
            TASK_UPDATE_TOOL_ID,
            json!({ "action": "updated", "task": task_to_json(&task) }),
        ))
    }

    async fn handle_claim(
        &self,
        args: &Value,
        ctx: &ToolCallContext<'_>,
        now: u64,
    ) -> Result<ToolResult, ToolError> {
        let task_id = args["task_id"].as_str().unwrap_or_default();
        if task_id.is_empty() {
            return Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_args",
                "task_id is required for claim",
            ));
        }
        let agent_id = args["agent_id"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| caller_agent_id(ctx))
            .unwrap_or_default();
        if agent_id.is_empty() {
            return Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_args",
                "agent_id is required for claim",
            ));
        }

        match self.tasks.claim_task(task_id, &agent_id, now).await {
            Ok(task) => Ok(ToolResult::success(
                TASK_UPDATE_TOOL_ID,
                json!({ "action": "claimed", "task": task_to_json(&task) }),
            )),
            Err(TaskStoreError::ClaimConflict(_)) => Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "claim_conflict",
                "task is already owned by another agent",
            )),
            Err(TaskStoreError::Blocked(_)) => Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "blocked",
                "task is blocked by unfinished dependencies",
            )),
            Err(e) => Err(ToolError::ExecutionFailed(e.to_string())),
        }
    }

    async fn handle_assign(&self, args: &Value, now: u64) -> Result<ToolResult, ToolError> {
        let task_id = args["task_id"].as_str().unwrap_or_default();
        let agent_id = args["agent_id"].as_str().unwrap_or_default();
        if task_id.is_empty() || agent_id.is_empty() {
            return Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_args",
                "task_id and agent_id are required for assign",
            ));
        }

        let task = self
            .tasks
            .assign_task(task_id, agent_id, now)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolResult::success(
            TASK_UPDATE_TOOL_ID,
            json!({ "action": "assigned", "task": task_to_json(&task) }),
        ))
    }

    async fn handle_complete(&self, args: &Value, now: u64) -> Result<ToolResult, ToolError> {
        let task_id = args["task_id"].as_str().unwrap_or_default();
        if task_id.is_empty() {
            return Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_args",
                "task_id is required for complete",
            ));
        }

        let task = self
            .tasks
            .complete_task(task_id, now)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolResult::success(
            TASK_UPDATE_TOOL_ID,
            json!({ "action": "completed", "task": task_to_json(&task) }),
        ))
    }

    async fn handle_delete(&self, args: &Value) -> Result<ToolResult, ToolError> {
        let task_id = args["task_id"].as_str().unwrap_or_default();
        if task_id.is_empty() {
            return Ok(ToolResult::error_with_code(
                TASK_UPDATE_TOOL_ID,
                "invalid_args",
                "task_id is required for delete",
            ));
        }

        self.tasks
            .delete_task(task_id)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        Ok(ToolResult::success(
            TASK_UPDATE_TOOL_ID,
            json!({ "action": "deleted", "task_id": task_id }),
        ))
    }
}

fn string_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
