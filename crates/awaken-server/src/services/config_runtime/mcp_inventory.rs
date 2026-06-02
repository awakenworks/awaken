use std::sync::Arc;

use awaken_ext_mcp::{McpPromptEntry, McpResourceEntry, McpServerStatusSnapshot};

use super::{ConfigRuntimeError, ConfigRuntimeManager};

pub struct McpServerInventory {
    pub status: McpServerStatusSnapshot,
    pub prompts: Vec<McpPromptEntry>,
    pub resources: Vec<McpResourceEntry>,
}

impl ConfigRuntimeManager {
    pub async fn mcp_server_inventory(
        &self,
        server_name: &str,
    ) -> Result<Option<McpServerInventory>, ConfigRuntimeError> {
        let handle = {
            let guard = self.active_mcp_registry.lock();
            guard.as_ref().map(|active| Arc::clone(&active.handle))
        };
        let Some(handle) = handle else {
            return Ok(None);
        };
        let Some(status) = handle.server_status(server_name).await else {
            return Ok(None);
        };
        let prompts = handle.server_prompts(server_name).await?;
        let resources = handle.server_resources(server_name).await?;
        Ok(Some(McpServerInventory {
            status,
            prompts,
            resources,
        }))
    }
}
