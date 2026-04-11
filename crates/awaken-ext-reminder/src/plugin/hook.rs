use std::sync::Arc;

use async_trait::async_trait;

use awaken_contract::{PluginConfigKey, StateError};
use awaken_runtime::agent::state::AddContextMessage;
use awaken_runtime::phase::{PhaseContext, PhaseHook};
use awaken_runtime::state::StateCommand;
use awaken_tool_pattern::pattern_matches;

use crate::config::{ReminderConfigKey, ReminderRulesConfig};
use crate::output_matcher::output_matches;
use crate::rule::ReminderRule;

pub(crate) struct ReminderHook {
    pub(crate) rules: Arc<[ReminderRule]>,
}

pub(crate) fn rules_from_config(
    config: ReminderRulesConfig,
) -> Result<Vec<ReminderRule>, StateError> {
    config.into_rules().map_err(|e| StateError::KeyDecode {
        key: ReminderConfigKey::KEY.into(),
        message: e.to_string(),
    })
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

        let configured_rules = rules_from_config(ctx.config::<ReminderConfigKey>()?)?;

        for rule in self.rules.iter().chain(configured_rules.iter()) {
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
