use async_trait::async_trait;
use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::model::Phase;
use awaken_contract::state::StateCommand;

use crate::agent::state::AddContextMessage;
use crate::hooks::{PhaseContext, PhaseHook};
use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};

pub const KNOWLEDGE_CUTOFF_PLUGIN_ID: &str = "knowledge_cutoff_context";

const KNOWLEDGE_CUTOFF_CONTEXT_KEY: &str = "knowledge_cutoff.context";

pub struct KnowledgeCutoffPlugin {
    cutoff: String,
    today: Option<String>,
}

impl KnowledgeCutoffPlugin {
    pub fn new(cutoff: impl Into<String>) -> Self {
        Self {
            cutoff: cutoff.into(),
            today: None,
        }
    }

    #[cfg(test)]
    fn with_today(cutoff: impl Into<String>, today: impl Into<String>) -> Self {
        Self {
            cutoff: cutoff.into(),
            today: Some(today.into()),
        }
    }
}

impl Plugin for KnowledgeCutoffPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: KNOWLEDGE_CUTOFF_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            KNOWLEDGE_CUTOFF_PLUGIN_ID,
            Phase::BeforeInference,
            KnowledgeCutoffHook {
                cutoff: self.cutoff.clone(),
                today: self.today.clone(),
            },
        )?;
        Ok(())
    }
}

struct KnowledgeCutoffHook {
    cutoff: String,
    today: Option<String>,
}

#[async_trait]
impl PhaseHook for KnowledgeCutoffHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let today = self.today.clone().unwrap_or_else(today_utc_date);
        let text = knowledge_cutoff_message(&today, &self.cutoff);
        let mut cmd = StateCommand::new();
        cmd.schedule_action::<AddContextMessage>(
            ContextMessage::system(KNOWLEDGE_CUTOFF_CONTEXT_KEY, text).with_priority(-100),
        )?;
        Ok(cmd)
    }
}

fn knowledge_cutoff_message(today: &str, cutoff: &str) -> String {
    format!("Today is {today}. The model's training cutoff is {cutoff}.")
}

fn today_utc_date() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    let (year, month, day) = civil_from_unix_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_unix_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_contract::model::Phase;

    use crate::agent::state::{ContextMessageStore, RunLifecycle, RunLifecycleUpdate};
    use crate::loop_runner::{LoopActionHandlersPlugin, LoopStatePlugin};
    use crate::phase::{ExecutionEnv, PhaseRuntime};
    use crate::plugins::Plugin;
    use crate::state::{MutationBatch, StateStore};

    use super::*;

    #[test]
    fn civil_from_unix_days_formats_known_dates() {
        assert_eq!(civil_from_unix_days(0), (1970, 1, 1));
        assert_eq!(civil_from_unix_days(20_000), (2024, 10, 4));
    }

    #[test]
    fn knowledge_cutoff_plugin_descriptor_name() {
        let plugin = KnowledgeCutoffPlugin::new("2025-01");
        assert_eq!(plugin.descriptor().name, KNOWLEDGE_CUTOFF_PLUGIN_ID);
    }

    #[tokio::test]
    async fn knowledge_cutoff_plugin_injects_system_context_message() {
        let store = StateStore::new();
        store
            .install_plugin(LoopStatePlugin)
            .expect("install loop state");

        let mut batch = MutationBatch::new();
        batch.update::<RunLifecycle>(RunLifecycleUpdate::Start {
            run_id: "run".into(),
            updated_at: 0,
        });
        store.commit(batch).expect("init lifecycle");

        let runtime = PhaseRuntime::new(store).expect("runtime");
        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(LoopActionHandlersPlugin),
            Arc::new(KnowledgeCutoffPlugin::with_today("2025-01", "2026-05-26")),
        ];
        let env = ExecutionEnv::from_plugins(&plugins, &Default::default()).expect("env");

        runtime
            .run_phase_with_context(
                &env,
                PhaseContext::new(Phase::BeforeInference, runtime.store().snapshot()),
            )
            .await
            .expect("phase");

        let messages = runtime
            .store()
            .read::<ContextMessageStore>()
            .expect("context store")
            .sorted_messages()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].key, KNOWLEDGE_CUTOFF_CONTEXT_KEY);
        let awaken_contract::contract::content::ContentBlock::Text { text } =
            &messages[0].content[0]
        else {
            panic!("expected text content block");
        };
        assert_eq!(
            text,
            "Today is 2026-05-26. The model's training cutoff is 2025-01."
        );
    }
}
