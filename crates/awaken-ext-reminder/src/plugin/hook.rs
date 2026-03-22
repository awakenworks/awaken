use std::sync::Arc;

use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::tool_pattern::pattern_matches;
use awaken_runtime::AddContextMessage;
use awaken_runtime::runtime::{PhaseContext, PhaseHook};
use awaken_runtime::state::StateCommand;

use crate::output_matcher::output_matches;
use crate::rule::ReminderRule;

pub(crate) struct ReminderHook {
    pub(crate) rules: Arc<[ReminderRule]>,
}

#[async_trait]
impl PhaseHook for ReminderHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();

        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(cmd),
        };
        let tool_args = ctx.tool_args.clone().unwrap_or_default();
        let tool_result = match &ctx.tool_result {
            Some(result) => result,
            None => return Ok(cmd),
        };

        for rule in self.rules.iter() {
            // 1. Match tool name + args
            if !pattern_matches(&rule.pattern, tool_name, &tool_args).is_match() {
                continue;
            }

            // 2. Match tool result
            if !output_matches(&rule.output, tool_result) {
                continue;
            }

            // 3. Schedule context message injection
            tracing::debug!(
                rule = %rule.name,
                tool = %tool_name,
                "reminder rule matched, scheduling context message"
            );
            cmd.schedule_action::<AddContextMessage>(rule.message.clone())?;
        }

        Ok(cmd)
    }
}
