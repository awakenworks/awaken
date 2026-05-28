use std::collections::BTreeMap;
use std::time::Duration;

use awaken_protocol_a2a::{PushNotificationConfig, Task, TaskState};
use awaken_runtime::RunActivation;
use awaken_server_contract::contract::mailbox::RunDispatch;
use awaken_server_contract::contract::storage::RunRecord;
use serde::{Deserialize, Serialize};

pub(super) const A2A_VERSION: &str = "1.0";
pub(super) const DEFAULT_PAGE_SIZE: usize = 50;
pub(super) const MAX_PAGE_SIZE: usize = 100;
pub(super) const DISCOVERY_PATH: &str = "/.well-known/agent-card.json";
pub(super) const INTERFACE_BASE_PATH: &str = "/v1/a2a";
pub(super) const BLOCKING_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
pub(super) const BLOCKING_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const SUPPORTED_OUTPUT_MODE: &str = "text/plain";
pub(super) const PUSH_CONFIGS_METADATA_KEY: &str = "a2a.pushNotificationConfigs";
pub(super) const TASK_BINDINGS_METADATA_KEY: &str = "a2a.taskBindings";
pub(super) const A2A_NOTIFICATION_TOKEN_HEADER: &str = "x-a2a-notification-token";
pub(super) const EXTENDED_CARD_SECURITY_SCHEME_ID: &str = "awakenExtendedCardBearer";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct StoredTaskBindings {
    #[serde(default)]
    pub(super) tasks: BTreeMap<String, StoredTaskBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredTaskBinding {
    pub(super) thread_id: String,
    #[serde(default)]
    pub(super) start_message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) end_message_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct StoredPushConfigs {
    #[serde(default)]
    pub(super) tasks: BTreeMap<String, Vec<PushNotificationConfig>>,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedTask {
    pub(super) thread_id: String,
    pub(super) run: Option<RunRecord>,
    pub(super) dispatch: Option<RunDispatch>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetTaskQuery {
    pub(super) history_length: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListTasksQuery {
    pub(super) context_id: Option<String>,
    pub(super) status: Option<String>,
    pub(super) history_length: Option<usize>,
    pub(super) page_size: Option<usize>,
    pub(super) page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListPushConfigsQuery {
    pub(super) page_size: Option<usize>,
    pub(super) page_token: Option<String>,
}

#[derive(Debug)]
pub(super) struct TaskSnapshot {
    pub(super) task: Task,
    pub(super) updated_at_ms: u64,
    pub(super) current_agent_id: Option<String>,
}

#[derive(Debug)]
pub(super) struct TaskSource {
    pub(super) state: TaskState,
    pub(super) updated_at_ms: u64,
    pub(super) current_agent_id: Option<String>,
}

pub(super) struct PreparedRequest {
    pub(super) task_id: String,
    pub(super) thread_id: String,
    pub(super) effective_tenant: Option<String>,
    pub(super) history_length: usize,
    pub(super) return_immediately: bool,
    pub(super) push_notification_config: Option<PushNotificationConfig>,
    pub(super) new_task_start_message_id: Option<String>,
    pub(super) request: RunActivation,
}
