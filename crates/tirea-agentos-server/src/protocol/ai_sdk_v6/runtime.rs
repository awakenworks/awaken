//! Runtime wiring for AI SDK requests.
//!
//! Applies AI SDK-specific extensions to a [`ResolvedRun`], currently
//! no additional plugins.

use std::sync::Arc;
use tirea_agentos::runtime::loop_runner::{ParallelToolExecutor, ResolvedRun};

use tirea_protocol_ai_sdk_v6::AiSdkV6RunRequest;

/// Apply AI SDK-specific extensions to a [`ResolvedRun`].
pub fn apply_ai_sdk_extensions(resolved: &mut ResolvedRun, _request: &AiSdkV6RunRequest) {
    // AI SDK transport supports batched approvals; replay only after the full
    // suspended set receives decisions to avoid partial duplicate replays.
    resolved.agent.tool_executor = Arc::new(ParallelToolExecutor::batch_approval());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tirea_agentos::runtime::loop_runner::BaseAgent;
    use tirea_contract::RunConfig;

    fn empty_resolved() -> ResolvedRun {
        ResolvedRun {
            agent: BaseAgent::default(),
            tools: HashMap::new(),
            run_config: RunConfig::new(),
        }
    }

    #[test]
    fn apply_extensions_is_noop_without_decisions() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t1",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .expect("request should deserialize");
        let mut resolved = empty_resolved();
        apply_ai_sdk_extensions(&mut resolved, &req);
        assert!(resolved.agent.plugins.is_empty());
        assert_eq!(
            resolved.agent.tool_executor.name(),
            "parallel_batch_approval"
        );
    }

    #[test]
    fn apply_extensions_is_noop_with_decisions() {
        let req: AiSdkV6RunRequest = serde_json::from_value(json!({
            "id": "t2",
            "messages": [{
                "role": "assistant",
                "parts": [{
                    "type": "tool-approval-response",
                    "approvalId": "fc_1",
                    "approved": true
                }]
            }]
        }))
        .expect("request should deserialize");
        let mut resolved = empty_resolved();
        apply_ai_sdk_extensions(&mut resolved, &req);
        assert!(resolved.agent.plugins.is_empty());
        assert_eq!(
            resolved.agent.tool_executor.name(),
            "parallel_batch_approval"
        );
    }
}
